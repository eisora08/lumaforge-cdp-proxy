use std::path::PathBuf;

/// Cross-platform path abstraction for LumaForge directories.
/// Replaces hardcoded `LOCALAPPDATA` references.

fn home_dir() -> Option<PathBuf> {
    std::env::var("HOME")
        .ok()
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir())
}

// ── Steam detection ──────────────────────────────────────────────────────────

/// Find the Steam installation directory.
#[cfg(target_os = "linux")]
pub fn find_steam_install() -> Option<PathBuf> {
    let home = home_dir()?;
    let candidates = [
        home.join(".steam").join("steam"),
        home.join(".steam").join("debian-installation"),
        home.join(".local").join("share").join("Steam"),
        home.join(".var")
            .join("app")
            .join("com.valvesoftware.Steam")
            .join("data")
            .join("Steam"),
    ];
    for candidate in &candidates {
        if candidate.exists() {
            return Some(candidate.clone());
        }
    }
    // Fallback: check if any candidate has steam.sh or config/loginusers.vdf
    for candidate in &candidates {
        if candidate.join("steam.sh").exists()
            || candidate.join("config").join("loginusers.vdf").exists()
        {
            return Some(candidate.clone());
        }
    }
    None
}

#[cfg(target_os = "windows")]
pub fn find_steam_install() -> Option<PathBuf> {
    // Windows: check registry and common paths
    let candidates = [
        "C:\\Program Files (x86)\\Steam",
        "C:\\Program Files\\Steam",
    ];
    for c in &candidates {
        let p = PathBuf::from(c);
        if p.join("steam.exe").exists() {
            return Some(p);
        }
    }
    if let Ok(local_appdata) = std::env::var("LOCALAPPDATA") {
        let p = PathBuf::from(&local_appdata).join("Steam");
        if p.join("steam.exe").exists() {
            return Some(p);
        }
    }
    None
}

// ── Data directories ─────────────────────────────────────────────────────────

/// Local data directory (equivalent to `%LOCALAPPDATA%` on Windows).
/// Linux: `~/.local/share/LumaForge`
#[cfg(target_os = "linux")]
pub fn local_data_dir() -> PathBuf {
    home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".local")
        .join("share")
        .join("LumaForge")
}

#[cfg(target_os = "windows")]
pub fn local_data_dir() -> PathBuf {
    PathBuf::from(
        std::env::var("LOCALAPPDATA").unwrap_or_else(|_| "C:\\Windows\\Temp".to_string()),
    )
    .join("LumaForge")
}

/// Config directory.
/// Linux: `~/.config/LumaForge`
#[cfg(target_os = "linux")]
pub fn config_dir() -> PathBuf {
    home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".config")
        .join("LumaForge")
}

#[cfg(target_os = "windows")]
pub fn config_dir() -> PathBuf {
    local_data_dir()
}

/// Runtime directory for discovery files, theme manifests, etc.
pub fn runtime_dir() -> PathBuf {
    local_data_dir().join("runtime")
}

/// Plugins directory.
pub fn plugins_dir() -> PathBuf {
    local_data_dir().join("plugins")
}

/// Themes directory.
pub fn themes_dir() -> PathBuf {
    local_data_dir().join("themes")
}

/// Temp directory.
#[cfg(target_os = "linux")]
pub fn temp_dir() -> PathBuf {
    PathBuf::from("/tmp")
}

#[cfg(target_os = "windows")]
pub fn temp_dir() -> PathBuf {
    PathBuf::from(
        std::env::var("TEMP").unwrap_or_else(|_| "C:\\Windows\\Temp".to_string()),
    )
}

/// Log file path for the CDP proxy.
pub fn log_file_path() -> PathBuf {
    temp_dir().join("steamcdp_proxy.log")
}

/// Config.json path (used by Lua backends).
pub fn config_json_path() -> PathBuf {
    config_dir().join("config.json")
}

/// Path separator for the current platform.
#[cfg(target_os = "linux")]
pub const SEP: char = '/';

#[cfg(target_os = "windows")]
pub const SEP: char = '\\';

/// Join path segments using the platform separator.
pub fn path_join(base: &std::path::Path, segments: &[&str]) -> PathBuf {
    let mut p = base.to_path_buf();
    for seg in segments {
        p.push(seg);
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_local_data_dir_not_empty() {
        let dir = local_data_dir();
        assert!(dir.to_string_lossy().contains("LumaForge"));
    }

    #[test]
    fn test_runtime_dir() {
        let dir = runtime_dir();
        assert!(dir.to_string_lossy().contains("runtime"));
    }

    #[test]
    fn test_plugins_dir() {
        let dir = plugins_dir();
        assert!(dir.to_string_lossy().contains("plugins"));
    }

    #[test]
    fn test_themes_dir() {
        let dir = themes_dir();
        assert!(dir.to_string_lossy().contains("themes"));
    }
}
