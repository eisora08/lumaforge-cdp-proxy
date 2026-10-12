use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet, HashSet};
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
        home_dir()?.join(".local/share/LumaForge/thirdparty/slssteam/bin/SLSsteam.so"),
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
        home_dir()?.join(".local/share/LumaForge/thirdparty/slssteam/bin/library-inject.so"),
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

/// Default config seeded on setup. `Plugins: yes` from the start so Steam
/// loads `~/.config/SLSsteam/plugins/` on the very first launch.
pub const DEFAULT_SLSSTEAM_CONFIG: &str = r#"# SLSsteam Configuration
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
AdditionalDepots:
DecryptionKeys:
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
Plugins: yes
DisableCloud: yes
DisableUpdates: yes
FakeName: ""
FakeEmail: ""
FakeWalletBalance: 0
LogLevels: 0xff
DumpClientInterfaces: no
ExtendedLogging: no
"#;

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

    std::fs::write(&config_path, DEFAULT_SLSSTEAM_CONFIG)
        .map_err(|e| format!("Failed to create default config: {e}"))?;

    match deploy_slssteam_plugins() {
        Ok(n) if n > 0 => {
            crate::log_to_temp(&format!("[slssteam] full_setup: wrote {n} plugin(s)"));
        }
        Ok(_) => {}
        Err(e) => crate::log_to_temp(&format!("[slssteam] full_setup: plugin deploy failed: {e}")),
    }

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

// ---------------------------------------------------------------------------
// SLSsteam Lua plugins (bundled — deployed to ~/.config/SLSsteam/plugins/)
// ---------------------------------------------------------------------------

const SLS_PLUGIN_DOWNLOAD: &str = include_str!("slssteam_plugins/download.lua");
const SLS_PLUGIN_SPLICED_TICKETS: &str = include_str!("slssteam_plugins/spliced-tickets.lua");

/// Write the bundled plugins, replacing files whose content differs.
/// Returns how many files were written.
fn deploy_slssteam_plugins() -> Result<usize, String> {
    let plugins_dir = get_config_path()
        .parent()
        .map(|p| p.join("plugins"))
        .ok_or_else(|| "config path has no parent".to_string())?;
    std::fs::create_dir_all(&plugins_dir)
        .map_err(|e| format!("Failed to create plugins dir: {e}"))?;

    let mut written = 0usize;
    for (name, content) in [
        ("download.lua", SLS_PLUGIN_DOWNLOAD),
        ("spliced-tickets.lua", SLS_PLUGIN_SPLICED_TICKETS),
    ] {
        let path = plugins_dir.join(name);
        let changed = match std::fs::read_to_string(&path) {
            Ok(existing) => existing != content,
            Err(_) => true,
        };
        if changed {
            std::fs::write(&path, content)
                .map_err(|e| format!("Failed to write plugin {name}: {e}"))?;
            written += 1;
        }
    }
    Ok(written)
}

// ---------------------------------------------------------------------------
// register_package — merge a downloaded .lua into ~/.config/SLSsteam/config.yaml
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
pub struct RegisterSummary {
    pub apps_added: usize,
    pub depots_added: usize,
    pub keys_added: usize,
    pub manifests_added: usize,
    pub dlc_named: usize,
    pub plugins_written: usize,
}

struct ParsedLuaStatements {
    /// (id, key, comment) — `key: Some` means a decryption-key statement (depot).
    addappids: Vec<(u64, Option<String>, Option<String>)>,
    /// Active setManifestid pins: (depot_id, manifest_gid).
    manifests: Vec<(u64, String)>,
}

fn parse_lua_for_config(content: &str) -> ParsedLuaStatements {
    let re_add = regex::Regex::new(
        r#"(?i)^addappid\s*\(\s*(\d+)\s*(?:,\s*\d+\s*(?:,\s*"([^"]*)")?\s*)?\)\s*(?:--\s*(.*))?$"#,
    );
    let re_man = regex::Regex::new(
        r#"(?i)^setManifestid\s*\(\s*(\d+)\s*,\s*"(\d+)"\s*(?:,\s*\d+)?\s*\)\s*(?:--.*)?$"#,
    );
    let (re_add, re_man) = match (re_add, re_man) {
        (Ok(a), Ok(m)) => (a, m),
        _ => {
            return ParsedLuaStatements {
                addappids: vec![],
                manifests: vec![],
            }
        }
    };

    let mut out = ParsedLuaStatements {
        addappids: Vec::new(),
        manifests: Vec::new(),
    };
    for raw in content.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with("--") {
            continue; // commented-out statements stay inactive
        }
        if let Some(caps) = re_add.captures(line) {
            if let Some(id) = caps.get(1).and_then(|m| m.as_str().parse::<u64>().ok()) {
                let key = caps
                    .get(2)
                    .map(|m| m.as_str().to_string())
                    .filter(|k| !k.is_empty());
                let comment = caps
                    .get(3)
                    .map(|m| m.as_str().trim().to_string())
                    .filter(|c| !c.is_empty());
                out.addappids.push((id, key, comment));
            }
        } else if let Some(caps) = re_man.captures(line) {
            if let Some(id) = caps.get(1).and_then(|m| m.as_str().parse::<u64>().ok()) {
                if let Some(gid) = caps.get(2).map(|m| m.as_str().to_string()) {
                    out.manifests.push((id, gid));
                }
            }
        }
    }
    out
}

/// Trailing comment of a bare `addappid(x) -- Name` usable as a DLC display
/// name. Filters out structural comments ("Depot 123", "Shared Depot 456").
fn dlc_comment_name(comment: Option<String>) -> Option<String> {
    let c = comment?;
    let trimmed = c.trim();
    if trimmed.is_empty() {
        return None;
    }
    let lower = trimmed.to_lowercase();
    if lower.starts_with("depot") || lower.starts_with("shared depot") {
        return None;
    }
    if trimmed.chars().all(|ch| ch.is_ascii_digit()) {
        return None;
    }
    Some(trimmed.to_string())
}

fn sanitize_comment(c: &str) -> String {
    c.replace(['\n', '\r'], " ").trim().to_string()
}

/// Locate a top-level `section:` header and the index where the section ends
/// (first later top-level line). Returns `(header, end)`.
fn section_bounds(lines: &[String], section: &str) -> Option<(usize, usize)> {
    let header_exact = format!("{section}:");
    let header = lines.iter().position(|l| {
        let t = l.trim_end();
        t == header_exact || l.starts_with(&format!("{header_exact} "))
    })?;
    let mut end = lines.len();
    for (i, l) in lines.iter().enumerate().skip(header + 1) {
        let t = l.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        if !l.starts_with(' ') && !l.starts_with('\t') {
            end = i;
            break;
        }
    }
    Some((header, end))
}

/// Item id of a dash-list line (`  - 105600   # name`).
fn dash_item_id(line: &str) -> Option<String> {
    let t = line.trim();
    let rest = t.strip_prefix('-')?.trim_start();
    if rest.is_empty() || rest.starts_with('-') || rest.starts_with('#') {
        return None;
    }
    let id = rest.split(&[' ', '\t', '#'][..]).next().unwrap_or("").trim();
    if id.is_empty() {
        return None;
    }
    Some(id.to_string())
}

/// Item id of a bare indented list line (`    105600   # name`).
fn bare_item_id(line: &str) -> Option<String> {
    let t = line.trim();
    if t.is_empty() || t.starts_with('#') || t.starts_with('-') {
        return None;
    }
    let id = t.split(&[' ', '\t', '#'][..]).next().unwrap_or("").trim();
    if id.is_empty() {
        return None;
    }
    Some(id.to_string())
}

/// Key of a map-item line (`  105601: "hex"`).
fn map_item_key(line: &str) -> Option<String> {
    let t = line.trim();
    if t.is_empty() || t.starts_with('#') {
        return None;
    }
    let key = t.split(':').next()?.trim();
    if key.is_empty() || key.starts_with('-') {
        return None;
    }
    Some(key.to_string())
}

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

fn rebuild(lines: &[String], trailing_nl: bool) -> String {
    let mut s = lines.join("\n");
    if trailing_nl {
        s.push('\n');
    }
    s
}

/// Ensure every `items` (id, comment) entry exists in a list section.
/// Detects dash vs bare item style (and indent) of existing entries and
/// inserts with the same style so the YAML stays parseable.
/// Returns how many entries were added.
fn ensure_list_items(
    content: &mut String,
    section: &str,
    items: &[(String, Option<String>)],
) -> usize {
    if items.is_empty() {
        return 0;
    }
    let trailing_nl = content.ends_with('\n');
    let mut lines: Vec<String> = content.lines().map(String::from).collect();

    let (header, end) = match section_bounds(&lines, section) {
        Some(b) => b,
        None => {
            if !content.is_empty() && !content.ends_with('\n') {
                content.push('\n');
            }
            content.push_str(&format!("{section}:\n"));
            for (id, comment) in items {
                match comment {
                    Some(c) => content.push_str(&format!("  - {id}   # {}\n", sanitize_comment(c))),
                    None => content.push_str(&format!("  - {id}\n")),
                }
            }
            return items.len();
        }
    };

    // Detect existing style + indent.
    let mut bare_style = false;
    let mut item_indent = 2usize;
    let mut existing: HashSet<String> = HashSet::new();
    let mut last_item: Option<usize> = None;
    for i in (header + 1)..end {
        let l = &lines[i];
        if let Some(id) = dash_item_id(l) {
            existing.insert(id);
            last_item = Some(i);
            item_indent = indent_of(l);
        } else if let Some(id) = bare_item_id(l) {
            bare_style = true;
            existing.insert(id);
            last_item = Some(i);
            item_indent = indent_of(l);
        }
    }

    let missing: Vec<&(String, Option<String>)> = items
        .iter()
        .filter(|(id, _)| !existing.contains(id))
        .collect();
    if missing.is_empty() {
        return 0;
    }

    let pad = " ".repeat(item_indent);
    let insert: Vec<String> = missing
        .iter()
        .map(|(id, comment)| {
            let suffix = match comment {
                Some(c) => format!("   # {}", sanitize_comment(c)),
                None => String::new(),
            };
            if bare_style {
                format!("{pad}{id}{suffix}")
            } else {
                format!("{pad}- {id}{suffix}")
            }
        })
        .collect();

    let at = match last_item {
        Some(i) => i + 1,
        None => header + 1,
    };
    let tail = lines.split_off(at);
    lines.extend(insert);
    lines.extend(tail);
    *content = rebuild(&lines, trailing_nl);
    missing.len()
}

/// Ensure every (key, value) entry exists in a map section (`  key: value`).
fn ensure_map_items(content: &mut String, section: &str, entries: &[(String, String)]) -> usize {
    if entries.is_empty() {
        return 0;
    }
    let trailing_nl = content.ends_with('\n');
    let mut lines: Vec<String> = content.lines().map(String::from).collect();

    let (header, end) = match section_bounds(&lines, section) {
        Some(b) => b,
        None => {
            if !content.is_empty() && !content.ends_with('\n') {
                content.push('\n');
            }
            content.push_str(&format!("{section}:\n"));
            for (k, v) in entries {
                content.push_str(&format!("  {k}: {v}\n"));
            }
            return entries.len();
        }
    };

    let mut item_indent = 2usize;
    let mut existing: HashSet<String> = HashSet::new();
    let mut last_item: Option<usize> = None;
    for i in (header + 1)..end {
        let l = &lines[i];
        if let Some(k) = map_item_key(l) {
            existing.insert(k);
            last_item = Some(i);
            item_indent = indent_of(l);
        }
    }

    let missing: Vec<&(String, String)> = entries
        .iter()
        .filter(|(k, _)| !existing.contains(k))
        .collect();
    if missing.is_empty() {
        return 0;
    }

    let pad = " ".repeat(item_indent);
    let insert: Vec<String> = missing
        .iter()
        .map(|(k, v)| format!("{pad}{k}: {v}"))
        .collect();

    let at = match last_item {
        Some(i) => i + 1,
        None => header + 1,
    };
    let tail = lines.split_off(at);
    lines.extend(insert);
    lines.extend(tail);
    *content = rebuild(&lines, trailing_nl);
    missing.len()
}

/// Ensure `key: value` at top level (rewrites an existing line in place).
fn ensure_scalar(content: &mut String, key: &str, value: &str) -> bool {
    let re = match regex::Regex::new(&format!(
        r"(?m)^{key}:[ \t]*(\S*)([ \t]*(?:#.*)?)$"
    )) {
        Ok(r) => r,
        Err(_) => return false,
    };
    let already = re
        .captures(content)
        .and_then(|c| c.get(1).map(|m| m.as_str()))
        .map(|v| v == value)
        .unwrap_or(false);
    if already {
        return false;
    }
    if re.find(content).is_some() {
        let replacement = format!("{key}: {value}$2");
        *content = re.replace(content, replacement).into_owned();
        return true;
    }
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    content.push_str(&format!("{key}: {value}\n"));
    true
}

/// Ensure `DlcData: { parent: { dlc_id: "Name" } }`. Matches existing child
/// indent (default 4). Returns how many entries were added.
fn ensure_dlc_data(content: &mut String, parent: u64, entries: &[(u64, String)]) -> usize {
    if entries.is_empty() {
        return 0;
    }
    let trailing_nl = content.ends_with('\n');
    let mut lines: Vec<String> = content.lines().map(String::from).collect();

    let (header, end) = match section_bounds(&lines, "DlcData") {
        Some(b) => b,
        None => {
            if !content.is_empty() && !content.ends_with('\n') {
                content.push('\n');
            }
            content.push_str("DlcData:\n");
            content.push_str(&format!("  {parent}:\n"));
            for (id, name) in entries {
                content.push_str(&format!("    {id}: \"{}\"\n", sanitize_comment(name)));
            }
            return entries.len();
        }
    };

    let parent_prefix = format!("{parent}:");
    let parent_pos = (header + 1..end).find(|&i| {
        let l = &lines[i];
        let indent = indent_of(l);
        let t = l.trim();
        indent == 2 && (t == parent_prefix || t.starts_with(&format!("{parent_prefix} ")))
    });

    let p = match parent_pos {
        Some(p) => p,
        None => {
            // Create the parent block right before the next top-level key.
            let mut insert = vec![format!("  {parent}:")];
            for (id, name) in entries {
                insert.push(format!("    {id}: \"{}\"", sanitize_comment(name)));
            }
            let tail = lines.split_off(end);
            lines.extend(insert);
            lines.extend(tail);
            *content = rebuild(&lines, trailing_nl);
            return entries.len();
        }
    };

    // Parent block bounds: next non-empty line at indent <= 2.
    let mut bend = end;
    for i in (p + 1)..end {
        let l = &lines[i];
        let t = l.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        if indent_of(l) <= 2 {
            bend = i;
            break;
        }
    }

    let mut child_indent = 4usize;
    let mut existing: HashSet<String> = HashSet::new();
    let mut last_item: Option<usize> = None;
    for i in (p + 1)..bend {
        let l = &lines[i];
        if let Some(k) = map_item_key(l) {
            existing.insert(k);
            last_item = Some(i);
            child_indent = indent_of(l);
        }
    }

    let missing: Vec<&(u64, String)> = entries
        .iter()
        .filter(|(id, _)| !existing.contains(&id.to_string()))
        .collect();
    if missing.is_empty() {
        return 0;
    }

    let pad = " ".repeat(child_indent);
    let insert: Vec<String> = missing
        .iter()
        .map(|(id, name)| format!("{pad}{id}: \"{}\"", sanitize_comment(name)))
        .collect();

    let at = match last_item {
        Some(i) => i + 1,
        None => p + 1,
    };
    let tail = lines.split_off(at);
    lines.extend(insert);
    lines.extend(tail);
    *content = rebuild(&lines, trailing_nl);
    missing.len()
}

/// Parse the installed `.lua` file(s) for `app_id` and merge the results into
/// `~/.config/SLSsteam/config.yaml` (creating it with `Plugins: yes` when
/// missing), then deploy the bundled SLSsteam plugins.
///
/// Mapping rules:
/// - bare `addappid(X)`            → `AdditionalApps` (main app and DLC app ids)
/// - keyed `addappid(X, 1, "key")` → `DecryptionKeys[X]`; additionally
///   `AdditionalApps` when X is the package app id, else `AdditionalDepots`
/// - active `setManifestid(X, "gid")` → `ManifestIds[X]`
/// - bare DLC entries with a trailing name comment → `DlcData[parent][X]`
///
/// Best-effort: never fails the install; errors are returned for logging.
pub fn register_package(app_id: &str, lua_paths: &[PathBuf]) -> Result<RegisterSummary, String> {
    let mut summary = RegisterSummary::default();

    let mut addappids: Vec<(u64, Option<String>, Option<String>)> = Vec::new();
    let mut manifests: Vec<(u64, String)> = Vec::new();
    for path in lua_paths {
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("Failed to read {}: {e}", path.display()))?;
        let parsed = parse_lua_for_config(&content);
        addappids.extend(parsed.addappids);
        manifests.extend(parsed.manifests);
    }

    let main_id: Option<u64> = app_id.parse().ok();

    let mut apps: BTreeSet<u64> = BTreeSet::new();
    let mut depots: BTreeSet<u64> = BTreeSet::new();
    let mut keys: BTreeMap<u64, String> = BTreeMap::new();
    let mut manifest_map: BTreeMap<u64, String> = BTreeMap::new();
    let mut dlc: Vec<(u64, String)> = Vec::new();
    let mut dlc_seen: BTreeSet<u64> = BTreeSet::new();

    for (id, key, comment) in addappids {
        match key {
            Some(k) => {
                if main_id == Some(id) {
                    apps.insert(id);
                } else {
                    depots.insert(id);
                }
                keys.insert(id, k);
            }
            None => {
                apps.insert(id);
                if main_id != Some(id) {
                    if let Some(name) = dlc_comment_name(comment) {
                        if dlc_seen.insert(id) {
                            dlc.push((id, name));
                        }
                    }
                }
            }
        }
    }
    for (id, gid) in manifests {
        manifest_map.insert(id, gid);
    }

    let cfg_path = get_config_path();
    let mut content = if cfg_path.exists() {
        std::fs::read_to_string(&cfg_path)
            .map_err(|e| format!("Failed to read config: {e}"))?
    } else {
        if let Some(parent) = cfg_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create config dir: {e}"))?;
        }
        DEFAULT_SLSSTEAM_CONFIG.to_string()
    };

    if ensure_scalar(&mut content, "Plugins", "yes") {
        crate::log_to_temp("[slssteam] register_package: Plugins set to yes");
    }

    let app_items: Vec<(String, Option<String>)> =
        apps.iter().map(|a| (a.to_string(), None)).collect();
    let depot_items: Vec<(String, Option<String>)> =
        depots.iter().map(|d| (d.to_string(), None)).collect();
    let key_items: Vec<(String, String)> = keys
        .iter()
        .map(|(k, v)| (k.to_string(), format!("\"{v}\"")))
        .collect();
    let manifest_items: Vec<(String, String)> = manifest_map
        .iter()
        .map(|(k, v)| (k.to_string(), format!("\"{v}\"")))
        .collect();

    summary.apps_added = ensure_list_items(&mut content, "AdditionalApps", &app_items);
    summary.depots_added = ensure_list_items(&mut content, "AdditionalDepots", &depot_items);
    summary.keys_added = ensure_map_items(&mut content, "DecryptionKeys", &key_items);
    summary.manifests_added = ensure_map_items(&mut content, "ManifestIds", &manifest_items);
    if let Some(parent) = main_id {
        summary.dlc_named = ensure_dlc_data(&mut content, parent, &dlc);
    }

    atomic_write(&cfg_path, &content)?;
    summary.plugins_written = deploy_slssteam_plugins()?;
    Ok(summary)
}


#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_LUA: &str = r#"-- fake generated lua
addappid(440)
addappid(550)
addappid(440110, 1, "089878asdkfja987554")
setManifestid(440, "1234567890123456789")
--setManifestid(550, "9999999999999999999")
--addappid(999)  -- commented out
addappid(440111)  -- DLC Awesome Pack
addappid(440112)  -- Depot 777
"#;

    #[test]
    fn parse_addappids_and_manifests() {
        let p = parse_lua_for_config(SAMPLE_LUA);
        // bare: 440, 550, 440111, 440112 (999 commented ? skipped)
        let bare: Vec<u64> = p
            .addappids
            .iter()
            .filter(|(_, k, _)| k.is_none())
            .map(|(id, _, _)| *id)
            .collect();
        assert_eq!(bare, vec![440, 550, 440111, 440112]);
        // keyed: 440110
        let keyed: Vec<(u64, &str)> = p
            .addappids
            .iter()
            .filter_map(|(id, k, _)| k.as_deref().map(|k| (*id, k)))
            .collect();
        assert_eq!(keyed, vec![(440110, "089878asdkfja987554")]);
        // active setManifestid only (commented one skipped)
        assert_eq!(p.manifests, vec![(440, "1234567890123456789".to_string())]);
    }

    #[test]
    fn parse_captures_trailing_comments() {
        let p = parse_lua_for_config(SAMPLE_LUA);
        let comments: Vec<(u64, Option<String>)> = p
            .addappids
            .iter()
            .map(|(id, _, c)| (*id, c.clone()))
            .collect();
        assert_eq!(
            comments,
            vec![
                (440, None),
                (550, None),
                (440110, None),
                (440111, Some("DLC Awesome Pack".to_string())),
                (440112, Some("Depot 777".to_string())),
            ]
        );
    }

    #[test]
    fn dlc_comment_filters_structural_names() {
        assert_eq!(dlc_comment_name(None), None);
        assert_eq!(dlc_comment_name(Some("".into())), None);
        assert_eq!(dlc_comment_name(Some("Depot 777".into())), None);
        assert_eq!(dlc_comment_name(Some("shared depot 123".into())), None);
        assert_eq!(dlc_comment_name(Some("456".into())), None);
        assert_eq!(
            dlc_comment_name(Some("DLC Awesome Pack".into())),
            Some("DLC Awesome Pack".to_string())
        );
    }

    #[test]
    fn ensure_list_items_creates_missing_section_with_dashes() {
        let mut c = String::from("AppIds:\nPlugins: yes\n");
        let items = vec![("440".to_string(), None), ("550".to_string(), None)];
        let added = ensure_list_items(&mut c, "AdditionalApps", &items);
        assert_eq!(added, 2);
        assert!(c.contains("AdditionalApps:\n  - 440\n  - 550\n"));
        // idempotent
        assert_eq!(ensure_list_items(&mut c, "AdditionalApps", &items), 0);
        assert_eq!(c.matches("- 440").count(), 1);
    }

    #[test]
    fn ensure_list_items_appends_to_existing_dash_style() {
        let mut c = "AdditionalApps:\n  - 440\nAppTokens:\n".to_string();
        let items = vec![("440".to_string(), None), ("550".to_string(), None)];
        let added = ensure_list_items(&mut c, "AdditionalApps", &items);
        assert_eq!(added, 1);
        assert!(c.starts_with("AdditionalApps:\n  - 440\n  - 550\n"));
        assert!(c.contains("AppTokens:"));
    }

    #[test]
    fn ensure_list_items_detects_bare_style() {
        let mut c = "AdditionalDepots:\n    105601\n".to_string();
        let items = vec![("105601".to_string(), None), ("105602".to_string(), None)];
        let added = ensure_list_items(&mut c, "AdditionalDepots", &items);
        assert_eq!(added, 1);
        assert!(c.contains("    105601\n    105602"));
    }

    #[test]
    fn ensure_map_items_creates_and_appends() {
        let mut c = String::from("ManifestIds:\n");
        let items = vec![("440".to_string(), "\"123\"".to_string())];
        assert_eq!(ensure_map_items(&mut c, "ManifestIds", &items), 1);
        assert!(c.contains("ManifestIds:\n  440: \"123\"\n"));
        let more = vec![
            ("440".to_string(), "\"123\"".to_string()),
            ("550".to_string(), "\"456\"".to_string()),
        ];
        assert_eq!(ensure_map_items(&mut c, "ManifestIds", &more), 1);
        assert!(c.contains("  550: \"456\""));
    }

    #[test]
    fn ensure_scalar_rewrites_and_creates() {
        let mut c = "Plugins: no\nDisableCloud: yes\n".to_string();
        assert!(ensure_scalar(&mut c, "Plugins", "yes"));
        assert!(c.contains("Plugins: yes\n"));
        assert!(!ensure_scalar(&mut c, "Plugins", "yes"));
        assert!(ensure_scalar(&mut c, "ExtendedLogging", "yes"));
        assert!(c.contains("ExtendedLogging: yes\n"));
        assert!(!c.contains("ExtendedLogging: yes\nExtendedLogging"));
    }

    #[test]
    fn ensure_dlc_data_creates_section_and_parent() {
        let mut c = String::from("AppIds:\n");
        let entries = vec![(440111u64, "DLC Awesome Pack".to_string())];
        let added = ensure_dlc_data(&mut c, 440, &entries);
        assert_eq!(added, 1);
        assert!(c.contains("DlcData:\n  440:\n    440111: \"DLC Awesome Pack\"\n"));
        // adding another child + a second parent
        let entries2 = vec![(440112u64, "More Stuff".to_string())];
        assert_eq!(ensure_dlc_data(&mut c, 440, &entries2), 1);
        let entries3 = vec![(550111u64, "Other Game DLC".to_string())];
        assert_eq!(ensure_dlc_data(&mut c, 550, &entries3), 1);
        assert!(c.contains("    440112: \"More Stuff\""));
        assert!(c.contains("  550:\n    550111: \"Other Game DLC\""));
    }

    #[test]
    fn sample_lua_classification_matches_expected_config() {
        let p = parse_lua_for_config(SAMPLE_LUA);
        let main_id = 440u64;
        let mut apps = BTreeSet::new();
        let mut depots = BTreeSet::new();
        let mut dlc = Vec::new();
        for (id, key, comment) in &p.addappids {
            match key {
                Some(_) => {
                    if main_id == *id {
                        apps.insert(*id);
                    } else {
                        depots.insert(*id);
                    }
                }
                None => {
                    apps.insert(*id);
                    if main_id != *id {
                        if let Some(name) = dlc_comment_name(comment.clone()) {
                            dlc.push((*id, name));
                        }
                    }
                }
            }
        }
        // main + bare DLC ids in AdditionalApps (550 is a bare addappid too)
        assert_eq!(apps.iter().copied().collect::<Vec<_>>(), vec![440, 550, 440111, 440112]);
        // keyed non-main id ? AdditionalDepots
        assert_eq!(depots.iter().copied().collect::<Vec<_>>(), vec![440110]);
        // named DLC only (Depot 777 filtered)
        assert_eq!(dlc, vec![(440111, "DLC Awesome Pack".to_string())]);
    }
}