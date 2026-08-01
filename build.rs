fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/app.ico");
        // Numeric version block (FILEVERSION / PRODUCTVERSION) — WiX binds
        // the MSI product version to this.
        let parts: Vec<u64> = env!("CARGO_PKG_VERSION")
            .split('.')
            .filter_map(|s| s.parse().ok())
            .collect();
        let (maj, min, build, rev) = (
            parts.first().copied().unwrap_or(0),
            parts.get(1).copied().unwrap_or(0),
            parts.get(2).copied().unwrap_or(0),
            parts.get(3).copied().unwrap_or(0),
        );
        let packed = (maj << 48) | (min << 32) | (build << 16) | rev;
        res.set_version_info(winresource::VersionInfo::FILEVERSION, packed);
        res.set_version_info(winresource::VersionInfo::PRODUCTVERSION, packed);
        res.compile().expect("failed to embed app icon");
    }
}
