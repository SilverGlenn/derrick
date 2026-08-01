//! Update checking and self-update: talks to the GitHub Releases API,
//! downloads the MSI, and installs it silently with a relaunch.
//!
//! The worker runs on a plain thread and reports through `UpdateStatus`,
//! which the About window polls on its own tick.
//!
//! HTTP goes through curl.exe (ships with Windows 10 1803+): derrick.exe's
//! own sockets are blocked by some AV products, and curl is whitelisted.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// What the update machinery is doing right now.
#[derive(Clone, Debug, PartialEq)]
pub enum Phase {
    Idle,
    Checking,
    /// Latest release info: Ok(None) = up to date.
    Checked(Result<Option<UpdateInfo>, String>),
    Downloading { done: u64, total: u64 },
    Downloaded,
    Installing,
}

#[derive(Clone, Debug, PartialEq)]
pub struct UpdateInfo {
    pub version: String,
    pub asset_name: String,
    pub url: String,
    pub size: u64,
}

pub struct UpdateStatus {
    pub phase: Mutex<Phase>,
    /// The downloaded MSI, set when the download finishes.
    pub download_dest: Mutex<Option<PathBuf>>,
}

impl Default for UpdateStatus {
    fn default() -> Self {
        Self {
            phase: Mutex::new(Phase::Idle),
            download_dest: Mutex::new(None),
        }
    }
}

pub fn shared() -> Arc<UpdateStatus> {
    Arc::new(UpdateStatus::default())
}

const API_URL: &str = "https://api.github.com/repos/SilverGlenn/derrick/releases/latest";
const USER_AGENT: &str = concat!("Derrick/", env!("CARGO_PKG_VERSION"));

/// Blocking call — run on a worker thread. Fetches the latest release's MSI
/// asset.
pub fn fetch_latest() -> Result<UpdateInfo, String> {
    let output = std::process::Command::new("curl")
        .args([
            "-sS",
            "--max-time",
            "15",
            "-A",
            USER_AGENT,
            "-H",
            "Accept: application/vnd.github+json",
            API_URL,
        ])
        .output()
        .map_err(|err| format!("couldn't run curl ({err})"))?;
    if !output.status.success() {
        let msg = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "couldn't reach GitHub ({})",
            msg.trim().lines().next().unwrap_or("curl failed")
        ));
    }
    let body: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|err| format!("bad response from GitHub ({err})"))?;

    let tag = body
        .get("tag_name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "bad response from GitHub (no tag)".to_string())?;
    let asset = body
        .get("assets")
        .and_then(|a| a.as_array())
        .and_then(|assets| {
            assets.iter().find(|a| {
                a.get("name")
                    .and_then(|n| n.as_str())
                    .map(|n| n.ends_with(".msi"))
                    .unwrap_or(false)
            })
        })
        .ok_or_else(|| "latest release has no MSI".to_string())?;

    let version = tag.trim_start_matches('v').to_string();
    let info = UpdateInfo {
        version: version.clone(),
        asset_name: asset
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("Derrick.msi")
            .to_string(),
        url: asset
            .get("browser_download_url")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        size: asset.get("size").and_then(|v| v.as_u64()).unwrap_or(0),
    };

    Ok(info)
}

/// Blocking call — run on a worker thread. Returns the latest release's MSI
/// asset, or Ok(None) if it is not newer than the running version.
pub fn check_for_update() -> Result<Option<UpdateInfo>, String> {
    let info = fetch_latest()?;
    Ok(is_newer(&info.version, env!("CARGO_PKG_VERSION")).then_some(info))
}

/// Simple x.y.z comparison. Returns true when `candidate` is newer than
/// `current`. Handles missing segments ("1.2" == "1.2.0") and 'v' prefixes.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    parse_version(candidate) > parse_version(current)
}

fn parse_version(v: &str) -> (u32, u32, u32) {
    let mut parts = v.trim_start_matches('v').split('.');
    let get = |p: Option<&str>| p.and_then(|s| s.parse().ok()).unwrap_or(0);
    (
        get(parts.next()),
        get(parts.next()),
        get(parts.next()),
    )
}

/// Record a failure in the shared status and return the message.
fn fail(status: &UpdateStatus, msg: String) -> String {
    *status.phase.lock().unwrap() = Phase::Checked(Err(msg.clone()));
    msg
}

/// Blocking call — run on a worker thread. Downloads the asset to the
/// updates folder, reporting progress through `status` as it goes.
pub fn download(info: &UpdateInfo, status: &UpdateStatus) -> Result<PathBuf, String> {
    *status.phase.lock().unwrap() = Phase::Downloading { done: 0, total: 0 };

    let dir = updates_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return Err(fail(status, format!("can't create updates folder ({e})")));
    }
    let dest = dir.join(&info.asset_name);

    let mut child = match std::process::Command::new("curl")
        .args(["-sSL", "--max-time", "300", "-A", USER_AGENT])
        .arg(&info.url)
        .stdout(std::process::Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(err) => return Err(fail(status, format!("couldn't run curl ({err})"))),
    };

    let size = info.size.max(1);
    let mut reader = child.stdout.take().expect("curl stdout piped");
    let mut file = match std::fs::File::create(&dest) {
        Ok(file) => file,
        Err(e) => return Err(fail(status, format!("can't write file ({e})"))),
    };
    let mut buffer = [0u8; 64 * 1024];
    let mut written: u64 = 0;
    loop {
        let n = match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => return Err(fail(status, format!("download interrupted ({e})"))),
        };
        if let Err(e) = file.write_all(&buffer[..n]) {
            return Err(fail(status, format!("can't write file ({e})")));
        }
        written += n as u64;
        *status.phase.lock().unwrap() = Phase::Downloading {
            done: written,
            total: size,
        };
    }

    match child.wait() {
        Ok(exit) if exit.success() => {}
        Ok(exit) => {
            let _ = std::fs::remove_file(&dest);
            return Err(fail(
                status,
                format!("download failed (curl exited {exit})"),
            ));
        }
        Err(err) => {
            let _ = std::fs::remove_file(&dest);
            return Err(fail(status, format!("download interrupted ({err})")));
        }
    }

    *status.phase.lock().unwrap() = Phase::Downloaded;
    *status.download_dest.lock().unwrap() = Some(dest.clone());
    Ok(dest)
}

/// Where downloads land: %LOCALAPPDATA%\Derrick\updates.
pub fn updates_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("Derrick").join("updates")
}

/// The installed exe: %LOCALAPPDATA%\Derrick\derrick.exe.
pub fn installed_exe() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("Derrick").join("derrick.exe")
}

/// Write the updater script and launch it detached. It waits for this
/// process to exit, silently installs the MSI, then relaunches Derrick.
/// Caller should quit the app right after.
pub fn launch_updater(msi: &std::path::Path) -> Result<(), String> {
    let dir = updates_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("can't create updates folder ({e})"))?;
    let script = dir.join("updater.ps1");
    let msi_quoted = msi.display().to_string().replace('\'', "''");
    let exe_quoted = installed_exe().display().to_string().replace('\'', "''");
    let ps = format!(
        "Start-Sleep -Seconds 3\n\
         Stop-Process -Name derrick -Force -ErrorAction SilentlyContinue\n\
         Start-Sleep -Milliseconds 500\n\
         Start-Process msiexec -ArgumentList '/i ''{msi_quoted}'' /qn /norestart' -Wait\n\
         Remove-Item '{msi_quoted}' -Force -ErrorAction SilentlyContinue\n\
         Start-Process '{exe_quoted}'\n\
         Remove-Item $MyInvocation.MyCommand.Path -Force -ErrorAction SilentlyContinue\n"
    );
    std::fs::write(&script, ps).map_err(|e| format!("can't write updater ({e})"))?;
    std::process::Command::new("powershell")
        .args([
            "-WindowStyle",
            "Hidden",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
            script.to_str().unwrap_or_default(),
        ])
        .spawn()
        .map_err(|e| format!("can't launch updater ({e})"))?;
    Ok(())
}

#[cfg(test)]
mod update_tests {
    use super::is_newer;

    #[test]
    fn version_compare() {
        assert!(is_newer("0.2.0", "0.1.0"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(is_newer("0.1.1", "0.1.0"));
        assert!(is_newer("v0.2.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
        assert!(!is_newer("0.1", "0.1.0"));
        assert!(!is_newer("0.1.0-beta", "0.1.0"));
    }
}
