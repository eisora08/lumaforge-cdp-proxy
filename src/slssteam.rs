use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;

use crate::platform;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SLSSteamStatus {
    pub installed: bool,
    pub slssteam_so_path: Option<String>,
    pub library_inject_so_path: Option<String>,
    pub config_exists: bool,
    pub steam_path: Option<String>,
    pub steam_install_type: Option<String>,
}

impl Default for SLSSteamStatus {
    fn default() -> Self {
        Self {
            installed: false,
            slssteam_so_path: None,
            library_inject_so_path: None,
            config_exists: false,
            steam_path: None,
            steam_install_type: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Path helpers
// ---------------------------------------------------------------------------

fn home_dir() -> Option<PathBuf> {
    std::env::var("HOME")
        .ok()
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir())
}

fn detect_steam_install_type(steam_path: &str) -> String {
    if steam_path.contains(".var/app/com.valvesoftware.Steam") {
        "flatpak".to_string()
    } else {
        "native".to_string()
    }
}

fn slssteam_lib_dir(steam_path: &str) -> PathBuf {
    let install_type = detect_steam_install_type(steam_path);
    if install_type == "flatpak" {
        home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".var")
            .join("app")
            .join("com.valvesoftware.Steam")
            .join(".local")
            .join("share")
            .join("SLSsteam")
    } else {
        home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".local")
            .join("share")
            .join("SLSsteam")
    }
}

fn get_config_path() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        PathBuf::from(xdg).join("SLSsteam").join("config.yaml")
    } else {
        home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".config")
            .join("SLSsteam")
            .join("config.yaml")
    }
}

fn find_slssteam_so(steam_path: Option<&str>) -> Option<String> {
    let mut candidates = vec![
        PathBuf::from("/usr/lib32/libSLSsteam.so"),
        home_dir()?.join(".local/share/SLSsteam/SLSsteam.so"),
        home_dir()?.join(".var/app/com.valvesoftware.Steam/.local/share/SLSsteam/SLSsteam.so"),
    ];

    if let Some(sp) = steam_path {
        let install_type = detect_steam_install_type(sp);
        if install_type == "flatpak" {
            candidates.push(
                home_dir()?.join(".var/app/com.valvesoftware.Steam/.local/share/SLSsteam/SLSsteam.so"),
            );
        } else {
            candidates.push(
                home_dir()?.join(".local/share/SLSsteam/SLSsteam.so"),
            );
        }
    }

    for path in &candidates {
        if path.exists() {
            return Some(path.to_string_lossy().to_string());
        }
    }
    None
}

fn find_library_inject_so(steam_path: Option<&str>) -> Option<String> {
    let mut candidates = vec![
        PathBuf::from("/usr/lib32/libSLS-library-inject.so"),
        home_dir()?.join(".local/share/SLSsteam/library-inject.so"),
        home_dir()?.join(".var/app/com.valvesoftware.Steam/.local/share/SLSsteam/library-inject.so"),
    ];

    if let Some(sp) = steam_path {
        let install_type = detect_steam_install_type(sp);
        if install_type == "flatpak" {
            candidates.push(
                home_dir()?.join(".var/app/com.valvesoftware.Steam/.local/share/SLSsteam/library-inject.so"),
            );
        } else {
            candidates.push(
                home_dir()?.join(".local/share/SLSsteam/library-inject.so"),
            );
        }
    }

    for path in &candidates {
        if path.exists() {
            return Some(path.to_string_lossy().to_string());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

pub fn get_status() -> serde_json::Value {
    let steam_path = platform::find_steam_install()
        .map(|p| p.to_string_lossy().to_string());
    let slssteam_so = find_slssteam_so(steam_path.as_deref());
    let library_inject_so = find_library_inject_so(steam_path.as_deref());
    let config_exists = get_config_path().exists();
    let install_type = steam_path.as_deref().map(detect_steam_install_type);

    let status = SLSSteamStatus {
        installed: slssteam_so.is_some() && library_inject_so.is_some(),
        slssteam_so_path: slssteam_so,
        library_inject_so_path: library_inject_so,
        config_exists,
        steam_path,
        steam_install_type: install_type,
    };

    json!({
        "ok": true,
        "installed": status.installed,
        "slssteamSoPath": status.slssteam_so_path,
        "libraryInjectSoPath": status.library_inject_so_path,
        "configExists": status.config_exists,
        "steamPath": status.steam_path,
        "steamInstallType": status.steam_install_type,
    })
}

pub fn kill_steam() -> Result<bool, String> {
    let output = StdCommand::new("pkill")
        .args(["-f", "[Ss]team"])
        .output()
        .or_else(|_| {
            StdCommand::new("killall")
                .args(["-q", "steam", "Steam", "steam.sh"])
                .output()
        })
        .map_err(|e| format!("Failed to kill Steam: {e}"))?;

    crate::log_to_temp(&format!("[slssteam] kill_steam result: {}", output.status));
    Ok(output.status.success())
}

pub fn start_steam(with_ld_audit: bool) -> Result<serde_json::Value, String> {
    let steam_path = platform::find_steam_install()
        .ok_or("Steam installation not found")?
        .to_string_lossy()
        .to_string();

    if with_ld_audit {
        let slssteam_so = find_slssteam_so(Some(&steam_path))
            .ok_or("SLSsteam.so not found. Install SLS Steam from Settings > Third-Party Tools.")?;
        let library_inject_so = find_library_inject_so(Some(&steam_path))
            .ok_or("library-inject.so not found. Install SLS Steam from Settings > Third-Party Tools.")?;

        let ld_audit = format!("{library_inject_so}:{slssteam_so}");
        crate::log_to_temp(&format!("[slssteam] Starting Steam with LD_AUDIT={ld_audit}"));

        let mut cmd = StdCommand::new("steam");
        cmd.env("LD_AUDIT", &ld_audit);
        cmd.current_dir(&steam_path);
        cmd.spawn()
            .map_err(|e| format!("Failed to launch Steam: {e}"))?;

        Ok(json!({
            "ok": true,
            "message": format!("Steam started with SLS Steam (LD_AUDIT={ld_audit})"),
        }))
    } else {
        crate::log_to_temp("[slssteam] Starting Steam without LD_AUDIT");
        StdCommand::new("steam")
            .current_dir(&steam_path)
            .spawn()
            .map_err(|e| format!("Failed to launch Steam: {e}"))?;

        Ok(json!({
            "ok": true,
            "message": "Steam started".to_string(),
        }))
    }
}

pub fn patch_steam_sh() -> Result<bool, String> {
    let steam_path = platform::find_steam_install()
        .ok_or("Steam installation not found")?
        .to_string_lossy()
        .to_string();

    let steam_sh = Path::new(&steam_path).join("steam.sh");
    if !steam_sh.exists() {
        return Err("steam.sh not found".to_string());
    }

    let slssteam_so = find_slssteam_so(Some(&steam_path))
        .ok_or("SLSsteam.so not found")?;
    let library_inject_so = find_library_inject_so(Some(&steam_path))
        .ok_or("library-inject.so not found")?;

    let ld_audit = format!("{library_inject_so}:{slssteam_so}");

    let content = std::fs::read_to_string(&steam_sh)
        .map_err(|e| format!("Failed to read steam.sh: {e}"))?;

    let backup = steam_sh.with_extension("sh.bak");
    if !backup.exists() {
        std::fs::copy(&steam_sh, &backup)
            .map_err(|e| format!("Failed to create backup: {e}"))?;
    }

    let lines: Vec<&str> = content.lines().collect();
    let mut new_lines: Vec<String> = Vec::new();
    let mut inserted = false;

    for (i, line) in lines.iter().enumerate() {
        if line.contains("export LD_AUDIT=") {
            continue;
        }
        new_lines.push(line.to_string());

        if !inserted && i == 9 {
            new_lines.push(format!("export LD_AUDIT={ld_audit}"));
            inserted = true;
        }
    }

    if !inserted {
        new_lines.push(format!("export LD_AUDIT={ld_audit}"));
    }

    let new_content = new_lines.join("\n");
    std::fs::write(&steam_sh, &new_content)
        .map_err(|e| format!("Failed to write steam.sh: {e}"))?;

    let steam_cfg = Path::new(&steam_path).join("steam.cfg");
    let cfg_content = "BootStrapperInhibitAll=enable\nBootStrapperForceSelfUpdate=disable\n";
    std::fs::write(&steam_cfg, cfg_content)
        .map_err(|e| format!("Failed to create steam.cfg: {e}"))?;

    Ok(true)
}

pub fn full_setup() -> Result<serde_json::Value, String> {
    let steam_path = platform::find_steam_install()
        .ok_or("Steam installation not found")?
        .to_string_lossy()
        .to_string();

    patch_steam_sh()?;

    let config_path = get_config_path();
    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create config dir: {e}"))?;
    }

    let default_config = r#"# SLSsteam Configuration
# Generated by LumaForge CDP Proxy

DisableFamilyShareLock: yes
UseWhitelist: no
AppIds:
AdditionalApps:
DlcData:
AppTokens:
CDKeys:
FakeOffline:
FakeAppIds:
ManifestIds:
DepotBlacklist:
GameTitles:
SubscriptionTimestamps:
DenuvoGames:
SteamIdOverride:
SmartTickets: 0x1
MaxSchemaTries: 10
LaunchOptions:
SafeMode: no
WarnHashMissmatch: no
NotifyInit: yes
API: yes
Plugins: no
DisableCloud: yes
DisableUpdates: yes
FakeName: ""
FakeEmail: ""
FakeWalletBalance: 0
LogLevels: 0xff
DumpClientInterfaces: no
ExtendedLogging: no
"#;
    std::fs::write(&config_path, default_config)
        .map_err(|e| format!("Failed to create default config: {e}"))?;

    let install_type = detect_steam_install_type(&steam_path);
    Ok(json!({
        "ok": true,
        "message": format!("SLS Steam setup complete ({install_type} installation). Steam.sh patched, steam.cfg created, config initialized."),
    }))
}

// ---------------------------------------------------------------------------
// Config management — simplified YAML-like operations
// ---------------------------------------------------------------------------

fn atomic_write(path: &Path, content: &str) -> Result<(), String> {
    let tmp = path.with_extension("yaml.tmp");
    std::fs::write(&tmp, content).map_err(|e| format!("Failed to write temp config: {e}"))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("Failed to rename temp config: {e}"))
}

pub fn config_add_app(app_id: &str, comment: &str) -> Result<bool, String> {
    let path = get_config_path();

    if !path.exists() {
        let entry = if comment.is_empty() {
            format!("AdditionalApps:\n  - {app_id}\n")
        } else {
            format!("AdditionalApps:\n  - {app_id}   # {comment}\n")
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create config dir: {e}"))?;
        }
        atomic_write(&path, &entry)?;
        return Ok(true);
    }

    let content = std::fs::read_to_string(&path)
        .map_err(|e| format!("Failed to read config: {e}"))?;

    let re = regex::Regex::new(&format!(
        r"(?m)^\s*-\s*{}\s*(?:#.*)?$",
        regex::escape(app_id)
    ))
    .map_err(|e| format!("Regex error: {e}"))?;

    if re.is_match(&content) {
        return Ok(false); // already exists
    }

    let section_re = regex::Regex::new(r"(?m)^AdditionalApps:\s*$")
        .map_err(|e| format!("Regex error: {e}"))?;

    let new_entry = if comment.is_empty() {
        format!("  - {app_id}\n")
    } else {
        format!("  - {app_id}   # {comment}\n")
    };

    if let Some(mat) = section_re.find(&content) {
        let after_header = &content[mat.end()..];
        let skip = if after_header.starts_with('\n') { 1 } else { 0 };
        let mut last_item_end = mat.end() + skip;

        let after = &content[last_item_end..];
        let lines: Vec<&str> = after.lines().collect();

        for line in &lines {
            let trimmed = line.trim();
            if trimmed.starts_with('-') {
                let line_start = content[..last_item_end].len()
                    + content[last_item_end..].find(line).unwrap_or(0);
                last_item_end = line_start + line.len();
                if content.get(last_item_end..last_item_end + 1) == Some("\n") {
                    last_item_end += 1;
                }
            } else if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            } else {
                break;
            }
        }

        let mut new_content = content.clone();
        new_content.insert_str(last_item_end, &new_entry);
        atomic_write(&path, &new_content)?;
    } else {
        let mut new_content = content;
        if !new_content.ends_with('\n') {
            new_content.push('\n');
        }
        new_content.push_str(&format!("\nAdditionalApps:\n{new_entry}"));
        atomic_write(&path, &new_content)?;
    }

    Ok(true)
}

pub fn config_remove_app(app_id: &str) -> Result<bool, String> {
    let path = get_config_path();
    if !path.exists() {
        return Ok(false);
    }

    let content = std::fs::read_to_string(&path)
        .map_err(|e| format!("Failed to read config: {e}"))?;

    let pattern = regex::Regex::new(&format!(
        r"(?m)^\s*-\s*{}\s*(?:#.*)?\n?$",
        regex::escape(app_id)
    ))
    .map_err(|e| format!("Regex error: {e}"))?;

    if pattern.find(&content).is_none() {
        return Ok(false);
    }

    let new_content = pattern.replace_all(&content, "");
    atomic_write(&path, &new_content)?;
    Ok(true)
}

pub fn config_get_apps() -> Result<Vec<String>, String> {
    let path = get_config_path();
    if !path.exists() {
        return Ok(vec![]);
    }

    let content = std::fs::read_to_string(&path)
        .map_err(|e| format!("Failed to read config: {e}"))?;

    let mut apps = Vec::new();
    let mut in_additional = false;

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed == "AdditionalApps:" {
            in_additional = true;
            continue;
        }
        if in_additional {
            if trimmed.starts_with('-') {
                let app = trimmed
                    .trim_start_matches('-')
                    .trim()
                    .split('#')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_string();
                if !app.is_empty() {
                    apps.push(app);
                }
            } else if !trimmed.is_empty() && !trimmed.starts_with('#') {
                break;
            }
        }
    }

    Ok(apps)
}
