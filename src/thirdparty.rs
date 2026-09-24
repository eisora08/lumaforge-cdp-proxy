use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const GITHUB_API_BASE: &str = "https://api.github.com";
const CONNECT_TIMEOUT_SECS: u64 = 15;
const DOWNLOAD_TIMEOUT_SECS: u64 = 300;
const RELEASE_CACHE_TTL_SECS: u64 = 86400;
const RELEASE_ERROR_TTL_SECS: u64 = 300;
const APP_USER_AGENT: &str = concat!(
    env!("CARGO_PKG_NAME"),
    "/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/eisora08/lumaforge-cdp-proxy)"
);

// ---------------------------------------------------------------------------
// Tool definitions
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum ToolPlatform {
    Any,
    WindowsOnly,
    LinuxOnly,
}

impl ToolPlatform {
    fn as_str(self) -> &'static str {
        match self {
            ToolPlatform::Any => "any",
            ToolPlatform::WindowsOnly => "windows",
            ToolPlatform::LinuxOnly => "linux",
        }
    }

    fn available_here(self) -> bool {
        match self {
            ToolPlatform::Any => true,
            ToolPlatform::WindowsOnly => cfg!(target_os = "windows"),
            ToolPlatform::LinuxOnly => cfg!(target_os = "linux"),
        }
    }
}

struct ExtraRepo {
    owner: &'static str,
    repo: &'static str,
    preferred_asset: Option<&'static str>,
}

struct ToolDef {
    id: &'static str,
    name: &'static str,
    description: &'static str,
    github_owner: &'static str,
    github_repo: &'static str,
    preferred_asset: Option<&'static str>,
    preferred_asset_contains: Option<&'static str>,
    linux_github_owner: Option<&'static str>,
    linux_github_repo: Option<&'static str>,
    linux_preferred_asset: Option<&'static str>,
    extra_repos: Option<&'static [ExtraRepo]>,
    install_to_steam_root: bool,
    steam_dll_names: &'static [&'static str],
    platform: ToolPlatform,
}

impl ToolDef {
    fn resolve_github(&self) -> (&str, &str, Option<&str>, Option<&str>) {
        #[cfg(target_os = "linux")]
        if let Some(owner) = self.linux_github_owner {
            return (
                owner,
                self.linux_github_repo.unwrap_or(self.github_repo),
                self.linux_preferred_asset,
                self.preferred_asset_contains,
            );
        }
        (
            self.github_owner,
            self.github_repo,
            self.preferred_asset,
            self.preferred_asset_contains,
        )
    }
}

const TOOL_DEFS: &[ToolDef] = &[
    ToolDef {
        id: "smokeapi",
        name: "SmokeAPI",
        description: "Steam API proxy for offline Steam games",
        github_owner: "acidicoala",
        github_repo: "SmokeAPI",
        preferred_asset: None,
        preferred_asset_contains: None,
        linux_github_owner: None,
        linux_github_repo: None,
        linux_preferred_asset: None,
        extra_repos: None,
        install_to_steam_root: false,
        steam_dll_names: &[],
        platform: ToolPlatform::Any,
    },
    ToolDef {
        id: "steamless",
        name: "Steamless",
        description: "SteamStub DRM unpacker for game executables",
        github_owner: "atom0s",
        github_repo: "Steamless",
        preferred_asset: None,
        preferred_asset_contains: None,
        linux_github_owner: None,
        linux_github_repo: None,
        linux_preferred_asset: None,
        extra_repos: None,
        install_to_steam_root: false,
        steam_dll_names: &[],
        platform: ToolPlatform::Any,
    },
    ToolDef {
        id: "goldberg_fork",
        name: "Goldberg (fork)",
        description: "Goldberg Steam Emu fork by Detanup01 + config tools",
        github_owner: "Detanup01",
        github_repo: "gbe_fork",
        preferred_asset: Some("emu-win-release.7z"),
        preferred_asset_contains: None,
        linux_github_owner: None,
        linux_github_repo: None,
        linux_preferred_asset: None,
        extra_repos: Some(&[ExtraRepo {
            owner: "Detanup01",
            repo: "gbe_fork_tools",
            preferred_asset: None,
        }]),
        install_to_steam_root: false,
        steam_dll_names: &[],
        platform: ToolPlatform::Any,
    },
    ToolDef {
        id: "opensteamtool",
        name: "OpenSteamTool",
        description: "Open-source Steam unlocker with Lua scripting",
        github_owner: "eisora08",
        github_repo: "OpenSteamTool",
        preferred_asset: None,
        preferred_asset_contains: Some("Release"),
        linux_github_owner: None,
        linux_github_repo: None,
        linux_preferred_asset: None,
        extra_repos: None,
        install_to_steam_root: true,
        steam_dll_names: &["dwmapi.dll", "xinput1_4.dll", "OpenSteamTool.dll"],
        platform: ToolPlatform::WindowsOnly,
    },
    ToolDef {
        id: "depotdownloader",
        name: "DepotDownloaderMod",
        description: "Anonymous Steam depot downloader — downloads game files directly from Steam",
        github_owner: "mendy-tools",
        github_repo: "DepotDownloaderMod",
        preferred_asset: Some("DepotDownloaderMod-win-x64.zip"),
        preferred_asset_contains: None,
        linux_github_owner: Some("eisora08"),
        linux_github_repo: Some("DepotDownloaderMod"),
        linux_preferred_asset: Some("DepotDownloaderMod-linux-x64.zip"),
        extra_repos: None,
        install_to_steam_root: false,
        steam_dll_names: &[],
        platform: ToolPlatform::Any,
    },
    ToolDef {
        id: "cloud_redirect",
        name: "CloudRedirect",
        description: "Redirect Steam Cloud saves to Google Drive, OneDrive, S3, R2, or local folder",
        github_owner: "Selectively11",
        github_repo: "CloudRedirect",
        preferred_asset: Some("cloud_redirect.dll"),
        preferred_asset_contains: None,
        linux_github_owner: None,
        linux_github_repo: None,
        linux_preferred_asset: None,
        extra_repos: None,
        install_to_steam_root: true,
        steam_dll_names: &["cloud_redirect.dll"],
        platform: ToolPlatform::WindowsOnly,
    },
    ToolDef {
        id: "slssteam",
        name: "SLS Steam",
        description: "Steam client modification for Linux — enables playing unowned games via LD_AUDIT",
        github_owner: "AceSLS",
        github_repo: "SLSsteam",
        preferred_asset: Some("SLSsteam-Any.7z"),
        preferred_asset_contains: None,
        linux_github_owner: None,
        linux_github_repo: None,
        linux_preferred_asset: None,
        extra_repos: None,
        install_to_steam_root: false,
        steam_dll_names: &[],
        platform: ToolPlatform::LinuxOnly,
    },
];

// ---------------------------------------------------------------------------
// File-lock detection
// ---------------------------------------------------------------------------

fn is_file_locked_error(e: &std::io::Error) -> bool {
    e.raw_os_error() == Some(32) || e.raw_os_error() == Some(33)
}

// ---------------------------------------------------------------------------
// Paths & state
// ---------------------------------------------------------------------------

fn thirdparty_dir() -> PathBuf {
    crate::platform::local_data_dir().join("thirdparty")
}

fn thirdparty_tool_dir(id: &str) -> PathBuf {
    thirdparty_dir().join(id)
}

fn thirdparty_state_path() -> PathBuf {
    thirdparty_dir().join("thirdparty-state.json")
}

fn is_dir_populated(dir: &Path) -> bool {
    dir.is_dir()
        && std::fs::read_dir(dir)
            .ok()
            .and_then(|mut entries| entries.next())
            .is_some()
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct ThirdPartyState {
    #[serde(flatten)]
    tools: HashMap<String, ToolStateEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ToolStateEntry {
    version: String,
    installed_at: String,
    #[serde(default = "default_true")]
    enabled: bool,
}

fn default_true() -> bool {
    true
}

fn load_state() -> ThirdPartyState {
    let path = thirdparty_state_path();
    if path.exists() {
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|c| serde_json::from_str(&c).ok())
            .unwrap_or_default()
    } else {
        ThirdPartyState::default()
    }
}

fn save_state(state: &ThirdPartyState) {
    let path = thirdparty_state_path();
    let Ok(json) = serde_json::to_string_pretty(state) else {
        return;
    };
    if let Some(parent) = path.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
    }
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, &json).is_ok() && std::fs::rename(&tmp, &path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

pub(crate) fn now_iso() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let days = (secs / 86400) as i64;
    let time_of_day = secs % 86400;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        y,
        m,
        d,
        time_of_day / 3600,
        (time_of_day % 3600) / 60,
        time_of_day % 60
    )
}

// Howard Hinnant's civil_from_days
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

// ---------------------------------------------------------------------------
// Jobs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
struct ToolJob {
    op: String,
    status: String, // running | restarting | error
    progress: u8,
    message: String,
}

static JOBS: OnceLock<RwLock<HashMap<String, ToolJob>>> = OnceLock::new();

fn jobs() -> &'static RwLock<HashMap<String, ToolJob>> {
    JOBS.get_or_init(|| RwLock::new(HashMap::new()))
}

fn update_job<F: FnOnce(&mut ToolJob)>(tool_id: &str, f: F) {
    if let Ok(mut map) = jobs().write() {
        if let Some(job) = map.get_mut(tool_id) {
            f(job);
        }
    }
}

fn set_job(tool_id: &str, job: ToolJob) {
    if let Ok(mut map) = jobs().write() {
        map.insert(tool_id.to_string(), job);
    }
}

fn job_is_running(tool_id: &str) -> bool {
    jobs()
        .read()
        .map(|m| {
            m.get(tool_id)
                .map(|j| j.status == "running" || j.status == "restarting")
                .unwrap_or(false)
        })
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// GitHub release cache
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct ReleaseInfo {
    tag_name: String,
    zip_url: String,
    zip_name: String,
    archive_ext: String, // zip | 7z | dll
}

struct CachedRelease {
    info: Option<ReleaseInfo>,
    cached_at: SystemTime,
}

static GITHUB_CACHE: OnceLock<Mutex<HashMap<String, CachedRelease>>> = OnceLock::new();
static REFRESHING: AtomicBool = AtomicBool::new(false);

fn github_cache() -> &'static Mutex<HashMap<String, CachedRelease>> {
    GITHUB_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cache_get(key: &str) -> Option<Option<ReleaseInfo>> {
    let cache = github_cache().lock().ok()?;
    let entry = cache.get(key)?;
    let age = entry.cached_at.elapsed().unwrap_or_default();
    let ttl = if entry.info.is_some() {
        RELEASE_CACHE_TTL_SECS
    } else {
        RELEASE_ERROR_TTL_SECS
    };
    if age.as_secs() < ttl {
        return Some(entry.info.clone());
    }
    None
}

fn cache_put(key: &str, info: Option<ReleaseInfo>) {
    if let Ok(mut cache) = github_cache().lock() {
        cache.insert(
            key.to_string(),
            CachedRelease {
                info,
                cached_at: SystemTime::now(),
            },
        );
    }
}

fn http_client(timeout: Duration) -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .user_agent(APP_USER_AGENT)
        .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS))
        .timeout(timeout)
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {e}"))
}

fn select_asset(
    assets: &[serde_json::Value],
    preferred_asset: Option<&str>,
    preferred_asset_contains: Option<&str>,
) -> Option<(String, String, String)> {
    let contains_lower = preferred_asset_contains.map(|s| s.to_lowercase());
    let asset = assets
        .iter()
        .find(|a| {
            preferred_asset.is_some()
                && a["name"].as_str().is_some_and(|n| n == preferred_asset.unwrap())
        })
        .or_else(|| {
            assets.iter().find(|a| {
                contains_lower.as_ref().is_some_and(|needle| {
                    a["name"]
                        .as_str()
                        .is_some_and(|n| n.to_lowercase().contains(needle.as_str()))
                })
            })
        })
        .or_else(|| {
            assets.iter().find(|a| {
                a["name"].as_str().is_some_and(|n| {
                    n.ends_with(".zip") && !n.to_lowercase().contains("linux")
                })
            })
        })
        .or_else(|| {
            assets
                .iter()
                .find(|a| a["name"].as_str().is_some_and(|n| n.ends_with(".zip")))
        })
        .or_else(|| {
            assets
                .iter()
                .find(|a| a["name"].as_str().is_some_and(|n| n.ends_with(".7z")))
        })
        .or_else(|| {
            assets
                .iter()
                .find(|a| a["name"].as_str().is_some_and(|n| n.ends_with(".dll")))
        })?;

    let name = asset["name"].as_str().unwrap_or("release.zip").to_string();
    let url = asset["browser_download_url"].as_str()?.to_string();
    let archive_ext = if name.ends_with(".7z") {
        "7z"
    } else if name.ends_with(".dll") || name.ends_with(".so") {
        "dll"
    } else {
        "zip"
    }
    .to_string();
    Some((name, url, archive_ext))
}

fn get_latest_github_release(
    owner: &str,
    repo: &str,
    preferred_asset: Option<&str>,
    preferred_asset_contains: Option<&str>,
    force_refresh: bool,
) -> Result<ReleaseInfo, String> {
    let cache_key = format!("{owner}/{repo}");
    if !force_refresh {
        if let Some(cached) = cache_get(&cache_key) {
            return cached.ok_or_else(|| "GitHub release lookup failed (cached error)".to_string());
        }
    }

    let client = http_client(Duration::from_secs(CONNECT_TIMEOUT_SECS))?;
    let url = format!("{GITHUB_API_BASE}/repos/{owner}/{repo}/releases/latest");
    let resp = client
        .get(&url)
        .header("Accept", "application/vnd.github.v3+json")
        .send()
        .map_err(|e| format!("Failed to fetch GitHub release: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let rate_reset = resp
            .headers()
            .get("x-ratelimit-reset")
            .and_then(|v| v.to_str().ok())
            .map(String::from);
        let body = resp.text().unwrap_or_default();
        let msg = if status.as_u16() == 403 {
            if let Some(reset) = rate_reset {
                format!("GitHub API rate limit exceeded. Resets at Unix timestamp {reset}")
            } else {
                format!("GitHub API error {status}")
            }
        } else {
            format!("GitHub API error {status}: {body}")
        };
        cache_put(&cache_key, None);
        return Err(msg);
    }

    let release: serde_json::Value = resp
        .json()
        .map_err(|e| format!("Failed to parse GitHub release: {e}"))?;

    let tag_name = release["tag_name"]
        .as_str()
        .unwrap_or("unknown")
        .to_string();

    let assets = release["assets"]
        .as_array()
        .ok_or_else(|| "No assets in release".to_string())?;

    let Some((zip_name, zip_url, archive_ext)) =
        select_asset(assets, preferred_asset, preferred_asset_contains)
    else {
        cache_put(&cache_key, None);
        return Err("No ZIP/7z/DLL asset found in release".to_string());
    };

    let info = ReleaseInfo {
        tag_name,
        zip_url,
        zip_name,
        archive_ext,
    };
    cache_put(&cache_key, Some(info.clone()));
    Ok(info)
}

/// Kick a background refresh of any missing/stale release cache entries.
fn schedule_release_refresh() {
    if REFRESHING.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(|| {
        for def in TOOL_DEFS {
            if !def.platform.available_here() {
                continue;
            }
            let (owner, repo, pref, pref_contains) = def.resolve_github();
            let key = format!("{owner}/{repo}");
            if cache_get(&key).is_some() {
                continue;
            }
            let _ = get_latest_github_release(owner, repo, pref, pref_contains, false);
        }
        REFRESHING.store(false, Ordering::SeqCst);
    });
}

// ---------------------------------------------------------------------------
// File helpers
// ---------------------------------------------------------------------------

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
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("Failed to create dir: {e}"))?;
    }
    std::fs::write(dest, &bytes).map_err(|e| format!("Failed to write downloaded file: {e}"))
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

fn copy_dir_recursive(src: &Path, dest: &Path) -> Result<Vec<String>, String> {
    let mut installed = Vec::new();
    copy_dir_inner(src, dest, dest, &mut installed)?;
    Ok(installed)
}

fn copy_dir_inner(
    src: &Path,
    dest: &Path,
    base: &Path,
    installed: &mut Vec<String>,
) -> Result<(), String> {
    for entry in std::fs::read_dir(src).map_err(|e| format!("Failed to read dir: {e}"))? {
        let entry = entry.map_err(|e| format!("Failed to read entry: {e}"))?;
        let src_path = entry.path();
        let dest_path = dest.join(entry.file_name());

        if src_path.is_dir() {
            std::fs::create_dir_all(&dest_path)
                .map_err(|e| format!("Failed to create dir: {e}"))?;
            copy_dir_inner(&src_path, &dest_path, base, installed)?;
        } else {
            if let Some(parent) = dest_path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("Failed to create dir: {e}"))?;
            }
            std::fs::copy(&src_path, &dest_path)
                .map_err(|e| format!("Failed to copy file: {e}"))?;
            let relative = dest_path
                .strip_prefix(base)
                .unwrap_or(&dest_path)
                .to_string_lossy()
                .to_string();
            installed.push(relative);
        }
    }
    Ok(())
}

fn remove_dir_recursive(dir: &Path) -> Result<(), String> {
    if dir.is_dir() {
        for entry in std::fs::read_dir(dir).map_err(|e| format!("Failed to read dir: {e}"))? {
            let entry = entry.map_err(|e| format!("Failed to read entry: {e}"))?;
            let path = entry.path();
            if path.is_dir() {
                remove_dir_recursive(&path)?;
            } else {
                std::fs::remove_file(&path).map_err(|e| format!("Failed to remove file: {e}"))?;
            }
        }
        std::fs::remove_dir(dir).map_err(|e| format!("Failed to remove dir: {e}"))?;
    }
    Ok(())
}

pub(crate) fn extract_archive(
    zip_path: &Path,
    archive_ext: &str,
    extract_dir: &Path,
) -> Result<(), String> {
    std::fs::create_dir_all(extract_dir).map_err(|e| format!("Failed to create extract dir: {e}"))?;
    if archive_ext == "7z" {
        sevenz_rust::decompress_file(zip_path, extract_dir)
            .map_err(|e| format!("Failed to extract 7z: {e}"))
    } else {
        let zip_file = std::fs::File::open(zip_path).map_err(|e| format!("Failed to open ZIP: {e}"))?;
        let mut archive =
            zip::ZipArchive::new(zip_file).map_err(|e| format!("Failed to read ZIP: {e}"))?;
        archive
            .extract(extract_dir)
            .map_err(|e| format!("Failed to extract ZIP: {e}"))
    }
}

#[cfg(target_os = "linux")]
fn chmod_executables(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755));
            } else if path.is_dir() {
                chmod_executables(&path);
            }
        }
    }
}

#[cfg(target_os = "windows")]
fn chmod_executables(_dir: &Path) {}

// ---------------------------------------------------------------------------
// Steam-root deploy + restart recovery (Windows)
// ---------------------------------------------------------------------------

#[derive(Clone)]
enum PendingOp {
    Copy { src: PathBuf, dst: PathBuf },
    Delete { path: PathBuf },
}

/// Write a detached cmd script that kills Steam, waits for exit, applies
/// file ops, then relaunches Steam. Used when Steam-root DLLs are locked.
#[cfg(target_os = "windows")]
fn schedule_steam_ops(ops: &[PendingOp]) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;

    let steam_root = crate::depot_downloader::steam_root()
        .ok_or_else(|| "Steam root not found".to_string())?;
    let steam_exe = steam_root.join("Steam.exe");
    let steam_exe = if steam_exe.exists() {
        steam_exe
    } else {
        steam_root.join("steam.exe")
    };

    let mut script = String::from("@echo off\r\nsetlocal\r\n");
    script.push_str("timeout /t 1 /nobreak >nul\r\n");
    script.push_str("taskkill /IM steam.exe /F >nul 2>&1\r\n");
    script.push_str(":waitsteam\r\n");
    script.push_str("timeout /t 1 >nul\r\n");
    script.push_str("tasklist /FI \"IMAGENAME eq steam.exe\" 2>nul | find /I \"steam.exe\" >nul\r\n");
    script.push_str("if not errorlevel 1 goto waitsteam\r\n");
    for op in ops {
        match op {
            PendingOp::Copy { src, dst } => {
                script.push_str(&format!(
                    "copy /Y \"{}\" \"{}\" >nul 2>&1\r\n",
                    src.display(),
                    dst.display()
                ));
            }
            PendingOp::Delete { path } => {
                script.push_str(&format!(
                    "del /F /Q \"{}\" >nul 2>&1\r\n",
                    path.display()
                ));
            }
        }
    }
    script.push_str(&format!(
        "start \"\" \"{}\" -clearbeta\r\n",
        steam_exe.display()
    ));
    script.push_str("del /F /Q \"%~f0\"\r\n");

    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let script_path = crate::platform::temp_dir().join(format!("lumaforge_restart_{}.cmd", ts));
    std::fs::write(&script_path, script)
        .map_err(|e| format!("Failed to write restart script: {e}"))?;

    let script_arg = script_path.to_string_lossy().to_string();
    let mut cmd = std::process::Command::new("cmd");
    cmd.args(["/C", &script_arg])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    cmd.creation_flags(CREATE_NO_WINDOW | CREATE_BREAKAWAY_FROM_JOB);
    if cmd.spawn().is_err() {
        // Job may forbid breakaway — retry without it
        let mut cmd2 = std::process::Command::new("cmd");
        cmd2
            .args(["/C", &script_arg])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .creation_flags(CREATE_NO_WINDOW);
        cmd2
            .spawn()
            .map_err(|e| format!("Failed to spawn restart script: {e}"))?;
    }
    crate::log_to_temp(&format!(
        "[tools] Scheduled Steam restart script: {}",
        script_path.display()
    ));
    Ok(())
}

#[cfg(target_os = "linux")]
fn schedule_steam_ops(_ops: &[PendingOp]) -> Result<(), String> {
    Err("Automatic Steam restart deploy is not supported on Linux".to_string())
}

/// Copy tool DLLs into the Steam root. On file-lock (Steam running), fall
/// back to a detached kill→copy→restart script.
fn deploy_steam_dlls(
    tool_id: &str,
    target_dir: &Path,
    def: &ToolDef,
) -> Result<bool, String> {
    if !def.install_to_steam_root || def.steam_dll_names.is_empty() {
        return Ok(false);
    }
    let Some(steam_root) = crate::depot_downloader::steam_root() else {
        return Err("Steam root not found".to_string());
    };

    let mut ops: Vec<PendingOp> = Vec::new();
    let mut locked = false;
    let mut copy_errors: Vec<String> = Vec::new();

    for dll_name in def.steam_dll_names {
        let src = target_dir.join(dll_name);
        let dst = steam_root.join(dll_name);
        if !src.exists() {
            copy_errors.push(format!("{dll_name} not found in extracted files"));
            continue;
        }
        match std::fs::copy(&src, &dst) {
            Ok(_) => {}
            Err(e) if is_file_locked_error(&e) => {
                locked = true;
                ops.push(PendingOp::Copy { src: src.clone(), dst });
                break;
            }
            Err(e) => copy_errors.push(format!("{dll_name}: {e}")),
        }
    }

    if locked {
        crate::log_to_temp(&format!(
            "[tools] {} Steam-root DLLs locked; scheduling restart deploy",
            def.name
        ));
        schedule_steam_ops(&ops)?;
        update_job(tool_id, |j| {
            j.status = "restarting".to_string();
            j.progress = 95;
            j.message = format!(
                "{} files staged. Steam is restarting to finish the install…",
                def.name
            );
        });
        return Ok(true);
    }

    if !copy_errors.is_empty() {
        crate::log_to_temp(&format!(
            "[tools] {} Steam root copy errors: {:?}",
            def.name, copy_errors
        ));
    }
    if copy_errors.len() == def.steam_dll_names.len() {
        return Err(format!(
            "Failed to copy DLLs to Steam root: {}",
            copy_errors.join(", ")
        ));
    }

    // Ensure config/lua exists
    let lua_dir = steam_root.join("config").join("lua");
    if !lua_dir.exists() {
        let _ = std::fs::create_dir_all(&lua_dir);
    }
    Ok(false)
}

fn remove_steam_dlls(tool_id: &str, def: &ToolDef) -> Result<bool, String> {
    if !def.install_to_steam_root || def.steam_dll_names.is_empty() {
        return Ok(false);
    }
    let Some(steam_root) = crate::depot_downloader::steam_root() else {
        return Ok(false);
    };

    let mut ops: Vec<PendingOp> = Vec::new();
    let mut locked = false;

    for dll_name in def.steam_dll_names {
        let dll = steam_root.join(dll_name);
        let bak = steam_root.join(format!("{dll_name}.bak"));
        for path in [&dll, &bak] {
            if !path.exists() {
                continue;
            }
            match std::fs::remove_file(path) {
                Ok(_) => {}
                Err(e) if is_file_locked_error(&e) => {
                    locked = true;
                    ops.push(PendingOp::Delete { path: path.clone() });
                }
                Err(e) => {
                    crate::log_to_temp(&format!(
                        "[tools] Failed to remove {}: {}",
                        path.display(),
                        e
                    ));
                }
            }
        }
        if locked {
            break;
        }
    }

    if locked {
        crate::log_to_temp(&format!(
            "[tools] {} Steam-root DLLs locked on uninstall; scheduling restart cleanup",
            def.name
        ));
        schedule_steam_ops(&ops)?;
        update_job(tool_id, |j| {
            j.status = "restarting".to_string();
            j.progress = 95;
            j.message = format!(
                "{} removed from disk. Steam is restarting to finish the uninstall…",
                def.name
            );
        });
        return Ok(true);
    }

    // Remove config/lua/ if empty
    let lua_dir = steam_root.join("config").join("lua");
    if lua_dir.exists() {
        let is_empty = std::fs::read_dir(&lua_dir)
            .ok()
            .map(|mut entries| entries.next().is_none())
            .unwrap_or(false);
        if is_empty {
            let _ = std::fs::remove_dir(&lua_dir);
        }
    }
    Ok(false)
}

/// Public restart-steam entry for the bridge (Windows). Spawns a detached
/// kill→relaunch script and returns immediately so the HTTP response can be
/// written before Steam dies.
#[cfg(target_os = "windows")]
pub fn restart_steam_request() -> (u16, String) {
    // Reuse the copy-op machinery with an empty op list: kill + relaunch only.
    match schedule_steam_ops(&[]) {
        Ok(()) => (
            200,
            json!({"ok": true, "message": "Steam restarting"}).to_string(),
        ),
        Err(e) => (
            200,
            json!({"ok": false, "message": e}).to_string(),
        ),
    }
}

#[cfg(not(target_os = "windows"))]
pub fn restart_steam_request() -> (u16, String) {
    (
        200,
        json!({"ok": false, "message": "Restart not supported on this platform"}).to_string(),
    )
}

// ---------------------------------------------------------------------------
// Install / update / uninstall
// ---------------------------------------------------------------------------

fn run_install(tool_id: &str, force: bool) -> Result<String, String> {
    let def = TOOL_DEFS
        .iter()
        .find(|d| d.id == tool_id)
        .ok_or_else(|| format!("Unknown tool: {tool_id}"))?;
    if !def.platform.available_here() {
        return Err(format!(
            "{} is not available on this platform",
            def.name
        ));
    }

    let target_dir = thirdparty_tool_dir(tool_id);
    std::fs::create_dir_all(&target_dir)
        .map_err(|e| format!("Failed to create tool dir: {e}"))?;

    if !force && is_dir_populated(&target_dir) {
        // Already installed — backfill state if needed
        let mut state = load_state();
        if !state.tools.contains_key(tool_id) {
            let (owner, repo, pref, pref_contains) = def.resolve_github();
            let version = get_latest_github_release(owner, repo, pref, pref_contains, false)
                .map(|r| r.tag_name)
                .unwrap_or_else(|_| "detected".to_string());
            state.tools.insert(
                tool_id.to_string(),
                ToolStateEntry {
                    version,
                    installed_at: now_iso(),
                    enabled: true,
                },
            );
            save_state(&state);
        }
        return Ok(format!("{} is already installed.", def.name));
    }

    update_job(tool_id, |j| {
        j.progress = 10;
        j.message = format!("Fetching {} release...", def.name);
    });

    let (owner, repo, pref, pref_contains) = def.resolve_github();
    let release = get_latest_github_release(owner, repo, pref, pref_contains, force)
        .map_err(|e| format!("Failed to fetch release: {e}"))?;

    update_job(tool_id, |j| {
        j.progress = 30;
        j.message = format!("Downloading {} v{}...", def.name, release.tag_name);
    });

    let temp_root = crate::platform::temp_dir().join("thirdparty");
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let temp_dir = temp_root.join(format!("{}_{}", tool_id, ts));
    std::fs::create_dir_all(&temp_dir)
        .map_err(|e| format!("Failed to create temp dir: {e}"))?;

    let result = (|| -> Result<Vec<String>, String> {
        let zip_path = temp_dir.join(&release.zip_name);
        download_file(&release.zip_url, &zip_path)?;

        update_job(tool_id, |j| {
            j.progress = 60;
            j.message = "Extracting...".to_string();
        });

        let mut all_installed: Vec<String> = Vec::new();

        if release.archive_ext == "dll" {
            let dest = target_dir.join(&release.zip_name);
            std::fs::copy(&zip_path, &dest).map_err(|e| format!("Failed to copy DLL: {e}"))?;
            all_installed.push(release.zip_name.clone());
        } else {
            let extract_dir = temp_dir.join("extracted");
            extract_archive(&zip_path, &release.archive_ext, &extract_dir)?;
            let effective_src = flatten_extracted_dir(&extract_dir).unwrap_or(extract_dir);
            all_installed = copy_dir_recursive(&effective_src, &target_dir)?;
        }

        // Extra repos (e.g. gbe_fork_tools for goldberg_fork)
        if let Some(extra_repos) = def.extra_repos {
            for extra in extra_repos {
                update_job(tool_id, |j| {
                    j.progress = 70;
                    j.message = format!("Downloading {}...", extra.repo);
                });
                match get_latest_github_release(
                    extra.owner,
                    extra.repo,
                    extra.preferred_asset,
                    None,
                    false,
                ) {
                    Ok(extra_release) => {
                        let extra_zip = temp_dir.join(&extra_release.zip_name);
                        if let Err(e) = download_file(&extra_release.zip_url, &extra_zip) {
                            crate::log_to_temp(&format!(
                                "[tools] Failed to download {}/{}: {e}",
                                extra.owner, extra.repo
                            ));
                            continue;
                        }
                        let extra_extract = temp_dir.join(format!("extract_{}", extra.repo));
                        if let Err(e) =
                            extract_archive(&extra_zip, &extra_release.archive_ext, &extra_extract)
                        {
                            crate::log_to_temp(&format!(
                                "[tools] Failed to extract {}/{}: {e}",
                                extra.owner, extra.repo
                            ));
                            continue;
                        }
                        let extra_src =
                            flatten_extracted_dir(&extra_extract).unwrap_or(extra_extract.clone());
                        match copy_dir_recursive(&extra_src, &target_dir) {
                            Ok(files) => all_installed.extend(files),
                            Err(e) => crate::log_to_temp(&format!(
                                "[tools] Failed to copy {}/{}: {e}",
                                extra.owner, extra.repo
                            )),
                        }
                        let _ = std::fs::remove_dir_all(&extra_extract);
                    }
                    Err(e) => crate::log_to_temp(&format!(
                        "[tools] Failed to fetch release for {}/{}: {e}",
                        extra.owner, extra.repo
                    )),
                }
            }
        }

        chmod_executables(&target_dir);
        Ok(all_installed)
    })();

    let all_installed = match result {
        Ok(files) => files,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&temp_dir);
            return Err(e);
        }
    };

    // Persist version
    let mut state = load_state();
    state.tools.insert(
        tool_id.to_string(),
        ToolStateEntry {
            version: release.tag_name.clone(),
            installed_at: now_iso(),
            enabled: true,
        },
    );
    save_state(&state);

    update_job(tool_id, |j| {
        j.progress = 90;
        j.message = "Deploying to Steam root...".to_string();
    });

    let restarting = match deploy_steam_dlls(tool_id, &target_dir, def) {
        Ok(r) => r,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&temp_dir);
            return Err(e);
        }
    };

    let _ = std::fs::remove_dir_all(&temp_dir);

    if restarting {
        return Ok(format!(
            "{} v{} staged ({} files). Awaiting Steam restart.",
            def.name,
            release.tag_name,
            all_installed.len()
        ));
    }

    Ok(format!(
        "{} v{} installed. {} file(s) extracted.",
        def.name,
        release.tag_name,
        all_installed.len()
    ))
}

fn run_uninstall(tool_id: &str) -> Result<String, String> {
    let def = TOOL_DEFS
        .iter()
        .find(|d| d.id == tool_id)
        .ok_or_else(|| format!("Unknown tool: {tool_id}"))?;
    if !def.platform.available_here() {
        return Err(format!("{} is not available on this platform", def.name));
    }

    let target_dir = thirdparty_tool_dir(tool_id);
    if !is_dir_populated(&target_dir) && !def.install_to_steam_root {
        return Ok(format!("{} is not installed.", def.name));
    }

    update_job(tool_id, |j| {
        j.progress = 50;
        j.message = format!("Uninstalling {}...", def.name);
    });

    let restarting = remove_steam_dlls(tool_id, def)?;

    if target_dir.exists() {
        remove_dir_recursive(&target_dir)?;
    }

    let mut state = load_state();
    state.tools.remove(tool_id);
    save_state(&state);

    if restarting {
        return Ok(format!(
            "{} removed from disk. Awaiting Steam restart.",
            def.name
        ));
    }

    Ok(format!("{} uninstalled successfully.", def.name))
}

fn start_job(tool_id: &str, op: &str) -> (u16, String) {
    let Some(def) = TOOL_DEFS.iter().find(|d| d.id == tool_id) else {
        return (
            404,
            json!({"ok": false, "message": format!("Unknown tool: {tool_id}")}).to_string(),
        );
    };
    if !def.platform.available_here() {
        return (
            200,
            json!({"ok": false, "message": format!("{} is not available on this platform", def.name)})
                .to_string(),
        );
    }
    if job_is_running(tool_id) {
        return (
            200,
            json!({"ok": false, "message": format!("{} operation already in progress", def.name)})
                .to_string(),
        );
    }

    let force = op == "update";
    if op == "update" && !is_dir_populated(&thirdparty_tool_dir(tool_id)) {
        // Treat update-on-missing as a fresh install
    }

    set_job(
        tool_id,
        ToolJob {
            op: op.to_string(),
            status: "running".to_string(),
            progress: 0,
            message: format!("Starting {op}..."),
        },
    );

    let tid = tool_id.to_string();
    let owned_op = op.to_string();
    std::thread::spawn(move || {
        let op = owned_op.as_str();
        let result = if op == "uninstall" {
            run_uninstall(&tid)
        } else {
            if op == "update" {
                // Remove existing files first, then reinstall
                let target_dir = thirdparty_tool_dir(&tid);
                if is_dir_populated(&target_dir) {
                    update_job(&tid, |j| {
                        j.progress = 5;
                        j.message = "Removing old files...".to_string();
                    });
                    if let Err(e) = remove_dir_recursive(&target_dir) {
                        let msg = format!("Failed to remove old files: {e}");
                        update_job(&tid, |j| {
                            j.status = "error".to_string();
                            j.message = msg.clone();
                        });
                        crate::log_to_temp(&format!("[tools] update {tid}: {msg}"));
                        return;
                    }
                }
            }
            run_install(&tid, force || op == "update")
        };

        match result {
            Ok(msg) => {
                update_job(&tid, |j| {
                    if j.status != "restarting" {
                        j.status = "done".to_string();
                        j.progress = 100;
                    }
                    j.message = msg.clone();
                });
                crate::log_to_temp(&format!("[tools] {tid} {op}: {msg}"));
            }
            Err(e) => {
                update_job(&tid, |j| {
                    j.status = "error".to_string();
                    j.message = e.clone();
                });
                crate::log_to_temp(&format!("[tools] {tid} {op} FAILED: {e}"));
            }
        }
    });

    (
        200,
        json!({"ok": true, "message": format!("Starting {op}...")}).to_string(),
    )
}

// ---------------------------------------------------------------------------
// List
// ---------------------------------------------------------------------------

fn handle_list() -> (u16, String) {
    schedule_release_refresh();

    let mut state = load_state();
    let mut state_updated = false;
    let jobs_map = jobs().read().map(|j| j.clone()).unwrap_or_default();

    let mut tools = Vec::new();

    for def in TOOL_DEFS {
        let available = def.platform.available_here();
        let tool_dir = thirdparty_tool_dir(def.id);
        let mut installed = is_dir_populated(&tool_dir);

        // Steam-root-only detection (manually deployed DLLs)
        if !installed && def.install_to_steam_root {
            if let Some(steam_root) = crate::depot_downloader::steam_root() {
                installed = def
                    .steam_dll_names
                    .iter()
                    .any(|d| steam_root.join(d).exists());
            }
        }
        if !available {
            installed = false;
        }

        let mut installed_version = state.tools.get(def.id).map(|e| e.version.clone());

        let (owner, repo, _pref, _pref_contains) = def.resolve_github();
        let cache_key = format!("{owner}/{repo}");
        let latest_version = cache_get(&cache_key).and_then(|i| i).map(|r| r.tag_name);

        // Auto-detect: installed but no state entry
        if installed && installed_version.is_none() {
            let ver = latest_version.clone().unwrap_or_else(|| "detected".to_string());
            state.tools.insert(
                def.id.to_string(),
                ToolStateEntry {
                    version: ver.clone(),
                    installed_at: now_iso(),
                    enabled: true,
                },
            );
            installed_version = Some(ver);
            state_updated = true;
        }

        let update_available = match (&installed_version, &latest_version) {
            (Some(installed_v), Some(latest_v)) => {
                installed_v != latest_v && installed_v != "detected"
            }
            _ => false,
        };

        let job = jobs_map.get(def.id).cloned();

        tools.push(json!({
            "id": def.id,
            "name": def.name,
            "description": def.description,
            "platform": def.platform.as_str(),
            "available": available,
            "installed": installed,
            "installedVersion": installed_version,
            "latestVersion": latest_version,
            "updateAvailable": update_available,
            "installPath": if installed { tool_dir.to_string_lossy() } else { "".into() },
            "job": job,
        }));
    }

    if state_updated {
        save_state(&state);
    }

    (
        200,
        json!({"ok": true, "tools": tools}).to_string(),
    )
}

// ---------------------------------------------------------------------------
// Route handler
// ---------------------------------------------------------------------------

pub fn try_handle_route(method: &str, path: &str, _body: &str) -> Option<(u16, String)> {
    if path == "/api/tools" && method == "GET" {
        return Some(handle_list());
    }
    if path.starts_with("/api/tools/") && method == "POST" {
        let rest = &path["/api/tools/".len()..];
        let mut parts = rest.splitn(2, '/');
        let tool_id = parts.next().unwrap_or("");
        let action = parts.next().unwrap_or("");
        if tool_id.is_empty() {
            return Some((
                400,
                json!({"ok": false, "message": "Missing tool id"}).to_string(),
            ));
        }
        return Some(match action {
            "install" | "update" | "uninstall" => start_job(tool_id, action),
            _ => (
                404,
                json!({"ok": false, "message": format!("Unknown action: {action}")}).to_string(),
            ),
        });
    }
    None
}
