use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const CONNECT_TIMEOUT_SECS: u64 = 30;
const DOWNLOAD_TIMEOUT_SECS: u64 = 300;
const FIND_DLL_DEPTH: u32 = 12;
const PROXY_DLL_CANDIDATES: &[&str] = &["version.dll", "winhttp.dll", "winmm.dll"];
const STEAMSTUB_MARKER: &[u8] = b"SteamStub";
const STEAMSTUB_SCAN_BYTES: usize = 4 * 1024 * 1024;
const APP_USER_AGENT: &str = concat!(
    env!("CARGO_PKG_NAME"),
    "/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/eisora08/lumaforge-cdp-proxy)"
);

// Catalog (Cloudflare Pages) — key lives in config.json (fixes.catalogKey),
// never in the frontend bundle. Env override: LUMAFORGE_FIXES_KEY.
const CATALOG_URL: &str = "https://lumaforge-fixes.pages.dev/catalog.json";
const CATALOG_CACHE_TTL_SECS: u64 = 300;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GameFixInfo {
    pub app_id: u64,
    pub name: String,
    pub installed: bool,
    pub install_path: Option<String>,
    pub has_online_fix: bool,
    pub has_steam_api_64: bool,
    pub has_steam_api_32: bool,
    pub game_arch: Option<String>,
    pub main_exe: Option<String>,
    pub has_steam_stub_drm: bool,
    pub exe_name: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FixInstallationStatus {
    pub smoke_api_installed: bool,
    pub steamless_installed: bool,
    pub koaloader_installed: bool,
    pub goldberg_installed: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GameFixResult {
    pub ok: bool,
    pub tool: String,
    pub message: String,
    pub files_installed: Vec<String>,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct FixJob {
    app_id: String,
    tool: String,
    op: String,
    status: String, // running | done | error
    progress: u8,
    message: String,
    result: Option<GameFixResult>,
}

#[derive(Debug, Clone, Deserialize)]
struct ApplyRequest {
    tool: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    download_url: Option<String>,
    #[serde(default)]
    fix_type: Option<String>,
    #[serde(default)]
    account_name: Option<String>,
    #[serde(default)]
    steam_id: Option<String>,
    #[serde(default)]
    manual_file: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct UnfixRequest {
    tool: String,
    #[serde(default)]
    fix_type: Option<String>,
}

// ---------------------------------------------------------------------------
// Jobs
// ---------------------------------------------------------------------------

static JOBS: OnceLock<RwLock<HashMap<String, FixJob>>> = OnceLock::new();

fn jobs() -> &'static RwLock<HashMap<String, FixJob>> {
    JOBS.get_or_init(|| RwLock::new(HashMap::new()))
}

fn job_key(app_id: u64, tool: &str) -> String {
    format!("{app_id}:{tool}")
}

fn set_job(key: &str, job: FixJob) {
    if let Ok(mut map) = jobs().write() {
        map.insert(key.to_string(), job);
    }
}

fn update_job<F: FnOnce(&mut FixJob)>(key: &str, f: F) {
    if let Ok(mut map) = jobs().write() {
        if let Some(job) = map.get_mut(key) {
            f(job);
        }
    }
}

fn job_is_running(key: &str) -> bool {
    jobs()
        .read()
        .map(|m| m.get(key).map(|j| j.status == "running").unwrap_or(false))
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

fn thirdparty_tool_dir(id: &str) -> PathBuf {
    crate::platform::local_data_dir().join("thirdparty").join(id)
}

fn is_dir_populated(dir: &Path) -> bool {
    dir.is_dir()
        && std::fs::read_dir(dir)
            .ok()
            .and_then(|mut entries| entries.next())
            .is_some()
}

fn temp_fix_dir() -> PathBuf {
    let dir = crate::platform::local_data_dir().join("temp").join("game_fixes");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Enumerate all Steam library roots (steam_root + paths from libraryfolders.vdf).
fn library_roots() -> Vec<PathBuf> {
    let mut libs = Vec::new();
    let Some(steam_root) = crate::depot_downloader::steam_root() else {
        return libs;
    };
    libs.push(steam_root.clone());
    let vdf_path = steam_root.join("steamapps").join("libraryfolders.vdf");
    if let Ok(content) = std::fs::read_to_string(&vdf_path) {
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("\"path\"") || trimmed.starts_with("\"1\"") {
                if let Some(pos) = trimmed.find('"') {
                    let rest = &trimmed[pos + 1..];
                    if let Some(q1) = rest.find('"') {
                        let after = &rest[q1 + 1..];
                        if let Some(q2) = after.find('"') {
                            let value = &after[..q2];
                            // Only take values that look like paths (contain slash or drive)
                            if value.contains('/') || value.contains('\\') || value.chars().nth(1) == Some(':') {
                                let p = PathBuf::from(value.replace("\\\\", "/").replace("\\", "/"));
                                if p.is_dir() && !libs.contains(&p) {
                                    libs.push(p);
                                }
                            }
                        }
                    }
                }
            }
            // Also match: "path"  "D:\\SteamLibrary"
            if trimmed.starts_with("\"path\"") {
                if let Some(q1) = trimmed.find('"') {
                    let rest = &trimmed[q1 + 1..];
                    // skip key name
                    if let Some(q2) = rest.find('"') {
                        let after = &rest[q2 + 1..].trim_start();
                        if let Some(v1) = after.find('"') {
                            let after_v = &after[v1 + 1..];
                            if let Some(v2) = after_v.find('"') {
                                let value = &after_v[..v2];
                                if !value.is_empty() && (value.contains('/') || value.contains('\\')) {
                                    let p = PathBuf::from(value.replace("\\\\", "/").replace("\\", "/"));
                                    if p.is_dir() && !libs.contains(&p) {
                                        libs.push(p);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    libs
}

fn parse_acf_field(content: &str, key: &str) -> Option<String> {
    // Matches: "key"   "value"  (tabs/spaces flexible)
    let pattern = format!("\"{}\"", key);
    let idx = content.find(&pattern)?;
    let rest = &content[idx + pattern.len()..];
    let rest = rest.trim_start();
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

struct AcfInfo {
    name: String,
    install_dir: String,
    library_root: PathBuf,
}

fn find_acf(app_id: u64) -> Option<AcfInfo> {
    let acf_name = format!("appmanifest_{app_id}.acf");
    for lib in library_roots() {
        let acf_path = lib.join("steamapps").join(&acf_name);
        if !acf_path.exists() {
            continue;
        }
        let content = std::fs::read_to_string(&acf_path).ok()?;
        let install_dir = parse_acf_field(&content, "installdir")?;
        let name = parse_acf_field(&content, "name").unwrap_or_else(|| format!("App {app_id}"));
        return Some(AcfInfo {
            name,
            install_dir,
            library_root: lib,
        });
    }
    None
}

fn resolve_game_path(app_id: u64) -> Option<(PathBuf, String)> {
    let acf = find_acf(app_id)?;
    let game_path = acf
        .library_root
        .join("steamapps")
        .join("common")
        .join(&acf.install_dir);
    if game_path.is_dir() {
        Some((game_path, acf.name))
    } else {
        None
    }
}

fn resolve_required(app_id: u64) -> Result<(PathBuf, String), String> {
    resolve_game_path(app_id)
        .ok_or_else(|| format!("Game {app_id} is not installed"))
}

// ---------------------------------------------------------------------------
// Fix log system — never overwrite: always .bak first, log lists all files
// ---------------------------------------------------------------------------

fn fix_log_path(game_path: &Path, app_id: u64) -> PathBuf {
    game_path.join(format!("lumaforge-fix-log-{app_id}.log"))
}

fn now_local() -> String {
    crate::thirdparty::now_iso().replace('T', " ")
}

fn write_fix_log(
    game_path: &Path,
    app_id: u64,
    game_name: &str,
    tool: &str,
    files: &[String],
) -> Result<(), String> {
    let path = fix_log_path(game_path, app_id);
    let mut content = String::new();
    if path.exists() {
        content = std::fs::read_to_string(&path).unwrap_or_default();
        if !content.trim().is_empty() {
            content.push_str("\n\n---\n\n");
        }
    }
    let now = now_local();
    content.push_str(&format!(
        "[FIX]\nDate: {now}\nGame: {game_name}\nFix Type: {tool}\nMainEXE: {}\nFiles:\n",
        files.first().map(|s| s.as_str()).unwrap_or("")
    ));
    for f in files {
        content.push_str(&format!("{f}\n"));
    }
    content.push_str("[/FIX]\n");
    std::fs::write(&path, content).map_err(|e| format!("Failed to write fix log: {e}"))
}

struct FixBlock {
    fix_type: String,
    files: Vec<String>,
    full_block: String,
}

fn parse_fix_blocks(content: &str) -> Vec<FixBlock> {
    let mut blocks = Vec::new();
    for block in content.split("[FIX]") {
        let trimmed = block.trim();
        if trimmed.is_empty() {
            continue;
        }
        let block_body = match trimmed.split_once("[/FIX]") {
            Some((body, _)) => body,
            None => trimmed,
        };
        let mut fix_type = String::new();
        let mut files = Vec::new();
        let mut in_files = false;
        for line in block_body.lines() {
            let line = line.trim();
            if let Some(ft) = line.strip_prefix("Fix Type:") {
                fix_type = ft.trim().to_string();
            }
            if line == "Files:" {
                in_files = true;
                continue;
            }
            if in_files && !line.is_empty() {
                files.push(line.to_string());
            }
        }
        blocks.push(FixBlock {
            fix_type,
            files,
            full_block: format!("[FIX]{block}"),
        });
    }
    blocks
}

fn has_fix_in_log(game_path: &Path, app_id: u64, types: &[&str]) -> bool {
    let log_path = fix_log_path(game_path, app_id);
    if !log_path.exists() {
        return false;
    }
    let content = std::fs::read_to_string(&log_path).unwrap_or_default();
    let blocks = parse_fix_blocks(&content);
    blocks.iter().any(|b| types.iter().any(|t| b.fix_type == *t))
}

fn rewrite_log_without(game_path: &Path, app_id: u64, remove_types: &[&str]) {
    let log_path = fix_log_path(game_path, app_id);
    if !log_path.exists() {
        return;
    }
    let content = std::fs::read_to_string(&log_path).unwrap_or_default();
    let blocks = parse_fix_blocks(&content);
    let remaining: Vec<&FixBlock> = blocks
        .iter()
        .filter(|b| !remove_types.iter().any(|t| b.fix_type == *t))
        .collect();
    if remaining.is_empty() {
        let _ = std::fs::remove_file(&log_path);
    } else {
        let new_content: String = remaining
            .iter()
            .map(|b| b.full_block.as_str())
            .collect::<Vec<_>>()
            .join("\n\n---\n\n");
        let _ = std::fs::write(&log_path, new_content);
    }
}

fn files_for_types(game_path: &Path, app_id: u64, types: &[&str]) -> Vec<String> {
    let log_path = fix_log_path(game_path, app_id);
    if !log_path.exists() {
        return Vec::new();
    }
    let content = std::fs::read_to_string(&log_path).unwrap_or_default();
    parse_fix_blocks(&content)
        .iter()
        .filter(|b| types.iter().any(|t| b.fix_type == *t))
        .flat_map(|b| b.files.clone())
        .collect()
}

/// Never overwrite: rename existing dest to `{dest}.bak`.
fn backup_file_if_exists(dest: &Path) -> bool {
    if !dest.exists() {
        return false;
    }
    let bak = PathBuf::from(format!("{}.bak", dest.to_string_lossy()));
    if bak.exists() {
        let _ = std::fs::remove_file(&bak);
    }
    std::fs::rename(dest, &bak).is_ok()
}

fn restore_bak(bak_path: &Path) -> Result<(), String> {
    let s = bak_path.to_string_lossy();
    let original = PathBuf::from(s.strip_suffix(".bak").unwrap_or(&s));
    if original.exists() {
        std::fs::remove_file(&original).map_err(|e| format!("remove {original:?}: {e}"))?;
    }
    std::fs::rename(bak_path, &original).map_err(|e| format!("rename {bak_path:?}: {e}"))
}

// ---------------------------------------------------------------------------
// File helpers
// ---------------------------------------------------------------------------

fn find_file_recursive(dir: &Path, filename: &str) -> Option<PathBuf> {
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = find_file_recursive(&path, filename) {
                return Some(found);
            }
        } else if path.file_name().and_then(|n| n.to_str()) == Some(filename) {
            return Some(path);
        }
    }
    None
}

fn find_file_recursive_bounded(dir: &Path, filename: &str, depth: u32) -> Option<PathBuf> {
    if depth == 0 {
        return None;
    }
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = find_file_recursive_bounded(&path, filename, depth - 1) {
                return Some(found);
            }
        } else if path.file_name().and_then(|n| n.to_str()) == Some(filename) {
            return Some(path);
        }
    }
    None
}

fn find_bak_files_recursive(dir: &Path, results: &mut Vec<PathBuf>) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                find_bak_files_recursive(&path, results);
            } else if path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.to_lowercase().ends_with(".exe.bak"))
            {
                results.push(path);
            }
        }
    }
}

fn find_goldberg_bak_recursive(dir: &Path, results: &mut Vec<PathBuf>) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                find_goldberg_bak_recursive(&path, results);
            } else if path.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                let lower = n.to_lowercase();
                lower == "steam_api64.dll.bak" || lower == "steam_api.dll.bak"
            }) {
                results.push(path);
            }
        }
    }
}

fn find_configs_user_ini_recursive(dir: &Path, results: &mut Vec<PathBuf>) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                find_configs_user_ini_recursive(&path, results);
            } else if path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.to_lowercase() == "configs.user.ini")
            {
                results.push(path);
            }
        }
    }
}

fn clean_empty_dirs_recursive(root: &Path) {
    for entry in std::fs::read_dir(root).into_iter().flatten().flatten() {
        if entry.path().is_dir() {
            clean_empty_dirs_recursive(&entry.path());
            let _ = std::fs::remove_dir(entry.path());
        }
    }
}

fn copy_dir_with_backup(
    src: &Path,
    dest_base: &Path,
    dest: &Path,
    installed: &mut Vec<String>,
) -> Result<(), String> {
    for entry in std::fs::read_dir(src).map_err(|e| format!("read dir: {e}"))? {
        let entry = entry.map_err(|e| format!("read entry: {e}"))?;
        let src_path = entry.path();
        let dest_path = dest.join(entry.file_name());
        if src_path.is_dir() {
            std::fs::create_dir_all(&dest_path).map_err(|e| format!("mkdir: {e}"))?;
            copy_dir_with_backup(&src_path, dest_base, &dest_path, installed)?;
        } else {
            if let Some(parent) = dest_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            backup_file_if_exists(&dest_path);
            std::fs::copy(&src_path, &dest_path).map_err(|e| format!("copy: {e}"))?;
            let relative = dest_path
                .strip_prefix(dest_base)
                .unwrap_or(&dest_path)
                .to_string_lossy()
                .to_string();
            installed.push(relative);
        }
    }
    Ok(())
}

fn flatten_extracted_dir(dir: &Path) -> Option<PathBuf> {
    let mut entries = std::fs::read_dir(dir).ok()?;
    let first = entries.next()?.ok()?;
    if entries.next().is_some() {
        return None;
    }
    if first.path().is_dir() {
        Some(first.path())
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// EXE discovery (Win64 preference, non-game filter)
// ---------------------------------------------------------------------------

fn exe_win64_priority(path: &Path) -> u32 {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_lowercase();
    let parent_base = path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_lowercase();
    let parent = path
        .parent()
        .map(|p| p.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if name.ends_with("win64-shipping.exe") {
        4
    } else if parent_base == "win64" {
        3
    } else if name.contains("win64") {
        2
    } else if parent.contains("win64") {
        1
    } else {
        0
    }
}

const NON_GAME_EXE_NAMES: &[&str] = &[
    "unrealeditor.exe",
    "ue4editor.exe",
    "ue5editor.exe",
    "crashreportclient.exe",
    "crashreportclienteditor.exe",
    "unrealcefsubprocess.exe",
    "shadercompileworker.exe",
    "unrealpak.exe",
    "unrealinsights.exe",
    "unrealcer.exe",
    "epicgameslauncher.exe",
    "setup.exe",
    "install.exe",
    "installer.exe",
    "autorun.exe",
    "dxsetup.exe",
    "redist.exe",
];

fn is_non_game_exe(name_lower: &str) -> bool {
    NON_GAME_EXE_NAMES.contains(&name_lower)
        || name_lower.starts_with("crashreportclient")
        || name_lower.starts_with("unitycrashhandler")
        || name_lower.starts_with("crashpad")
        || name_lower.starts_with("vcredist")
        || name_lower.starts_with("vc_redist")
        || name_lower.starts_with("dotnet")
        || name_lower.ends_with("unins000.exe")
}

fn filter_non_game_exes(exes: &mut Vec<(u64, PathBuf)>) {
    if exes.is_empty() {
        return;
    }
    let keep: Vec<(u64, PathBuf)> = exes
        .iter()
        .filter(|(_, p)| {
            let name = p
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_lowercase();
            !is_non_game_exe(&name)
        })
        .cloned()
        .collect();
    if !keep.is_empty() {
        *exes = keep;
    }
}

fn compare_exes_win64_first(a: &(u64, PathBuf), b: &(u64, PathBuf)) -> std::cmp::Ordering {
    exe_win64_priority(&b.1)
        .cmp(&exe_win64_priority(&a.1))
        .then_with(|| b.0.cmp(&a.0))
}

fn collect_exes_recursive(dir: &Path, exes: &mut Vec<(u64, PathBuf)>) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_exes_recursive(&path, exes);
            } else if path.extension().and_then(|e| e.to_str()) == Some("exe") {
                if let Ok(meta) = std::fs::metadata(&path) {
                    exes.push((meta.len(), path));
                }
            }
        }
    }
}

fn find_candidate_exes(game_path: &Path) -> Vec<PathBuf> {
    let mut exes: Vec<(u64, PathBuf)> = Vec::new();
    collect_exes_recursive(game_path, &mut exes);
    filter_non_game_exes(&mut exes);
    exes.sort_by(compare_exes_win64_first);
    exes.into_iter().map(|(_, p)| p).collect()
}

fn find_main_exe(game_path: &Path) -> Option<PathBuf> {
    find_candidate_exes(game_path).into_iter().next()
}

fn find_game_exe_dir(game_path: &Path) -> PathBuf {
    if let Some(exe) = find_main_exe(game_path) {
        if let Some(parent) = exe.parent() {
            return parent.to_path_buf();
        }
    }
    game_path.to_path_buf()
}

// ---------------------------------------------------------------------------
// PE helpers (goblin)
// ---------------------------------------------------------------------------

fn detect_architecture(game_path: &Path) -> (Option<String>, bool, bool) {
    let has_64 = find_file_recursive_bounded(game_path, "steam_api64.dll", FIND_DLL_DEPTH).is_some();
    let has_32 = find_file_recursive_bounded(game_path, "steam_api.dll", FIND_DLL_DEPTH).is_some();
    let arch = if has_64 {
        Some("x64".to_string())
    } else if has_32 {
        Some("x86".to_string())
    } else {
        find_main_exe(game_path).and_then(|exe| detect_exe_arch(&exe).ok().flatten())
    };
    (arch, has_64, has_32)
}

fn detect_exe_arch(exe_path: &Path) -> Result<Option<String>, String> {
    let data = std::fs::read(exe_path).map_err(|e| e.to_string())?;
    let pe = goblin::pe::PE::parse(&data).map_err(|e| format!("PE parse: {e}"))?;
    Ok(Some(if pe.is_64 {
        "x64".to_string()
    } else {
        "x86".to_string()
    }))
}

fn get_game_imported_dlls(game_path: &Path) -> Vec<String> {
    let mut exes: Vec<(u64, PathBuf)> = Vec::new();
    collect_exes_recursive(game_path, &mut exes);
    filter_non_game_exes(&mut exes);
    exes.sort_by(compare_exes_win64_first);
    let Some((_, exe)) = exes.into_iter().next() else {
        return Vec::new();
    };
    let Ok(data) = std::fs::read(&exe) else {
        return Vec::new();
    };
    let Ok(pe) = goblin::pe::PE::parse(&data) else {
        return Vec::new();
    };
    pe.imports.iter().map(|i| i.dll.to_lowercase()).collect()
}

fn has_steamstub_drm(exe: &Path) -> bool {
    let Ok(data) = std::fs::read(exe) else {
        return false;
    };
    if let Ok(pe) = goblin::pe::PE::parse(&data) {
        for section in &pe.sections {
            let lower = section.name().unwrap_or_default().to_lowercase();
            if lower.contains(".stub") || lower.contains("steamstub") {
                return true;
            }
        }
    }
    let scan_len = data.len().min(STEAMSTUB_SCAN_BYTES);
    data[..scan_len]
        .windows(STEAMSTUB_MARKER.len())
        .any(|w| w.eq_ignore_ascii_case(STEAMSTUB_MARKER))
}

fn find_proxy_dll_in_imports(imported: &[String]) -> Option<&'static str> {
    for candidate in PROXY_DLL_CANDIDATES {
        let lower = candidate.to_lowercase();
        if imported.contains(&lower) {
            return Some(candidate);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Hidden child processes (Windows)
// ---------------------------------------------------------------------------

#[cfg(target_os = "windows")]
fn hide_window(cmd: &mut std::process::Command) -> &mut std::process::Command {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    cmd.creation_flags(CREATE_NO_WINDOW);
    cmd
}

#[cfg(not(target_os = "windows"))]
fn hide_window(cmd: &mut std::process::Command) -> &mut std::process::Command {
    cmd
}

// ---------------------------------------------------------------------------
// HTTP helpers
// ---------------------------------------------------------------------------

fn http_client(timeout: Duration) -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .user_agent(APP_USER_AGENT)
        .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS))
        .timeout(timeout)
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {e}"))
}

fn download_file(url: &str, dest: &Path) -> Result<(), String> {
    let client = http_client(Duration::from_secs(DOWNLOAD_TIMEOUT_SECS))?;
    let resp = client
        .get(url)
        .send()
        .map_err(|e| format!("Failed to start download: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {} for {}", resp.status(), url));
    }
    let bytes = resp
        .bytes()
        .map_err(|e| format!("Failed to read download body: {e}"))?;
    if bytes.is_empty() {
        return Err(format!("Downloaded file is empty: {url}"));
    }
    if bytes.len() > 16
        && (bytes[..16].eq_ignore_ascii_case(b"<!DOCTYPE html")
            || bytes[..16].eq_ignore_ascii_case(b"<html"))
    {
        return Err("Server returned HTML instead of a file archive".to_string());
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create dir: {e}"))?;
    }
    std::fs::write(dest, &bytes).map_err(|e| format!("write file: {e}"))
}

// ---------------------------------------------------------------------------
// SmokeAPI
// ---------------------------------------------------------------------------

fn apply_smoke_api(game_path: &Path) -> Result<Vec<String>, String> {
    #[cfg(not(target_os = "windows"))]
    {
        let _ = game_path;
        return Err("SmokeAPI is only available on Windows".to_string());
    }

    #[cfg(target_os = "windows")]
    {
        let plugins = thirdparty_tool_dir("smokeapi");
        if !is_dir_populated(&plugins) {
            return Err("SmokeAPI is not installed. Install it from Tools first.".to_string());
        }

        let (arch, has_64, has_32) = detect_architecture(game_path);
        let architecture = arch.unwrap_or_else(|| "x64".to_string());
        let game_exe_dir = find_game_exe_dir(game_path);
        let imported = get_game_imported_dlls(game_path);
        let proxy_dll = if has_64 || has_32 {
            find_proxy_dll_in_imports(&imported)
        } else {
            None
        };

        let dll_name = if architecture == "x64" {
            "smoke_api64.dll"
        } else {
            "smoke_api32.dll"
        };

        let src_dll = find_file_recursive(&plugins, dll_name)
            .ok_or_else(|| format!("Could not find {dll_name} in installed SmokeAPI"))?;

        match proxy_dll {
            Some(proxy_name) => {
                let target_path = game_exe_dir.join(proxy_name);
                backup_file_if_exists(&target_path);
                std::fs::copy(&src_dll, &target_path)
                    .map_err(|e| format!("Failed to copy DLL: {e}"))?;
                Ok(vec![proxy_name.to_string()])
            }
            None => {
                // Koaloader mode: download if missing, then copy d3d11 + smoke dll
                let koaloader_dir = thirdparty_tool_dir("koaloader");
                if !is_dir_populated(&koaloader_dir) {
                    install_tool_from_github("acidicoala", "Koaloader", &koaloader_dir)?;
                }
                let subdir = if architecture == "x64" { "d3d11-64" } else { "d3d11-32" };
                let koaloader_src = find_file_recursive(&koaloader_dir.join(subdir), "d3d11.dll")
                    .or_else(|| find_file_recursive(&koaloader_dir, "d3d11.dll"))
                    .ok_or_else(|| "Koaloader d3d11.dll not found".to_string())?;
                let target_koaloader = game_exe_dir.join("d3d11.dll");
                backup_file_if_exists(&target_koaloader);
                std::fs::copy(&koaloader_src, &target_koaloader)
                    .map_err(|e| format!("copy d3d11: {e}"))?;
                let target_smoke = game_exe_dir.join(dll_name);
                std::fs::copy(&src_dll, &target_smoke)
                    .map_err(|e| format!("copy smoke dll: {e}"))?;
                Ok(vec!["d3d11.dll".to_string(), dll_name.to_string()])
            }
        }
    }
}

fn unfix_smoke_api(game_path: &Path, app_id: u64) -> Result<GameFixResult, String> {
    let files = files_for_types(
        game_path,
        app_id,
        &["SmokeAPI", "smoke_api"],
    );
    if files.is_empty() {
        return Ok(GameFixResult {
            ok: false,
            tool: "smokeapi".into(),
            message: "No SmokeAPI fix entries found in log.".into(),
            files_installed: vec![],
            errors: vec![],
        });
    }
    let game_exe_dir = find_game_exe_dir(game_path);
    let total = files.len();
    let mut removed = 0;
    for file_name in &files {
        let file_path = game_exe_dir.join(file_name);
        let file_path = if file_path.exists() {
            file_path
        } else {
            game_path.join(file_name)
        };
        if file_path.exists() {
            let _ = std::fs::remove_file(&file_path);
        }
        let bak_path = PathBuf::from(format!("{}.bak", file_path.to_string_lossy()));
        if bak_path.exists() {
            let _ = restore_bak(&bak_path);
        }
        removed += 1;
    }
    rewrite_log_without(game_path, app_id, &["SmokeAPI", "smoke_api"]);
    Ok(GameFixResult {
        ok: true,
        tool: "smokeapi".into(),
        message: format!("Removed {removed}/{total} SmokeAPI file(s)"),
        files_installed: vec![],
        errors: vec![],
    })
}

// ---------------------------------------------------------------------------
// Steamless
// ---------------------------------------------------------------------------

fn strip_extended_prefix(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    if let Some(stripped) = s.strip_prefix(r"\\?\") {
        PathBuf::from(stripped)
    } else {
        path.to_path_buf()
    }
}

fn run_steamless_on_exe(game_exe: &Path) -> Result<Vec<String>, String> {
    #[cfg(not(target_os = "windows"))]
    {
        let _ = game_exe;
        return Err("Steamless is only available on Windows".to_string());
    }

    #[cfg(target_os = "windows")]
    {
        let plugins = thirdparty_tool_dir("steamless");
        if !is_dir_populated(&plugins) {
            return Err("Steamless is not installed. Install it from Tools first.".to_string());
        }
        let steamless_exe = find_file_recursive(&plugins, "Steamless.CLI.exe")
            .or_else(|| find_file_recursive(&plugins, "Steamless.exe"))
            .ok_or_else(|| "Could not find Steamless.CLI.exe".to_string())?;

        let exe_name = game_exe
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        if exe_name.ends_with(".bak") || exe_name.contains(".unpacked") {
            return Err("Invalid target exe (already unpacked or backup)".into());
        }
        let backup_path = PathBuf::from(format!("{}.bak", game_exe.to_string_lossy()));
        if backup_path.exists() {
            return Err(format!(
                "Steamless already applied (backup exists: {})",
                backup_path.file_name().unwrap_or_default().to_string_lossy()
            ));
        }

        let mut cmd = std::process::Command::new(&steamless_exe);
        cmd.current_dir(&plugins)
            .arg(
                strip_extended_prefix(game_exe)
                    .to_str()
                    .unwrap_or_default(),
            )
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        hide_window(&mut cmd);
        let result = cmd
            .output()
            .map_err(|e| format!("Failed to run Steamless: {e}"))?;

        let stdout = String::from_utf8_lossy(&result.stdout);
        let stderr = String::from_utf8_lossy(&result.stderr);

        let no_drm = stdout.contains("All unpackers failed to unpack file")
            || stdout.contains("is not supported")
            || (!result.status.success()
                && !stdout.contains("Successfully unpacked file!")
                && !stdout.contains("Error"));

        if no_drm {
            return Ok(vec![
                "__no_drm__".to_string(),
                "__no_unpack_needed__".to_string(),
            ]);
        }
        if !result.status.success() {
            return Err(format!(
                "Steamless failed (exit {}): {stdout} {stderr}",
                result.status.code().unwrap_or(-1)
            ));
        }
        if !stdout.contains("Successfully unpacked file!") {
            return Err(format!("Steamless did not unpack: {stdout} {stderr}"));
        }
        let unpacked_path = PathBuf::from(format!("{}.unpacked.exe", game_exe.to_string_lossy()));
        if !unpacked_path.exists() {
            return Err(format!(
                "Steamless reported success but unpacked file missing: {}",
                unpacked_path.display()
            ));
        }
        std::fs::rename(game_exe, &backup_path)
            .map_err(|e| format!("rename original to .bak: {e}"))?;
        std::fs::rename(&unpacked_path, game_exe)
            .map_err(|e| format!("rename unpacked: {e}"))?;
        Ok(vec![
            exe_name,
            backup_path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string(),
        ])
    }
}

fn apply_steamless(game_path: &Path) -> Result<Vec<String>, String> {
    let candidates = find_candidate_exes(game_path);
    if candidates.is_empty() {
        return Err("Game has no executable".into());
    }
    let mut files_installed = Vec::new();
    let mut errors = Vec::new();
    let mut no_drm = Vec::new();
    let mut already = Vec::new();
    let mut applied = Vec::new();

    for exe in &candidates {
        let exe_name = exe
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let backup_path = PathBuf::from(format!("{}.bak", exe.to_string_lossy()));
        if backup_path.exists() {
            already.push(exe_name);
            continue;
        }
        match run_steamless_on_exe(exe) {
            Ok(installed) if installed.first().is_some_and(|s| s == "__no_drm__") => {
                no_drm.push(exe_name);
            }
            Ok(installed) => {
                applied.push(exe_name);
                files_installed.extend(installed);
            }
            Err(e) => errors.push(format!("{exe_name}: {e}")),
        }
    }

    if !files_installed.is_empty() {
        return Ok(files_installed);
    }
    if !already.is_empty() {
        return Ok(vec![]);
    }
    if !no_drm.is_empty() {
        return Err(
            "None of the candidate executables use SteamStub DRM. No unpacking needed.".into(),
        );
    }
    Err(format!(
        "Failed to apply Steamless to any executable: {}",
        errors.join("; ")
    ))
}

fn unfix_steamless(game_path: &Path, app_id: u64) -> Result<GameFixResult, String> {
    let mut bak_files = Vec::new();
    find_bak_files_recursive(game_path, &mut bak_files);
    if bak_files.is_empty() {
        return Ok(GameFixResult {
            ok: false,
            tool: "steamless".into(),
            message: "No Steamless backups found to revert".into(),
            files_installed: vec![],
            errors: vec![],
        });
    }
    let total = bak_files.len();
    let mut restored = 0;
    let mut errors = Vec::new();
    for bak_path in &bak_files {
        match restore_bak(bak_path) {
            Ok(()) => restored += 1,
            Err(e) => errors.push(e),
        }
    }
    rewrite_log_without(game_path, app_id, &["Steamless"]);
    Ok(GameFixResult {
        ok: errors.is_empty(),
        tool: "steamless".into(),
        message: format!("Restored {restored}/{total} file(s)"),
        files_installed: vec![],
        errors,
    })
}

// ---------------------------------------------------------------------------
// Goldberg (fork)
// ---------------------------------------------------------------------------

fn apply_goldberg(game_path: &Path, app_id: u64) -> Result<Vec<String>, String> {
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (game_path, app_id);
        return Err("Goldberg is only available on Windows".to_string());
    }

    #[cfg(target_os = "windows")]
    {
        let emu_dir = thirdparty_tool_dir("goldberg_fork");
        if !is_dir_populated(&emu_dir) {
            return Err(
                "Goldberg not installed. Install it from Tools first.".to_string(),
            );
        }
        let (_, has_64, has_32) = detect_architecture(game_path);
        if !has_64 && !has_32 {
            return Err(
                "No steam_api dll found. Goldberg needs the real steam_api dll to proxy."
                    .to_string(),
            );
        }
        let mut present_dlls: Vec<&str> = Vec::new();
        if has_64 {
            present_dlls.push("steam_api64.dll");
        }
        if has_32 {
            present_dlls.push("steam_api.dll");
        }

        let mut installed: Vec<String> = Vec::new();
        let mut errors: Vec<String> = Vec::new();
        let app_id_str = app_id.to_string();
        let output_steam_settings = emu_dir
            .join("output")
            .join(&app_id_str)
            .join("steam_settings");

        // Step 1: generate steam_interfaces.txt from original DLL
        let generate_interfaces = find_file_recursive_bounded(
            &emu_dir,
            "generate_interfaces_x64.exe",
            8,
        )
        .or_else(|| find_file_recursive_bounded(&emu_dir, "generate_interfaces_x86.exe", 8));
        let original_dll_path = present_dlls
            .iter()
            .filter_map(|dll| find_file_recursive_bounded(game_path, dll, FIND_DLL_DEPTH))
            .next();
        let mut steam_interfaces_content: Option<String> = None;
        if let (Some(tool), Some(dll)) = (&generate_interfaces, &original_dll_path) {
            let temp = std::env::temp_dir().join(format!("lf_goldberg_{app_id}"));
            let _ = std::fs::create_dir_all(&temp);
            let mut cmd = std::process::Command::new(tool);
            cmd.arg(dll).current_dir(&temp);
            hide_window(&mut cmd);
            match cmd.output() {
                Ok(_) => {
                    let p = temp.join("steam_interfaces.txt");
                    steam_interfaces_content = std::fs::read_to_string(&p).ok();
                }
                Err(e) => errors.push(format!("generate_interfaces failed: {e}")),
            }
            let _ = std::fs::remove_dir_all(&temp);
        }

        // Step 2: backup + copy Goldberg DLLs
        for emu_dll in &present_dlls {
            let Some(src_dll) = find_file_recursive_bounded(&emu_dir, emu_dll, FIND_DLL_DEPTH)
            else {
                errors.push(format!("Could not find {emu_dll} in installed Goldberg"));
                continue;
            };
            let Some(target_path) =
                find_file_recursive_bounded(game_path, emu_dll, FIND_DLL_DEPTH)
            else {
                errors.push(format!("Could not find real {emu_dll} in game"));
                continue;
            };
            backup_file_if_exists(&target_path);
            match std::fs::copy(&src_dll, &target_path) {
                Ok(_) => installed.push(emu_dll.to_string()),
                Err(e) => errors.push(format!("copy Goldberg {emu_dll}: {e}")),
            }
        }
        if installed.is_empty() {
            return Err(format!(
                "Goldberg could not be applied: {}",
                errors.join("; ")
            ));
        }

        // Step 3: generate_emu_config if output empty
        let output_has_content = output_steam_settings.exists()
            && std::fs::read_dir(&output_steam_settings)
                .map(|mut e| e.next().is_some())
                .unwrap_or(false);
        if !output_has_content {
            if let Some(tool) = find_file_recursive_bounded(&emu_dir, "generate_emu_config.exe", 8)
            {
                let mut cmd = std::process::Command::new(&tool);
                cmd.args(["-anon", "-skip_ach", &app_id_str])
                    .current_dir(&emu_dir);
                hide_window(&mut cmd);
                if cmd.output().is_err() {
                    errors.push("generate_emu_config failed to run".into());
                }
            } else {
                errors.push("generate_emu_config.exe not found".into());
            }
        }

        // Step 4: write steam_interfaces.txt into output
        if let Some(content) = &steam_interfaces_content {
            let _ = std::fs::create_dir_all(&output_steam_settings);
            if std::fs::write(output_steam_settings.join("steam_interfaces.txt"), content).is_ok()
            {
                installed.push("steam_settings/steam_interfaces.txt".into());
            }
        }

        // Step 5: copy steam_settings next to the steam_api dll
        let dll_dir = original_dll_path
            .as_ref()
            .and_then(|p| p.parent())
            .unwrap_or(game_path);
        let game_steam_settings = dll_dir.join("steam_settings");
        if output_steam_settings.exists() {
            let mut tmp_installed = Vec::new();
            if copy_dir_with_backup(
                &output_steam_settings,
                &game_steam_settings,
                &game_steam_settings,
                &mut tmp_installed,
            )
            .is_ok()
            {
                for f in tmp_installed {
                    installed.push(format!("steam_settings/{f}"));
                }
            }
        }

        Ok(installed)
    }
}

fn unfix_goldberg(game_path: &Path, app_id: u64) -> Result<GameFixResult, String> {
    let files = files_for_types(game_path, app_id, &["Goldberg"]);
    if files.is_empty() {
        let mut bak_files = Vec::new();
        find_goldberg_bak_recursive(game_path, &mut bak_files);
        if bak_files.is_empty() {
            return Ok(GameFixResult {
                ok: false,
                tool: "goldberg".into(),
                message: "No Goldberg fix found to revert".into(),
                files_installed: vec![],
                errors: vec![],
            });
        }
        let total = bak_files.len();
        let mut restored = 0;
        let mut errors = Vec::new();
        for bak in &bak_files {
            match restore_bak(bak) {
                Ok(()) => restored += 1,
                Err(e) => errors.push(e),
            }
        }
        rewrite_log_without(game_path, app_id, &["Goldberg"]);
        return Ok(GameFixResult {
            ok: errors.is_empty(),
            tool: "goldberg".into(),
            message: format!("Restored {restored}/{total} file(s)"),
            files_installed: vec![],
            errors,
        });
    }

    let game_exe_dir = find_game_exe_dir(game_path);
    let total = files.len();
    let mut removed = 0;
    for file_name in &files {
        let lower = file_name.to_lowercase();
        let mut file_path = game_exe_dir.join(file_name);
        if !file_path.exists() && (lower == "steam_api64.dll" || lower == "steam_api.dll") {
            if let Some(found) = find_file_recursive_bounded(game_path, &lower, FIND_DLL_DEPTH) {
                file_path = found;
            }
        }
        if !file_path.exists() {
            let alt = game_path.join(file_name);
            if alt.exists() {
                file_path = alt;
            }
        }
        if file_path.exists() {
            let _ = std::fs::remove_file(&file_path);
        }
        let bak_path = PathBuf::from(format!("{}.bak", file_path.to_string_lossy()));
        if bak_path.exists() {
            let _ = restore_bak(&bak_path);
        }
        removed += 1;
    }
    rewrite_log_without(game_path, app_id, &["Goldberg"]);
    Ok(GameFixResult {
        ok: true,
        tool: "goldberg".into(),
        message: format!("Removed {removed}/{total} Goldberg file(s)"),
        files_installed: vec![],
        errors: vec![],
    })
}

// ---------------------------------------------------------------------------
// Online Fix — perondepot scrape + fuzzy match (phase 2, included here)
// ---------------------------------------------------------------------------

const ONLINE_FIX_URL: &str = "https://api.perondepot.xyz/all/";
const ONLINE_FIX_RAR_PASSWORD: &str = "online-fix.me";
const ONLINE_FIX_CACHE_TTL_SECS: u64 = 86400;
const ONLINE_FIX_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/137.0.0.0 Safari/537.36";
const MAX_MANUAL_FILES: usize = 10;

#[derive(Debug, Clone)]
struct OnlineFixEntry {
    url: String,
    display_name: String,
}

struct OnlineFixCache {
    entries: Vec<OnlineFixEntry>,
    cached_at: SystemTime,
}

static ONLINE_FIX_CACHE: OnceLock<Mutex<OnlineFixCache>> = OnceLock::new();

fn online_fix_cache() -> &'static Mutex<OnlineFixCache> {
    ONLINE_FIX_CACHE.get_or_init(|| {
        Mutex::new(OnlineFixCache {
            entries: Vec::new(),
            cached_at: SystemTime::UNIX_EPOCH,
        })
    })
}

fn encode_non_ascii_href(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut output = String::with_capacity(input.len() * 2);
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && bytes[i + 1].is_ascii_hexdigit()
            && bytes[i + 2].is_ascii_hexdigit()
        {
            output.push_str(&input[i..i + 3]);
            i += 3;
        } else if bytes[i] < 128 {
            output.push(bytes[i] as char);
            i += 1;
        } else {
            output.push_str(&format!("%{:02X}", bytes[i]));
            i += 1;
        }
    }
    output
}

fn fetch_online_fix_directory() -> Result<Vec<OnlineFixEntry>, String> {
    {
        let cache = online_fix_cache().lock().map_err(|e| e.to_string())?;
        let age = cache.cached_at.elapsed().unwrap_or_default();
        if age.as_secs() < ONLINE_FIX_CACHE_TTL_SECS && !cache.entries.is_empty() {
            return Ok(cache.entries.clone());
        }
    }

    let client = reqwest::blocking::Client::builder()
        .user_agent(ONLINE_FIX_USER_AGENT)
        .timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS))
        .build()
        .map_err(|e| format!("perondepot client: {e}"))?;

    let resp = client
        .get(ONLINE_FIX_URL)
        .send()
        .map_err(|e| format!("Failed to connect to online-fix: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("perondepot returned HTTP {}", resp.status()));
    }
    let html = resp.text().map_err(|e| format!("read response: {e}"))?;

    // Parse <a href="..."> without a full HTML crate
    let mut entries = Vec::new();
    let lower = html.to_lowercase();
    let mut search_from = 0usize;
    while let Some(href_pos) = lower[search_from..].find("href=\"") {
        let start = search_from + href_pos + 6;
        let Some(end_rel) = lower[start..].find('"') else {
            break;
        };
        let href = &html[start..start + end_rel];
        search_from = start + end_rel + 1;
        if !href.ends_with(".rar") && !href.to_lowercase().ends_with(".rar") {
            // also check nearby text for .rar — skip for simplicity if href not rar
            if !href.contains(".rar") {
                continue;
            }
        }
        let url = if href.starts_with("http") {
            encode_non_ascii_href(href)
        } else {
            format!(
                "https://api.perondepot.xyz/all/{}",
                encode_non_ascii_href(href.trim_start_matches('/'))
            )
        };
        let filename = href.rsplit('/').next().unwrap_or(href);
        let decoded = percent_decode(filename);
        let display_name = decoded
            .strip_suffix(".rar")
            .unwrap_or(&decoded)
            .to_string();
        entries.push(OnlineFixEntry {
            url,
            display_name,
        });
    }

    {
        let mut cache = online_fix_cache().lock().map_err(|e| e.to_string())?;
        cache.entries = entries.clone();
        cache.cached_at = SystemTime::now();
    }
    Ok(entries)
}

fn percent_decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v as char);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

fn normalize_text(text: &str) -> String {
    let cleaned = text.replace("по сети", "").replace("  ", " ").trim().to_lowercase();
    let normalized = cleaned
        .replace(['.', '_', '-'], " ")
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect::<String>();
    normalized.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn extract_app_id_from_filename(display_name: &str) -> Option<u64> {
    let without_ext = display_name.strip_suffix(".rar").unwrap_or(display_name);
    if let Some(end) = without_ext.find(']') {
        if without_ext.starts_with('[') && end > 1 {
            let digits = &without_ext[1..end];
            if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
                return digits.parse::<u64>().ok();
            }
        }
    }
    None
}

fn extract_game_name_from_filename(display_name: &str) -> String {
    let without_ext = display_name.strip_suffix(".rar").unwrap_or(display_name);
    if let Some(end) = without_ext.find(']') {
        if without_ext.starts_with('[') && end > 1 {
            let rest = &without_ext[end + 1..];
            let rest_trimmed = rest.trim_start_matches('_').trim_start();
            if !rest_trimmed.is_empty() {
                return rest_trimmed.to_string();
            }
        }
    }
    if let Some(pos) = without_ext.find('_') {
        let prefix = without_ext[..pos].trim().to_string();
        let lower = prefix.to_lowercase();
        if lower == "fix" || lower == "crack" || lower == "update" || lower == "patch" {
            return without_ext.trim().to_string();
        }
        return prefix;
    }
    without_ext.trim().to_string()
}

fn fuzzy_match_game(
    game_name: &str,
    files: &[OnlineFixEntry],
    app_id: Option<u64>,
) -> Option<String> {
    // Step 0: exact app ID match in [appid]_Name patterns
    if let Some(id) = app_id {
        for file in files {
            if extract_app_id_from_filename(&file.display_name) == Some(id) {
                return Some(file.url.clone());
            }
        }
        let has_brackets = files
            .iter()
            .any(|f| extract_app_id_from_filename(&f.display_name).is_some());
        if has_brackets {
            return None;
        }
    }

    let normalized_game = normalize_text(game_name);
    let game_words: Vec<String> = normalized_game
        .split_whitespace()
        .filter(|w| w.len() > 1)
        .map(String::from)
        .collect();
    let abbreviation: String = game_words.iter().filter_map(|w| w.chars().next()).collect();

    let mut scored: Vec<(usize, &OnlineFixEntry)> = files
        .iter()
        .enumerate()
        .map(|(idx, file)| {
            let file_game_name = extract_game_name_from_filename(&file.display_name);
            let normalized_file = normalize_text(&file_game_name);
            if normalized_file == normalized_game {
                return (0, file);
            }
            if normalized_file.contains(&normalized_game)
                || normalized_game.contains(&normalized_file)
            {
                return (1, file);
            }
            let file_words: Vec<String> = normalized_file
                .split_whitespace()
                .filter(|w| w.len() > 1)
                .map(String::from)
                .collect();
            let common = game_words
                .iter()
                .filter(|gw| file_words.iter().any(|fw| fw == *gw))
                .count();
            if common >= 2 {
                return (2, file);
            }
            let compact: String = normalized_file.replace(' ', "");
            if compact.contains(&abbreviation) || abbreviation.contains(&compact) {
                return (3, file);
            }
            (999 + idx, file)
        })
        .collect();

    scored.sort_by_key(|(s, _)| *s);
    scored
        .first()
        .filter(|(s, _)| *s < 100)
        .map(|(_, f)| f.url.clone())
}

fn apply_online_fix(
    game_path: &Path,
    app_id: u64,
    game_name: &str,
    manual_file: Option<&str>,
) -> Result<Vec<String>, String> {
    let client = reqwest::blocking::Client::builder()
        .user_agent(ONLINE_FIX_USER_AGENT)
        .timeout(Duration::from_secs(DOWNLOAD_TIMEOUT_SECS))
        .build()
        .map_err(|e| format!("HTTP client: {e}"))?;

    let url = if let Some(file_name) = manual_file {
        if file_name.starts_with("http") {
            file_name.to_string()
        } else {
            format!("https://api.perondepot.xyz/{}", file_name)
        }
    } else {
        let files = fetch_online_fix_directory()?;
        fuzzy_match_game(game_name, &files, Some(app_id))
            .ok_or_else(|| format!("No online-fix match found for '{game_name}' (app {app_id})"))?
    };

    download_and_extract_rar(&client, &url, game_path)
}

fn find_rar_extractor() -> Option<RarExtractor> {
    // unar
    if std::process::Command::new("unar")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
    {
        return Some(RarExtractor::Unar);
    }
    #[cfg(target_os = "windows")]
    {
        if let Some(path) = find_winrar_path() {
            return Some(RarExtractor::WinRar(path));
        }
        for p in [
            r"C:\Program Files\7-Zip\7z.exe",
            r"C:\Program Files (x86)\7-Zip\7z.exe",
        ] {
            if Path::new(p).exists() {
                return Some(RarExtractor::SevenZip(p.to_string()));
            }
        }
        if let Ok(p) = which("7z") {
            return Some(RarExtractor::SevenZip(p));
        }
    }
    None
}

#[cfg(target_os = "windows")]
fn find_winrar_path() -> Option<String> {
    for base in [r"C:\Program Files\WinRAR", r"C:\Program Files (x86)\WinRAR"] {
        let p = PathBuf::from(base).join("UnRAR.exe");
        if p.exists() {
            return Some(p.to_string_lossy().into_owned());
        }
    }
    None
}

#[cfg(not(target_os = "windows"))]
fn find_winrar_path() -> Option<String> {
    None
}

fn which(name: &str) -> Result<String, String> {
    if let Ok(paths) = std::env::var("PATH") {
        for p in paths.split(';').chain(paths.split(':')) {
            let candidate = Path::new(p).join(name);
            if candidate.exists() {
                return Ok(candidate.to_string_lossy().to_string());
            }
            #[cfg(windows)]
            {
                let exe = Path::new(p).join(format!("{name}.exe"));
                if exe.exists() {
                    return Ok(exe.to_string_lossy().to_string());
                }
            }
        }
    }
    Err(format!("{name} not found in PATH"))
}

enum RarExtractor {
    Unar,
    WinRar(String),
    SevenZip(String),
}

fn download_and_extract_rar(
    _client: &reqwest::blocking::Client,
    url: &str,
    game_path: &Path,
) -> Result<Vec<String>, String> {
    let extractor =
        find_rar_extractor().ok_or_else(|| {
            "No RAR extractor found. Install WinRAR, unar or 7-Zip.".to_string()
        })?;

    let temp_dir = temp_fix_dir().join(format!(
        "onlinefix_{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
    ));
    let _ = std::fs::create_dir_all(&temp_dir);
    let rar_path = temp_dir.join("download.rar");
    download_file(url, &rar_path)?;

    let meta = std::fs::metadata(&rar_path).map_err(|e| e.to_string())?;
    if meta.len() < 1024 {
        let _ = std::fs::remove_dir_all(&temp_dir);
        return Err(format!("Downloaded file too small ({} bytes)", meta.len()));
    }

    let extract_dir = temp_dir.join("extracted");
    let _ = std::fs::create_dir_all(&extract_dir);

    let output = match &extractor {
        RarExtractor::Unar => {
            let mut cmd = std::process::Command::new("unar");
            cmd.args([
                "-p",
                ONLINE_FIX_RAR_PASSWORD,
                "-o",
                extract_dir.to_str().unwrap_or_default(),
                rar_path.to_str().unwrap_or_default(),
            ]);
            hide_window(&mut cmd);
            cmd.output().map_err(|e| format!("run unar: {e}"))?
        }
        RarExtractor::WinRar(exe) => {
            let password_arg = format!("-p{}", ONLINE_FIX_RAR_PASSWORD);
            let mut cmd = std::process::Command::new(exe);
            cmd.args([
                "x",
                &password_arg,
                rar_path.to_str().unwrap_or_default(),
                extract_dir.to_str().unwrap_or_default(),
            ]);
            hide_window(&mut cmd);
            cmd.output().map_err(|e| format!("run UnRAR: {e}"))?
        }
        RarExtractor::SevenZip(exe) => {
            let password_arg = format!("-p{}", ONLINE_FIX_RAR_PASSWORD);
            let output_arg = format!("-o{}", extract_dir.to_str().unwrap_or_default());
            let mut cmd = std::process::Command::new(exe);
            cmd.args([
                "x",
                &password_arg,
                &output_arg,
                rar_path.to_str().unwrap_or_default(),
                "-y",
            ]);
            hide_window(&mut cmd);
            cmd.output().map_err(|e| format!("run 7z: {e}"))?
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let _ = std::fs::remove_dir_all(&temp_dir);
        return Err(format!(
            "RAR extraction failed:\nstdout: {stdout}\nstderr: {stderr}"
        ));
    }

    let mut installed = Vec::new();
    let effective_src = flatten_extracted_dir(&extract_dir).unwrap_or(extract_dir);
    let result = copy_dir_with_backup(&effective_src, game_path, game_path, &mut installed);
    let _ = std::fs::remove_dir_all(&temp_dir);
    result?;
    Ok(installed)
}

fn unfix_online(game_path: &Path, app_id: u64) -> Result<GameFixResult, String> {
    let files = files_for_types(game_path, app_id, &["OnlineFix", "online_fix"]);
    if files.is_empty() {
        return Ok(GameFixResult {
            ok: false,
            tool: "online_fix".into(),
            message: "No Online-Fix entries found in log.".into(),
            files_installed: vec![],
            errors: vec![],
        });
    }
    let total = files.len();
    let mut removed = 0;
    for file_name in &files {
        let file_path = game_path.join(file_name);
        if file_path.exists() {
            let _ = std::fs::remove_file(&file_path);
        }
        let bak_path = PathBuf::from(format!("{}.bak", file_path.to_string_lossy()));
        if bak_path.exists() {
            let _ = restore_bak(&bak_path);
        }
        removed += 1;
    }
    clean_empty_dirs_recursive(game_path);
    rewrite_log_without(game_path, app_id, &["OnlineFix", "online_fix"]);
    Ok(GameFixResult {
        ok: true,
        tool: "online_fix".into(),
        message: format!("Removed {removed}/{total} Online-Fix file(s)"),
        files_installed: vec![],
        errors: vec![],
    })
}

// ---------------------------------------------------------------------------
// Catalog fixes (Rockstar / Voices38) — phase 3
// ---------------------------------------------------------------------------

fn catalog_key() -> String {
    if let Ok(k) = std::env::var("LUMAFORGE_FIXES_KEY") {
        if !k.is_empty() {
            return k;
        }
    }
    // config.json: fixes.catalogKey (outside the repo)
    if let Ok(lad) = std::env::var("LOCALAPPDATA") {
        let path = PathBuf::from(lad).join("LumaForge").join("config.json");
        if let Ok(raw) = std::fs::read_to_string(&path) {
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&raw) {
                if let Some(k) = val
                    .get("fixes")
                    .and_then(|f| f.get("catalogKey"))
                    .and_then(|v| v.as_str())
                {
                    return k.to_string();
                }
            }
        }
    }
    String::new()
}

struct CatalogCache {
    body: Option<String>,
    cached_at: SystemTime,
}

static CATALOG_CACHE: OnceLock<Mutex<CatalogCache>> = OnceLock::new();

fn catalog_cache() -> &'static Mutex<CatalogCache> {
    CATALOG_CACHE.get_or_init(|| {
        Mutex::new(CatalogCache {
            body: None,
            cached_at: SystemTime::UNIX_EPOCH,
        })
    })
}

fn fetch_catalog() -> Result<String, String> {
    {
        let cache = catalog_cache().lock().map_err(|e| e.to_string())?;
        let age = cache.cached_at.elapsed().unwrap_or_default();
        if age.as_secs() < CATALOG_CACHE_TTL_SECS {
            if let Some(body) = &cache.body {
                return Ok(body.clone());
            }
        }
    }
    let key = catalog_key();
    if key.is_empty() {
        return Err(
            "Fixes catalog key missing. Set fixes.catalogKey in config.json.".to_string(),
        );
    }
    let client = http_client(Duration::from_secs(CONNECT_TIMEOUT_SECS))?;
    let resp = client
        .get(CATALOG_URL)
        .header("X-LumaForge-Key", &key)
        .send()
        .map_err(|e| format!("catalog fetch: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("catalog HTTP {}", resp.status()));
    }
    let body = resp.text().map_err(|e| format!("catalog read: {e}"))?;
    {
        let mut cache = catalog_cache().lock().map_err(|e| e.to_string())?;
        cache.body = Some(body.clone());
        cache.cached_at = SystemTime::now();
    }
    Ok(body)
}

fn patch_configs_user_ini(game_path: &Path, account_name: &str, steam_id: &str) {
    let mut ini_files = Vec::new();
    find_configs_user_ini_recursive(game_path, &mut ini_files);
    for ini_path in &ini_files {
        let Ok(content) = std::fs::read_to_string(ini_path) else {
            continue;
        };
        let patched = content
            .replace(
                "account_name=voices38",
                &format!("account_name={account_name}"),
            )
            .replace(
                "account_steamid=76561197960285355",
                &format!("account_steamid={steam_id}"),
            );
        if patched != content {
            let _ = std::fs::write(ini_path, patched);
        }
    }
}

fn download_and_extract_catalog_archive(
    url: &str,
    game_path: &Path,
) -> Result<Vec<String>, String> {
    let temp_dir = temp_fix_dir().join(format!(
        "catalog_{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
    ));
    let _ = std::fs::create_dir_all(&temp_dir);

    let url_lower = url.to_lowercase();
    let is_rar = url_lower.ends_with(".rar") || url_lower.contains(".rar?");
    let archive_name = if is_rar { "download.rar" } else { "download.zip" };
    let archive_path = temp_dir.join(archive_name);
    download_file(url, &archive_path)?;

    let meta = std::fs::metadata(&archive_path).map_err(|e| e.to_string())?;
    if meta.len() < 1024 {
        let _ = std::fs::remove_dir_all(&temp_dir);
        return Err(format!(
            "Downloaded file too small ({} bytes)",
            meta.len()
        ));
    }

    let extract_dir = temp_dir.join("extracted");
    let _ = std::fs::create_dir_all(&extract_dir);

    let mut extracted = false;
    if let Ok(file) = std::fs::File::open(&archive_path) {
        if let Ok(mut archive) = zip::ZipArchive::new(file) {
            extracted = archive.extract(&extract_dir).is_ok();
        }
    }

    if !extracted {
        let Some(extractor) = find_rar_extractor() else {
            let _ = std::fs::remove_dir_all(&temp_dir);
            return Err("No extractor found for catalog archive".into());
        };
        let output = match &extractor {
            RarExtractor::Unar => {
                let mut cmd = std::process::Command::new("unar");
                cmd.args([
                    "-o",
                    extract_dir.to_str().unwrap_or_default(),
                    archive_path.to_str().unwrap_or_default(),
                ]);
                hide_window(&mut cmd);
                cmd.output().map_err(|e| format!("unar: {e}"))?
            }
            RarExtractor::WinRar(exe) => {
                let mut cmd = std::process::Command::new(exe);
                cmd.args([
                    "x",
                    archive_path.to_str().unwrap_or_default(),
                    extract_dir.to_str().unwrap_or_default(),
                ]);
                hide_window(&mut cmd);
                cmd.output().map_err(|e| format!("WinRAR: {e}"))?
            }
            RarExtractor::SevenZip(exe) => {
                let output_arg = format!("-o{}", extract_dir.to_str().unwrap_or_default());
                let mut cmd = std::process::Command::new(exe);
                cmd.args([
                    "x",
                    &output_arg,
                    archive_path.to_str().unwrap_or_default(),
                    "-y",
                ]);
                hide_window(&mut cmd);
                cmd.output().map_err(|e| format!("7z: {e}"))?
            }
        };
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let _ = std::fs::remove_dir_all(&temp_dir);
            return Err(format!("Archive extraction failed: {stderr}"));
        }
    }

    let mut installed = Vec::new();
    let effective_src = flatten_extracted_dir(&extract_dir).unwrap_or(extract_dir);
    let result = copy_dir_with_backup(&effective_src, game_path, game_path, &mut installed);
    let _ = std::fs::remove_dir_all(&temp_dir);
    result?;
    Ok(installed)
}

fn apply_catalog(
    game_path: &Path,
    app_id: u64,
    game_name: &str,
    download_url: &str,
    fix_type: &str,
    account_name: Option<&str>,
    steam_id: Option<&str>,
) -> Result<Vec<String>, String> {
    let installed = download_and_extract_catalog_archive(download_url, game_path)?;
    if let (Some(acct), Some(sid)) = (account_name, steam_id) {
        if !acct.is_empty() && !sid.is_empty() {
            patch_configs_user_ini(game_path, acct, sid);
        }
    }
    write_fix_log(game_path, app_id, game_name, fix_type, &installed)?;
    Ok(installed)
}

fn unfix_by_type(
    game_path: &Path,
    app_id: u64,
    fix_type: &str,
) -> Result<GameFixResult, String> {
    let files = files_for_types(game_path, app_id, &[fix_type]);
    if files.is_empty() {
        return Ok(GameFixResult {
            ok: false,
            tool: fix_type.into(),
            message: format!("No {fix_type} entries found in log."),
            files_installed: vec![],
            errors: vec![],
        });
    }
    let total = files.len();
    let mut removed = 0;
    for file_name in &files {
        let file_path = game_path.join(file_name);
        if file_path.exists() {
            let _ = std::fs::remove_file(&file_path);
        }
        let bak_path = PathBuf::from(format!("{}.bak", file_path.to_string_lossy()));
        if bak_path.exists() {
            let _ = restore_bak(&bak_path);
        }
        removed += 1;
    }
    clean_empty_dirs_recursive(game_path);
    rewrite_log_without(game_path, app_id, &[fix_type]);
    Ok(GameFixResult {
        ok: true,
        tool: fix_type.into(),
        message: format!("Removed {removed}/{total} {fix_type} file(s)"),
        files_installed: vec![],
        errors: vec![],
    })
}

// ---------------------------------------------------------------------------
// GitHub download for auto-install (Koaloader etc.)
// ---------------------------------------------------------------------------

fn install_tool_from_github(
    owner: &str,
    repo: &str,
    target_dir: &Path,
) -> Result<Vec<String>, String> {
    let client = http_client(Duration::from_secs(DOWNLOAD_TIMEOUT_SECS))?;
    let url = format!("https://api.github.com/repos/{owner}/{repo}/releases/latest");
    let resp = client
        .get(&url)
        .header("Accept", "application/vnd.github.v3+json")
        .send()
        .map_err(|e| format!("GitHub release: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("GitHub API HTTP {}", resp.status()));
    }
    let release: serde_json::Value =
        resp.json().map_err(|e| format!("parse release: {e}"))?;
    let assets = release["assets"]
        .as_array()
        .ok_or_else(|| "No assets in release".to_string())?;
    let asset = assets
        .iter()
        .find(|a| {
            a["name"]
                .as_str()
                .is_some_and(|n| n.ends_with(".zip") || n.ends_with(".7z"))
        })
        .or_else(|| assets.first())
        .ok_or_else(|| "No downloadable asset".to_string())?;
    let zip_url = asset["browser_download_url"]
        .as_str()
        .ok_or_else(|| "No download URL".to_string())?;
    let zip_name = asset["name"].as_str().unwrap_or("release.zip");

    let temp = temp_fix_dir().join(format!(
        "tool_{}_{zip_name}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
    ));
    let _ = std::fs::create_dir_all(&temp);
    let archive_path = temp.join(zip_name);
    download_file(zip_url, &archive_path)?;

    let extract_dir = temp.join("extracted");
    let _ = std::fs::create_dir_all(&extract_dir);
    let ext = Path::new(zip_name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("zip");
    crate::thirdparty::extract_archive(&archive_path, ext, &extract_dir)?;

    std::fs::create_dir_all(target_dir).map_err(|e| e.to_string())?;
    let mut installed = Vec::new();
    let effective = flatten_extracted_dir(&extract_dir).unwrap_or(extract_dir);
    copy_dir_with_backup(&effective, target_dir, target_dir, &mut installed)?;
    let _ = std::fs::remove_dir_all(&temp);
    Ok(installed)
}

// ---------------------------------------------------------------------------
// Status / info handlers
// ---------------------------------------------------------------------------

fn tool_install_status() -> FixInstallationStatus {
    FixInstallationStatus {
        smoke_api_installed: is_dir_populated(&thirdparty_tool_dir("smokeapi")),
        steamless_installed: is_dir_populated(&thirdparty_tool_dir("steamless")),
        koaloader_installed: is_dir_populated(&thirdparty_tool_dir("koaloader")),
        goldberg_installed: is_dir_populated(&thirdparty_tool_dir("goldberg_fork")),
    }
}

fn handle_info(app_id: u64) -> (u16, String) {
    let resolved = resolve_game_path(app_id);
    let installed = resolved.is_some();
    let (game_path, name) = match &resolved {
        Some((p, n)) => (Some(p.clone()), n.clone()),
        None => (None, format!("App {app_id}")),
    };

    let (arch, has_64, has_32) = match &game_path {
        Some(p) => detect_architecture(p),
        None => (None, false, false),
    };
    let main_exe_path = game_path.as_ref().and_then(|p| find_main_exe(p));
    let main_exe = main_exe_path.as_ref().map(|exe| {
        exe.file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string()
    });
    let exe_name = main_exe_path
        .as_ref()
        .map(|exe| exe.to_string_lossy().to_string());
    let has_steam_stub_drm = game_path
        .as_ref()
        .map(|p| find_candidate_exes(p).iter().any(|exe| has_steamstub_drm(exe)))
        .unwrap_or(false);

    // has_online_fix: cheap check — try cache only first; full fetch on demand
    let has_online_fix = {
        let cache = online_fix_cache()
            .lock()
            .map(|c| {
                let age = c.cached_at.elapsed().unwrap_or_default();
                age.as_secs() < ONLINE_FIX_CACHE_TTL_SECS && !c.entries.is_empty()
            })
            .unwrap_or(false);
        if cache {
            online_fix_cache()
                .lock()
                .ok()
                .map(|c| fuzzy_match_game(&name, &c.entries, Some(app_id)).is_some())
                .unwrap_or(false)
        } else {
            // Best-effort: try live fetch (may take a moment on first open)
            fetch_online_fix_directory()
                .map(|files| fuzzy_match_game(&name, &files, Some(app_id)).is_some())
                .unwrap_or(false)
        }
    };

    let info = GameFixInfo {
        app_id,
        name,
        installed,
        install_path: game_path.map(|p| p.to_string_lossy().to_string()),
        has_online_fix,
        has_steam_api_64: has_64,
        has_steam_api_32: has_32,
        game_arch: arch,
        main_exe,
        has_steam_stub_drm,
        exe_name,
    };
    (200, json!({"ok": true, "info": info}).to_string())
}

fn handle_status(app_id: u64) -> (u16, String) {
    let tools = tool_install_status();
    let resolved = resolve_game_path(app_id);

    let applied = json!({
        "smokeApi": resolved.as_ref().map(|(p, _)| has_fix_in_log(p, app_id, &["SmokeAPI", "smoke_api"])).unwrap_or(false),
        "steamless": resolved.as_ref().map(|(p, _)| has_fix_in_log(p, app_id, &["Steamless"])).unwrap_or(false),
        "goldberg": resolved.as_ref().map(|(p, _)| has_fix_in_log(p, app_id, &["Goldberg"])).unwrap_or(false),
        "onlineFix": resolved.as_ref().map(|(p, _)| has_fix_in_log(p, app_id, &["OnlineFix", "online_fix"])).unwrap_or(false),
        "RockstarFix": resolved.as_ref().map(|(p, _)| has_fix_in_log(p, app_id, &["RockstarFix"])).unwrap_or(false),
        "Voices38Fix": resolved.as_ref().map(|(p, _)| has_fix_in_log(p, app_id, &["Voices38Fix"])).unwrap_or(false),
    });

    // Jobs for this app
    let mut app_jobs = json!({});
    if let Ok(map) = jobs().read() {
        for (key, job) in map.iter() {
            if key.starts_with(&format!("{app_id}:")) {
                app_jobs[key.split_once(':').map(|(_, t)| t.to_string()).unwrap_or_default()] = json!({
                    "tool": job.tool,
                    "op": job.op,
                    "status": job.status,
                    "progress": job.progress,
                    "message": job.message,
                    "result": job.result,
                });
            }
        }
    }

    (
        200,
        json!({
            "ok": true,
            "appId": app_id,
            "tools": tools,
            "applied": applied,
            "jobs": app_jobs,
        })
        .to_string(),
    )
}

fn handle_applied() -> (u16, String) {
    let mut applied = Vec::new();
    if let Some(steam_root) = crate::depot_downloader::steam_root() {
        let steamapps = steam_root.join("steamapps");
        if let Ok(entries) = std::fs::read_dir(&steamapps) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if let Some(id_str) = name
                    .strip_prefix("appmanifest_")
                    .and_then(|s| s.strip_suffix(".acf"))
                {
                    if let Ok(app_id) = id_str.parse::<u64>() {
                        if let Some((path, _)) = resolve_game_path(app_id) {
                            let log = fix_log_path(&path, app_id);
                            if log.exists() {
                                applied.push(app_id);
                            } else {
                                // legacy: any bak
                                let mut baks = Vec::new();
                                find_bak_files_recursive(&path, &mut baks);
                                if !baks.is_empty() {
                                    applied.push(app_id);
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    (200, json!({"ok": true, "appIds": applied}).to_string())
}

// ---------------------------------------------------------------------------
// Apply / unfix (job-based)
// ---------------------------------------------------------------------------

fn start_apply(app_id: u64, body: &str) -> (u16, String) {
    let req: ApplyRequest = match serde_json::from_str(body) {
        Ok(r) => r,
        Err(e) => {
            return (
                400,
                json!({"ok": false, "message": format!("Invalid JSON: {e}")}).to_string(),
            )
        }
    };
    let tool = req.tool.clone();
    let key = job_key(app_id, &tool);
    if job_is_running(&key) {
        return (
            200,
            json!({"ok": false, "message": "A job for this tool is already running"}).to_string(),
        );
    }

    set_job(
        &key,
        FixJob {
            app_id: app_id.to_string(),
            tool: tool.clone(),
            op: "apply".into(),
            status: "running".into(),
            progress: 5,
            message: "Starting…".into(),
            result: None,
        },
    );

    let req_clone = req.clone();
    let key_for_thread = key.clone();
    std::thread::spawn(move || {
        let result = run_apply_job(app_id, &tool, &req_clone);
        match result {
            Ok(files) => {
                let msg = if files.is_empty() {
                    format!("{tool} applied successfully")
                } else {
                    format!(
                        "{tool} applied. {} file(s) installed.",
                        files.len()
                    )
                };
                update_job(&key_for_thread, |j| {
                    j.status = "done".into();
                    j.progress = 100;
                    j.message = msg.clone();
                    j.result = Some(GameFixResult {
                        ok: true,
                        tool: tool.clone(),
                        message: msg,
                        files_installed: files,
                        errors: vec![],
                    });
                });
                crate::log_to_temp(&format!("[fixes] {app_id} apply {tool}: OK"));
            }
            Err(e) => {
                update_job(&key_for_thread, |j| {
                    j.status = "error".into();
                    j.progress = 100;
                    j.message = e.clone();
                    j.result = Some(GameFixResult {
                        ok: false,
                        tool: tool.clone(),
                        message: e.clone(),
                        files_installed: vec![],
                        errors: vec![e.clone()],
                    });
                });
                crate::log_to_temp(&format!("[fixes] {app_id} apply {tool}: ERROR {e}"));
            }
        }
    });

    (
        200,
        json!({"ok": true, "message": "Starting apply...", "jobKey": key}).to_string(),
    )
}

fn run_apply_job(
    app_id: u64,
    tool: &str,
    req: &ApplyRequest,
) -> Result<Vec<String>, String> {
    let (game_path, game_name) = resolve_required(app_id)?;
    let key = job_key(app_id, tool);

    let progress = |p: u8, msg: &str| {
        update_job(&key, |j| {
            j.progress = p;
            j.message = msg.to_string();
        });
    };

    let files = match tool {
        "smoke_api" | "smokeapi" => {
            progress(30, "Analyzing game executable…");
            let files = apply_smoke_api(&game_path)?;
            progress(90, "Writing fix log…");
            write_fix_log(&game_path, app_id, &game_name, "SmokeAPI", &files)?;
            files
        }
        "steamless" => {
            progress(30, "Applying Steamless…");
            let files = apply_steamless(&game_path)?;
            if !files.is_empty() {
                progress(90, "Writing fix log…");
                write_fix_log(&game_path, app_id, &game_name, "Steamless", &files)?;
            }
            files
        }
        "goldberg" => {
            progress(20, "Applying Goldberg emulator…");
            let files = apply_goldberg(&game_path, app_id)?;
            progress(90, "Writing fix log…");
            write_fix_log(&game_path, app_id, &game_name, "Goldberg", &files)?;
            files
        }
        "online_fix" | "onlinefix" => {
            progress(15, "Fetching online-fix directory…");
            let files = apply_online_fix(
                &game_path,
                app_id,
                &game_name,
                req.manual_file.as_deref(),
            )?;
            progress(90, "Writing fix log…");
            write_fix_log(&game_path, app_id, &game_name, "OnlineFix", &files)?;
            files
        }
        "catalog" => {
            let download_url = req
                .download_url
                .as_deref()
                .ok_or_else(|| "downloadUrl required for catalog fix".to_string())?;
            let fix_type = req
                .fix_type
                .clone()
                .unwrap_or_else(|| "Voices38Fix".to_string());
            progress(15, "Downloading catalog fix…");
            let files = apply_catalog(
                &game_path,
                app_id,
                &game_name,
                download_url,
                &fix_type,
                req.account_name.as_deref(),
                req.steam_id.as_deref(),
            )?;
            files
        }
        other => return Err(format!("Unknown fix tool: {other}")),
    };
    Ok(files)
}

fn handle_unfix(app_id: u64, body: &str) -> (u16, String) {
    let req: UnfixRequest = match serde_json::from_str(body) {
        Ok(r) => r,
        Err(e) => {
            return (
                400,
                json!({"ok": false, "message": format!("Invalid JSON: {e}")}).to_string(),
            )
        }
    };
    let game_path = match resolve_game_path(app_id) {
        Some((p, _)) => p,
        None => {
            return (
                200,
                json!({"ok": false, "message": format!("Game {app_id} is not installed")})
                    .to_string(),
            )
        }
    };

    let tool = req.tool.as_str();
    let result = match tool {
        "smoke_api" | "smokeapi" => unfix_smoke_api(&game_path, app_id),
        "steamless" => unfix_steamless(&game_path, app_id),
        "goldberg" => unfix_goldberg(&game_path, app_id),
        "online_fix" | "onlinefix" => unfix_online(&game_path, app_id),
        "catalog" => {
            let fix_type = req
                .fix_type
                .clone()
                .unwrap_or_else(|| "Voices38Fix".to_string());
            unfix_by_type(&game_path, app_id, &fix_type)
        }
        other => Err(format!("Unknown fix tool: {other}")),
    };

    match result {
        Ok(r) => (200, json!({"ok": r.ok, "result": r}).to_string()),
        Err(e) => (
            200,
            json!({"ok": false, "message": e}).to_string(),
        ),
    }
}

// ---------------------------------------------------------------------------
// Route handler
// ---------------------------------------------------------------------------

pub fn try_handle_route(method: &str, path: &str, body: &str) -> Option<(u16, String)> {
    if !path.starts_with("/api/fixes") {
        return None;
    }

    // GET /api/fixes/applied
    if path == "/api/fixes/applied" && method == "GET" {
        return Some(handle_applied());
    }

    // GET /api/fixes/catalog
    if path == "/api/fixes/catalog" && method == "GET" {
        return Some(match fetch_catalog() {
            Ok(body) => (200, body),
            Err(e) => (
                200,
                json!({"ok": false, "message": e, "entries": []}).to_string(),
            ),
        });
    }

    // /api/fixes/{appId}/{action}
    let rest = path.trim_start_matches("/api/fixes/");
    let mut parts = rest.splitn(2, '/');
    let id_str = parts.next().unwrap_or("");
    let action = parts.next().unwrap_or("");
    let Ok(app_id) = id_str.parse::<u64>() else {
        return Some((
            400,
            json!({"ok": false, "message": "Invalid appId"}).to_string(),
        ));
    };

    Some(match (method, action) {
        ("GET", "info") => handle_info(app_id),
        ("GET", "status") => handle_status(app_id),
        ("POST", "apply") => start_apply(app_id, body),
        ("POST", "unfix") => handle_unfix(app_id, body),
        _ => (
            404,
            json!({"ok": false, "message": format!("Unknown fixes route: {method} {path}")})
                .to_string(),
        ),
    })
}
