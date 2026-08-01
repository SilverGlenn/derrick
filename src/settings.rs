//! Persistent settings (config file in %APPDATA%) and the Windows autostart
//! registry key.
//!
//! Config lives at `%APPDATA%\Derrick\config.toml`. Environment
//! variables (SERGEANT_WORK_MINUTES, SERGEANT_BREAK_MINUTES, --camera=N)
//! override the config for testing, but the config file is the real source
//! of truth for day-to-day use.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Settings {
    /// Length of one work block, in minutes.
    pub work_minutes: f64,
    /// Required standing time per break, in minutes.
    pub break_minutes: f64,
    /// Webcam index (takes effect on next start).
    pub camera_index: u32,
    /// Launch the app automatically when Windows starts.
    pub autostart: bool,
    /// The X/close button hides to the tray (true) instead of quitting (false).
    pub close_to_tray: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            work_minutes: 30.0,
            break_minutes: 5.0,
            camera_index: 0,
            autostart: false,
            close_to_tray: true,
        }
    }
}

impl Settings {
    pub fn sanitized(mut self) -> Self {
        self.work_minutes = self.work_minutes.clamp(1.0, 480.0);
        self.break_minutes = self.break_minutes.clamp(0.1, 120.0);
        self
    }
}

pub fn config_path() -> PathBuf {
    // SERGEANT_CONFIG_DIR overrides the location (used by tests).
    if let Some(dir) = std::env::var_os("SERGEANT_CONFIG_DIR") {
        return PathBuf::from(dir).join("config.toml");
    }
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("Derrick").join("config.toml")
}

/// Load settings, falling back to defaults when the file is missing or broken.
/// Migrates the old `StandUpSergeant` config directory if present.
pub fn load() -> Settings {
    let path = config_path();
    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(_) => {
            // One-time migration from the pre-rename config location.
            let old_dir = std::env::var_os("APPDATA")
                .map(PathBuf::from)
                .unwrap_or_else(std::env::temp_dir)
                .join("StandUpSergeant");
            let old = old_dir.join("config.toml");
            if let Ok(old_raw) = fs::read_to_string(&old) {
                if let Ok(settings) = toml::from_str::<Settings>(&old_raw) {
                    let settings = settings.sanitized();
                    let _ = save(&settings);
                    // One-time migration: the old folder is no longer needed.
                    let _ = fs::remove_dir_all(&old_dir);
                    return settings;
                }
            }
            return Settings::default();
        }
    };
    match toml::from_str::<Settings>(&raw) {
        Ok(settings) => settings.sanitized(),
        Err(err) => {
            log::warn!("config at {} is invalid ({err}) — using defaults", path.display());
            Settings::default()
        }
    }
}

/// Persist settings to the config file.
pub fn save(settings: &Settings) -> Result<()> {
    let path = config_path();
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)
            .with_context(|| format!("failed to create config dir {}", dir.display()))?;
    }
    let raw = toml::to_string_pretty(settings).context("failed to serialize config")?;
    fs::write(&path, raw).with_context(|| format!("failed to write config {}", path.display()))?;
    log::info!("saved config to {}", path.display());
    Ok(())
}

// ---------------------------------------------------------------------------
// Autostart (HKCU\Software\Microsoft\Windows\CurrentVersion\Run)
// ---------------------------------------------------------------------------

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "Derrick";

/// Is the app registered to start with Windows?
pub fn autostart_enabled() -> bool {
    use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, RegGetValueW, RRF_RT_REG_SZ};
    let Ok(key) = wide(RUN_KEY) else { return false };
    let Ok(value) = wide(RUN_VALUE) else { return false };
    let mut data = [0u16; 2048];
    let mut size = (data.len() * 2) as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            data.as_mut_ptr() as *mut _,
            &mut size,
        )
    };
    status == 0
}

/// Register (or unregister) the app in the Windows Run key.
pub fn set_autostart(enabled: bool) -> Result<()> {
    use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, REG_SZ, RegSetKeyValueW};
    let key = wide(RUN_KEY)?;
    let value = wide(RUN_VALUE)?;
    if enabled {
        let exe = std::env::current_exe().context("failed to resolve exe path")?;
        let cmd = format!("\"{}\"", exe.display());
        let cmd = wide(&cmd)?;
        let status = unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                value.as_ptr(),
                REG_SZ,
                cmd.as_ptr() as *const _,
                (cmd.len() * 2) as u32,
            )
        };
        if status != 0 {
            return Err(anyhow!("failed to write Run key: error {status}"));
        }
        log::info!("autostart enabled: {cmd:?}");
    } else {
        // RegDeleteValueW operates on a key HANDLE, so open the Run key first.
        let mut run_key: *mut core::ffi::c_void = std::ptr::null_mut();
        let status = unsafe {
            windows_sys::Win32::System::Registry::RegOpenKeyExW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                0,
                windows_sys::Win32::System::Registry::KEY_SET_VALUE,
                &mut run_key,
            )
        };
        if status == 0 {
            let status = unsafe { windows_sys::Win32::System::Registry::RegDeleteValueW(run_key, value.as_ptr()) };
            unsafe {
                windows_sys::Win32::System::Registry::RegCloseKey(run_key);
            }
            // ERROR_FILE_NOT_FOUND (2) just means it wasn't registered.
            if status != 0 && status != 2 {
                return Err(anyhow!("failed to delete Run key: error {status}"));
            }
        } else {
            return Err(anyhow!("failed to open Run key: error {status}"));
        }
        log::info!("autostart disabled");
    }
    Ok(())
}

fn wide(s: &str) -> Result<Vec<u16>> {
    if s.contains('\0') {
        return Err(anyhow!("string contains NUL"));
    }
    Ok(s.encode_utf16().chain(std::iter::once(0)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    /// Both autostart tests touch the same real registry key — serialize them.
    fn autostart_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn config_roundtrip() {
        let settings = Settings {
            work_minutes: 45.0,
            break_minutes: 7.5,
            camera_index: 1,
            autostart: true,
            close_to_tray: false,
        };
        let raw = toml::to_string_pretty(&settings).unwrap();
        let back: Settings = toml::from_str(&raw).unwrap();
        assert_eq!(settings, back);
        assert!(raw.contains("work_minutes = 45.0"));
    }

    #[test]
    fn autostart_toggle() {
        let _guard = autostart_lock().lock().unwrap();
        // Reads/writes the real HKCU Run key — set, verify, clear.
        set_autostart(true).unwrap();
        assert!(autostart_enabled());
        set_autostart(false).unwrap();
        assert!(!autostart_enabled());
    }

    #[test]
    fn autostart_clear_when_absent_is_ok() {
        let _guard = autostart_lock().lock().unwrap();
        set_autostart(false).unwrap(); // must not error when not registered
        assert!(!autostart_enabled());
    }
}

#[cfg(test)]
mod e2e {
    use super::*;

    #[test]
    fn loads_config_from_disk() {
        // Hermetic: point the config dir at a temp folder we control.
        let dir = std::env::temp_dir().join(format!("sas-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("config.toml"),
            "work_minutes = 0.05\nbreak_minutes = 7.5\ncamera_index = 2\nautostart = true\n",
        )
        .unwrap();
        std::env::set_var("SERGEANT_CONFIG_DIR", &dir);
        let s = load();
        // 0.05 is below the work floor, so it clamps to 1.0; the rest passes.
        assert_eq!(s.work_minutes, 1.0);
        assert_eq!(s.break_minutes, 7.5);
        assert_eq!(s.camera_index, 2);
        assert!(s.autostart);
        std::env::remove_var("SERGEANT_CONFIG_DIR");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
