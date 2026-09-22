use regex::Regex;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use crate::platform;
use crate::manifest_parser;

// ---------------------------------------------------------------------------
// Serde helper — accept both string and number for u64 fields
// ---------------------------------------------------------------------------

fn deserialize_string_or_u64<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    let val = serde_json::Value::deserialize(deserializer)?;
    match val {
        serde_json::Value::Number(n) => n.as_u64().ok_or_else(|| serde::de::Error::custom("number out of range")),
        serde_json::Value::String(s) => s.parse::<u64>().map_err(serde::de::Error::custom),
        _ => Err(serde::de::Error::custom("expected number or string")),
    }
}

// ---------------------------------------------------------------------------
// SteamCMD API enrichment (matches luma-lite's steamcmd_api)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Deserialize)]
struct SteamCmdDepotInfo {
    depot_id: u64,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    dlc_app_id: Option<u64>,
    #[serde(default)]
    is_shared: bool,
    #[serde(default)]
    os: Option<String>,
    #[serde(default)]
    language: Option<String>,
    #[serde(default)]
    public_manifest_id: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct SteamCmdAppInfo {
    #[serde(default)]
    app_name: Option<String>,
    #[serde(default)]
    depots: Vec<SteamCmdDepotInfo>,
}

fn fetch_app_depot_info(app_id: u64) -> Option<SteamCmdAppInfo> {
    let url = format!("https://api.steamcmd.net/v1/info/{}", app_id);
    let client = reqwest::blocking::Client::builder()
        .user_agent(concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .ok()?;
    let resp = client.get(&url).send().ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let root: serde_json::Value = resp.json().ok()?;
    let data = root.get("data")?.get(app_id.to_string())?;

    let app_name = data
        .get("common")
        .and_then(|c| c.get("name"))
        .and_then(|n| n.as_str())
        .map(|s| s.to_string());

    let mut depots = Vec::new();
    if let Some(depot_map) = data.get("depots").and_then(|d| d.as_object()) {
        for (key, val) in depot_map {
            let depot_id: u64 = match key.parse() {
                Ok(id) => id,
                Err(_) => continue,
            };
            if !val.is_object() {
                continue;
            }
            let is_shared = val.get("depotfromapp").is_some();
            let dlc_app_id = val
                .get("dlcappid")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<u64>().ok());
            let mut os = None;
            let mut language = None;
            if let Some(config) = val.get("config").and_then(|c| c.as_object()) {
                os = config
                    .get("oslist")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                language = config
                    .get("dlclanguage")
                    .or_else(|| config.get("language"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
            }
            let public_manifest_id = val
                .get("manifests")
                .and_then(|m| m.get("public"))
                .and_then(|p| p.get("gid"))
                .and_then(|g| g.as_str())
                .map(|s| s.to_string());
            let public_size = val
                .get("manifests")
                .and_then(|m| m.get("public"))
                .and_then(|p| p.get("size"))
                .and_then(|s| s.as_str())
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);

            depots.push(SteamCmdDepotInfo {
                depot_id,
                size: public_size,
                dlc_app_id,
                is_shared,
                os,
                language,
                public_manifest_id,
            });
        }
    }

    Some(SteamCmdAppInfo { app_name, depots })
}

// ---------------------------------------------------------------------------
// Lua parser (inline — parses addappid/setManifestid from .lua files)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct LuaDepotEntry {
    depot_id: u64,
    key: Option<String>,
    manifest_id: Option<String>,
    size_on_disk: Option<u64>,
    is_active: bool,
}

fn parse_lua_content(content: &str) -> Vec<LuaDepotEntry> {
    let re_add = Regex::new(
        r#"(?i)addappid\s*\(\s*(\d+)\s*(?:,\s*\d+\s*(?:,\s*"([^"]*)")?)?\s*\)"#,
    );
    let re_manifest = Regex::new(
        r#"(?i)setManifestid\s*\(\s*(\d+)\s*,\s*"(\d+)"\s*(?:,\s*(\d+))?"#,
    );
    let re_add = match re_add { Ok(r) => r, Err(_) => return vec![] };
    let re_manifest = match re_manifest { Ok(r) => r, Err(_) => return vec![] };

    let mut entries: HashMap<u64, LuaDepotEntry> = HashMap::new();

    for raw_line in content.lines() {
        let line = raw_line.trim();
        let is_active = !line.starts_with("--");

        if let Some(caps) = re_manifest.captures(line) {
            if let Some(id_str) = caps.get(1).map(|m| m.as_str()) {
                if let Ok(depot_id) = id_str.parse::<u64>() {
                    let manifest_id = caps.get(2).map(|m| m.as_str().to_string());
                    let size = caps.get(3).and_then(|m| m.as_str().parse::<u64>().ok());
                    let entry = entries.entry(depot_id).or_insert_with(|| LuaDepotEntry {
                        depot_id,
                        key: None,
                        manifest_id: None,
                        size_on_disk: None,
                        is_active,
                    });
                    if is_active {
                        entry.manifest_id = manifest_id;
                    }
                    if let Some(s) = size {
                        entry.size_on_disk = Some(s);
                    }
                    entry.is_active = entry.is_active || is_active;
                }
            }
        }

        let stripped = if !is_active {
            line.trim_start_matches('-').trim_start()
        } else {
            line
        };

        if let Some(caps) = re_add.captures(stripped) {
            if let Some(id_str) = caps.get(1).map(|m| m.as_str()) {
                if let Ok(depot_id) = id_str.parse::<u64>() {
                    let key = caps.get(2).map(|m| m.as_str().to_string()).filter(|k| !k.is_empty());
                    let entry = entries.entry(depot_id).or_insert_with(|| LuaDepotEntry {
                        depot_id,
                        key: None,
                        manifest_id: None,
                        size_on_disk: None,
                        is_active,
                    });
                    if is_active && key.is_some() {
                        entry.key = key;
                    }
                    entry.is_active = entry.is_active || is_active;
                }
            }
        }
    }

    entries.into_values().collect()
}

struct LuaDepotData {
    key: Option<String>,
    manifest_id: Option<String>,
    size_on_disk: Option<u64>,
}

fn resolve_depots_from_lua(lua_dir: &Path, app_id: u64) -> HashMap<u64, LuaDepotData> {
    let mut depots: HashMap<u64, LuaDepotData> = HashMap::new();

    let lua_path = lua_dir.join(format!("{app_id}.lua"));
    if lua_path.exists() {
        if let Ok(content) = std::fs::read_to_string(&lua_path) {
            for entry in parse_lua_content(&content) {
                let d = depots.entry(entry.depot_id).or_insert_with(|| LuaDepotData {
                    key: None,
                    manifest_id: None,
                    size_on_disk: None,
                });
                if entry.is_active {
                    if let Some(k) = entry.key { d.key = Some(k); }
                    if let Some(m) = entry.manifest_id { d.manifest_id = Some(m); }
                }
                if let Some(s) = entry.size_on_disk { d.size_on_disk = Some(s); }
            }
        }
    }

    let disabled_path = lua_dir.join(format!("{app_id}.lua.disabled"));
    if disabled_path.exists() {
        if let Ok(content) = std::fs::read_to_string(&disabled_path) {
            for entry in parse_lua_content(&content) {
                let d = depots.entry(entry.depot_id).or_insert_with(|| LuaDepotData {
                    key: None,
                    manifest_id: None,
                    size_on_disk: None,
                });
                if d.key.is_none() { d.key = entry.key; }
                if d.manifest_id.is_none() { d.manifest_id = entry.manifest_id; }
                if d.size_on_disk.is_none() { d.size_on_disk = entry.size_on_disk; }
            }
        }
    }

    depots
}

fn resolve_keys_from_lua(lua_dir: &Path, app_id: u64) -> HashMap<u64, String> {
    resolve_depots_from_lua(lua_dir, app_id)
        .into_iter()
        .filter_map(|(id, d)| d.key.map(|k| (id, k)))
        .collect()
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DepotInfo {
    pub depot_id: u64,
    pub name: String,
    pub manifest_id: String,
    pub manifest_path: Option<String>,
    pub size: u64,
    pub key: Option<String>,
    pub encrypted: bool,
    #[serde(default)]
    pub dlc_app_id: Option<u64>,
    #[serde(default)]
    pub os: Option<String>,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub is_shared: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DepotDownloadJob {
    #[serde(default)]
    pub job_id: Option<String>,
    #[serde(deserialize_with = "deserialize_string_or_u64")]
    pub app_id: u64,
    pub game_name: String,
    pub output_dir: String,
    pub depots: Vec<DepotSelection>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DepotSelection {
    #[serde(deserialize_with = "deserialize_string_or_u64")]
    pub depot_id: u64,
    pub manifest_id: String,
    #[serde(default)]
    pub manifest_path: Option<String>,
    #[serde(default, deserialize_with = "deserialize_string_or_u64")]
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DepotResolveResult {
    pub depots: Vec<DepotInfo>,
    pub game_name: String,
    pub output_dir: String,
}

// ---------------------------------------------------------------------------
// Download queue (persistent on disk)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueItem {
    #[serde(default)]
    pub id: String,
    #[serde(deserialize_with = "deserialize_string_or_u64")]
    pub app_id: u64,
    pub game_name: String,
    pub output_dir: String,
    pub depots: Vec<DepotSelection>,
    #[serde(default)]
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_id: Option<String>,
    #[serde(default)]
    pub progress: f64,
    #[serde(default)]
    pub bytes_downloaded: u64,
    #[serde(default)]
    pub total_bytes: u64,
    #[serde(default)]
    pub added_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryItem {
    pub id: String,
    pub app_id: u64,
    pub game_name: String,
    pub status: String,
    pub progress: f64,
    pub bytes_downloaded: u64,
    pub total_bytes: u64,
    pub completed_at: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueFile {
    #[serde(default)]
    pub queue: Vec<QueueItem>,
    #[serde(default)]
    pub history: Vec<HistoryItem>,
}

#[derive(Debug, Clone)]
struct JobState {
    job_id: String,
    app_id: u64,
    pid: Option<u32>,
    status: String,       // "downloading", "validating", "completed", "failed", "cancelled", "paused"
    phase: String,
    progress: f64,
    bytes_read: u64,
    total_bytes: u64,
    message: String,
    error: Option<String>,
    started_at: Instant,
    last_output: Instant,
    last_disk_flush: Instant,
}

static JOBS: LazyLock<Mutex<HashMap<String, JobState>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static PAUSED_PIDS: LazyLock<Mutex<Vec<u32>>> = LazyLock::new(|| Mutex::new(Vec::new()));

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

pub(crate) fn steam_root() -> Option<PathBuf> {
    platform::find_steam_install().or_else(|| {
        let home = std::env::var("HOME").ok()?;
        let fallback = PathBuf::from(home).join(".steam").join("steam");
        if fallback.exists() { Some(fallback) } else { None }
    })
}

fn thirdparty_dir() -> PathBuf {
    platform::local_data_dir().join("thirdparty")
}

fn depot_downloader_exe() -> Option<PathBuf> {
    let base = thirdparty_dir().join("depotdownloader");
    let exe = base.join("DepotDownloaderMod");
    if exe.exists() {
        Some(exe)
    } else {
        None
    }
}

fn lua_dir() -> Option<PathBuf> {
    Some(steam_root()?.join("config").join("lua"))
}

fn depotcache_dir() -> Option<PathBuf> {
    Some(steam_root()?.join("depotcache"))
}

fn staging_dir() -> PathBuf {
    platform::local_data_dir().join("staging")
}

fn parse_output_line(line: &str) -> Option<(String, f64)> {
    if let Some(pos) = line.find('%') {
        let before = &line[..pos];
        if let Some(start) = before.rfind(' ') {
            let pct_str = &before[start + 1..];
            if let Ok(pct) = pct_str.parse::<f64>() {
                let pct = pct.clamp(0.0, 100.0);
                let phase = if line.contains("Validating") { "validating" } else { "downloading" };
                return Some((phase.to_string(), pct));
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Resolve depots for a given app_id from .lua files + depotcache + SteamCMD API.
/// This mirrors luma-lite's `depot_downloader_resolve_depots` behavior.
pub fn resolve_depots(app_id: u64) -> Result<DepotResolveResult, String> {
    let lua_dir = lua_dir().ok_or("Lua directory not found")?;
    let lua_depots = resolve_depots_from_lua(&lua_dir, app_id);

    if lua_depots.is_empty() {
        return Err(format!(
            "No depot keys found for app {}. Make sure the .lua file is installed.",
            app_id
        ));
    }

    // Fetch API enrichment (like luma-lite's steamcmd_api)
    let api_info = fetch_app_depot_info(app_id);

    let depotcache = depotcache_dir();
    let mut depots: Vec<DepotInfo> = Vec::new();

    for (depot_id, lua_data) in &lua_depots {
        let manifest_path = depotcache
            .as_ref()
            .and_then(|dc| find_manifest_for_depot(dc, *depot_id));

        let (manifest_id, manifest_path_str, size, encrypted) = if let Some(ref path) = manifest_path {
            if let Some(info) = manifest_parser::try_read_manifest(path) {
                (
                    info.gid_manifest.to_string(),
                    Some(path.to_string_lossy().to_string()),
                    info.size_on_disk,
                    info.filenames_encrypted,
                )
            } else {
                // Fallback to lua data
                let mid = lua_data.manifest_id.clone().unwrap_or_default();
                let sz = lua_data.size_on_disk.unwrap_or(0);
                (mid, None, sz, lua_data.key.is_some())
            }
        } else {
            let mid = lua_data.manifest_id.clone().unwrap_or_default();
            let sz = lua_data.size_on_disk.unwrap_or(0);
            (mid, None, sz, lua_data.key.is_some())
        };

        // Merge API data
        let api_depot = api_info
            .as_ref()
            .and_then(|info| info.depots.iter().find(|d| d.depot_id == *depot_id));

        let dlc_app_id = api_depot.and_then(|d| d.dlc_app_id);
        let os = api_depot.and_then(|d| d.os.clone());
        let language = api_depot.and_then(|d| d.language.clone());
        let is_shared = api_depot.map(|d| d.is_shared).unwrap_or(false);

        let name = if let Some(dlc_id) = dlc_app_id {
            format!("DLC Depot ({})", dlc_id)
        } else if is_shared {
            format!("Shared Depot {}", depot_id)
        } else {
            format!("Depot {}", depot_id)
        };

        depots.push(DepotInfo {
            depot_id: *depot_id,
            name,
            manifest_id,
            manifest_path: manifest_path_str,
            size,
            key: lua_data.key.clone(),
            encrypted,
            dlc_app_id,
            os,
            language,
            is_shared,
        });
    }

    depots.sort_by_key(|d| d.depot_id);

    let game_name = api_info
        .as_ref()
        .and_then(|i| i.app_name.clone())
        .or_else(|| {
            let content = std::fs::read_to_string(lua_dir.join(format!("{app_id}.lua"))).ok()?;
            for line in content.lines() {
                let line = line.trim();
                if let Some(name) = line.strip_prefix("-- ") {
                    if !name.is_empty() {
                        return Some(name.to_string());
                    }
                }
            }
            None
        })
        .unwrap_or_else(|| format!("Game {}", app_id));

    let output_dir = steam_root()
        .map(|r| r.to_string_lossy().to_string())
        .unwrap_or_default();

    Ok(DepotResolveResult {
        depots,
        game_name,
        output_dir,
    })
}

/// Find and validate a manifest file for a depot by parsing its content.
/// Mirrors luma-lite's `find_manifest_for_depot`.
fn find_manifest_for_depot(depotcache: &Path, depot_id: u64) -> Option<PathBuf> {
    if !depotcache.exists() {
        return None;
    }

    let prefix = format!("{}_", depot_id);

    for entry in std::fs::read_dir(depotcache).ok()? {
        let entry = entry.ok()?;
        let name = entry.file_name().to_string_lossy().to_string();

        if name.starts_with(&prefix) && name.ends_with(".manifest") {
            let path = entry.path();
            if let Some(info) = manifest_parser::try_read_manifest(&path) {
                if info.depot_id == depot_id {
                    return Some(path);
                }
            }
        }
    }

    None
}

/// Start a depot download job.
pub fn start_download(mut job: DepotDownloadJob) -> Result<String, String> {
    let exe = depot_downloader_exe()
        .ok_or("DepotDownloaderMod is not installed. Install it from Settings > Third-Party Tools.")?;

    // Generate job_id if not provided by the client
    let job_id = job.job_id.take().filter(|s| !s.is_empty()).unwrap_or_else(|| {
        use std::time::{SystemTime, UNIX_EPOCH};
        let ts = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis();
        let rand: u32 = rand::random();
        format!("{}-{:08x}", ts, rand)
    });

    let lua_dir = lua_dir().ok_or("Lua directory not found")?;
    let keys = resolve_keys_from_lua(&lua_dir, job.app_id);
    if keys.is_empty() {
        return Err("No depot keys found".to_string());
    }

    // Write keys file
    let staging = staging_dir();
    std::fs::create_dir_all(&staging).ok();
    let keys_path = staging.join(format!("depotkeys_{}.txt", &job_id));
    let mut keys_content = String::new();
    for (depot_id, key) in &keys {
        keys_content.push_str(&format!("{};{}\n", depot_id, key));
    }
    std::fs::write(&keys_path, &keys_content).map_err(|e| format!("Failed to write keys file: {e}"))?;

    let output_dir = PathBuf::from(&job.output_dir)
        .join("steamapps")
        .join("common");
    std::fs::create_dir_all(&output_dir).ok();

    let total_bytes: u64 = job.depots.iter().map(|d| d.size).sum();

    // Initial state
    {
        let mut jobs = JOBS.lock().map_err(|e| e.to_string())?;
        jobs.insert(job_id.clone(), JobState {
            job_id: job_id.clone(),
            app_id: job.app_id,
            pid: None,
            status: "downloading".to_string(),
            phase: "downloading".to_string(),
            progress: 0.0,
            bytes_read: 0,
            total_bytes,
            message: "Starting download...".to_string(),
            error: None,
            started_at: Instant::now(),
            last_output: Instant::now(),
            last_disk_flush: Instant::now(),
        });
    }

    let job_id_return = job_id.clone();
    let app_id = job.app_id;
    let depots_clone = job.depots.clone();

    // Sanitize game name for folder (same logic as post_download)
    let game_name = job.game_name.clone();
    let safe_name: String = game_name
        .chars()
        .map(|c| if c.is_alphanumeric() || c == ' ' || c == '-' || c == '.' { c } else { '_' })
        .collect::<String>()
        .trim()
        .replace(' ', "_");
    let installdir = if safe_name.is_empty() {
        format!("App_{}", app_id)
    } else {
        safe_name
    };

    // Spawn download thread
    std::thread::spawn(move || {
        let mut cumulative_bytes: u64 = 0;
        let depots_for_integration = depots_clone.clone();

        for (i, depot) in depots_clone.into_iter().enumerate() {
            // Build args
            let mut args = vec![
                "-app".to_string(),
                app_id.to_string(),
                "-depot".to_string(),
                depot.depot_id.to_string(),
            ];

            if !depot.manifest_id.is_empty() {
                args.push("-manifest".to_string());
                args.push(depot.manifest_id.clone());
            }

            // Pass local manifest file path if available (avoids re-downloading manifest)
            if let Some(ref mp) = depot.manifest_path {
                if !mp.is_empty() {
                    args.push("-manifestfile".to_string());
                    args.push(mp.clone());
                }
            }

            args.push("-depotkeys".to_string());
            args.push(keys_path.display().to_string());

            let depot_output = output_dir.join(&installdir);
            std::fs::create_dir_all(&depot_output).ok();

            args.push("-dir".to_string());
            args.push(depot_output.display().to_string());
            args.push("-max-downloads".to_string());
            args.push("32".to_string());
            args.push("-validate".to_string());

            // Kill any existing process for this app_id before spawning
            #[cfg(target_os = "linux")]
            kill_existing_for_app(job.app_id);

            // Spawn process
            let mut cmd = std::process::Command::new(&exe);
            cmd.args(&args)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());

            if let Some(parent) = exe.parent() {
                cmd.current_dir(parent);
            }

            let mut child = match cmd.spawn() {
                Ok(c) => c,
                Err(e) => {
                    let _ = update_job(&job_id, |j| {
                        j.status = "failed".to_string();
                        j.error = Some(format!("Failed to spawn DepotDownloaderMod: {e}"));
                    });
                    let _ = std::fs::remove_file(&keys_path);
                    if let Some(qid) = find_queue_id_for_job(&job_id) {
                        on_download_complete(&qid, &format!("Failed to spawn DepotDownloaderMod: {e}"));
                    }
                    return;
                }
            };

            let pid = child.id();
            {
                let _ = update_job(&job_id, |j| { j.pid = Some(pid); });
            }

            // Read stdout
            let stdout = child.stdout.take().unwrap();
            let stderr = child.stderr.take().unwrap();

            let stdout_job = job_id.clone();
            let stdout_handle = std::thread::spawn(move || {
                let reader = BufReader::new(stdout);
                for line in reader.lines() {
                    if let Ok(line) = line {
                        if let Some((phase, progress)) = parse_output_line(&line) {
                            let depot_bytes = (depot.size as f64 * progress / 100.0) as u64;
                            let bytes_read = cumulative_bytes + depot_bytes;
                            let _ = update_job(&stdout_job, |j| {
                                j.phase = phase;
                                j.progress = if total_bytes > 0 {
                                    ((bytes_read as f64 / total_bytes as f64) * 100.0).min(99.0)
                                } else {
                                    progress
                                };
                                j.bytes_read = bytes_read;
                                j.message = line;
                                j.last_output = Instant::now();
                            });
                        }
                    }
                }
            });

            // Read stderr
            let stderr_job = job_id.clone();
            let stderr_handle = std::thread::spawn(move || {
                let reader = BufReader::new(stderr);
                for line in reader.lines() {
                    if let Ok(line) = line {
                        crate::log_to_temp(&format!("[depot-dl] stderr: {}", line));
                    }
                }
            });

            // Wait with silence timeout
            let silence_timeout = Duration::from_secs(600);
            let status = loop {
                match child.try_wait() {
                    Ok(Some(status)) => break status,
                    Ok(None) => {
                        // Check if paused
                        let is_paused = PAUSED_PIDS.lock()
                            .map(|p| p.contains(&pid))
                            .unwrap_or(false);
                        if is_paused {
                            std::thread::sleep(Duration::from_millis(500));
                            continue;
                        }

                        // Check silence timeout
                        let last_output = JOBS.lock()
                            .ok()
                            .and_then(|j| j.get(&job_id).map(|j| j.last_output))
                            .unwrap_or_else(Instant::now);
                        if last_output.elapsed() > silence_timeout {
                            let _ = update_job(&job_id, |j| {
                                j.status = "failed".to_string();
                                j.error = Some("Download timed out (no output for 10 minutes)".to_string());
                            });
                            if let Some(qid) = find_queue_id_for_job(&job_id) {
                                on_download_complete(&qid, "Download timed out (no output for 10 minutes)");
                            }
                            return;
                        }

                        std::thread::sleep(Duration::from_millis(500));
                    }
                    Err(e) => {
                        let _ = update_job(&job_id, |j| {
                            j.status = "failed".to_string();
                            j.error = Some(format!("Process wait error: {e}"));
                        });
                        if let Some(qid) = find_queue_id_for_job(&job_id) {
                            on_download_complete(&qid, &format!("Process wait error: {e}"));
                        }
                        return;
                    }
                }
            };

            let _ = stdout_handle.join();
            let _ = stderr_handle.join();

            if !status.success() {
                let _ = update_job(&job_id, |j| {
                    j.status = "failed".to_string();
                    j.error = Some(format!("DepotDownloaderMod exited with status: {}", status));
                });
                let _ = std::fs::remove_file(&keys_path);
                // Move to queue history and start next
                if let Some(qid) = find_queue_id_for_job(&job_id) {
                    on_download_complete(&qid, &format!("DepotDownloaderMod exited with status: {}", status));
                }
                return;
            }

            cumulative_bytes += depot.size;
        }

        // All depots done — integrate with Steam (8-step process)
        let _ = update_job(&job_id, |j| {
            j.status = "integrating".to_string();
            j.progress = 100.0;
            j.message = "Integrating with Steam...".to_string();
        });

        let qid_opt = find_queue_id_for_job(&job_id);
        let integration_result = post_download_steam_integration(
            app_id,
            &game_name,
            &output_dir,
            &depots_for_integration,
        );

        match integration_result {
            Ok(steps) => {
                crate::log_to_temp(&format!("[depot-dl] Steam integration complete for job {}: {:?}", job_id, steps));
            }
            Err(e) => {
                crate::log_to_temp(&format!("[depot-dl] Steam integration failed for job {}: {}", job_id, e));
            }
        }

        let _ = update_job(&job_id, |j| {
            j.status = "completed".to_string();
            j.progress = 100.0;
            j.message = "Download completed".to_string();
        });
        let _ = std::fs::remove_file(&keys_path);
        // Move to queue history and start next
        if let Some(qid) = qid_opt {
            on_download_complete(&qid, "");
        }
    });

    Ok(job_id_return)
}

/// Get the status of a download job.
pub fn get_status(job_id: &str) -> serde_json::Value {
    let jobs = match JOBS.lock() {
        Ok(j) => j,
        Err(_) => return json!({"ok": false, "message": "Lock error"}),
    };

    if let Some(job) = jobs.get(job_id) {
        json!({
            "ok": true,
            "jobId": job.job_id,
            "appId": job.app_id,
            "status": job.status,
            "phase": job.phase,
            "progress": job.progress,
            "bytesRead": job.bytes_read,
            "totalBytes": job.total_bytes,
            "message": job.message,
            "error": job.error,
        })
    } else {
        json!({"ok": false, "message": "Job not found"})
    }
}

/// Pause a download (SIGSTOP).
pub fn pause_download(job_id: &str) -> Result<bool, String> {
    let pid = {
        let jobs = JOBS.lock().map_err(|e| e.to_string())?;
        jobs.get(job_id).and_then(|j| j.pid).ok_or("Job not found or no PID")?
    };

    #[cfg(target_os = "linux")]
    unsafe {
        libc::kill(pid as i32, libc::SIGSTOP);
    }

    {
        let mut paused = PAUSED_PIDS.lock().map_err(|e| e.to_string())?;
        paused.push(pid);
    }

    let _ = update_job(job_id, |j| {
        j.status = "paused".to_string();
        j.message = "Download paused".to_string();
    });

    Ok(true)
}

/// Resume a download (SIGCONT).
pub fn resume_download(job_id: &str) -> Result<bool, String> {
    let pid = {
        let jobs = JOBS.lock().map_err(|e| e.to_string())?;
        jobs.get(job_id).and_then(|j| j.pid).ok_or("Job not found or no PID")?
    };

    #[cfg(target_os = "linux")]
    unsafe {
        libc::kill(pid as i32, libc::SIGCONT);
    }

    {
        let mut paused = PAUSED_PIDS.lock().map_err(|e| e.to_string())?;
        paused.retain(|&p| p != pid);
    }

    let _ = update_job(job_id, |j| {
        j.status = "downloading".to_string();
        j.message = "Download resumed".to_string();
    });

    Ok(true)
}

/// Cancel a download (SIGTERM).
pub fn cancel_download(job_id: &str) -> Result<bool, String> {
    let pid = {
        let mut jobs = JOBS.lock().map_err(|e| e.to_string())?;
        match jobs.remove(job_id) {
            Some(j) => j.pid,
            None => return Err("Job not found".to_string()),
        }
    };

    if let Some(pid) = pid {
        #[cfg(target_os = "linux")]
        unsafe {
            libc::kill(pid as i32, libc::SIGTERM);
        }
    }

    Ok(true)
}

/// Kill orphaned DepotDownloader processes left over from previous sessions.
#[cfg(target_os = "linux")]
pub fn kill_orphaned_depots() {
    use std::io::Read;

    let my_pid = std::process::id();
    let mut pids: Vec<u32> = Vec::new();

    if let Ok(entries) = std::fs::read_dir("/proc") {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if let Ok(pid) = name_str.parse::<u32>() {
                if pid == my_pid {
                    continue;
                }
                let cmdline_path = entry.path().join("cmdline");
                if let Ok(mut f) = std::fs::File::open(&cmdline_path) {
                    let mut buf = String::new();
                    let _ = f.read_to_string(&mut buf);
                    let lower = buf.to_lowercase();
                    if lower.contains("depotdownloader") || lower.contains("depot_downloader") {
                        pids.push(pid);
                    }
                }
            }
        }
    }

    if pids.is_empty() {
        crate::log_to_temp("[depot-dl] No orphaned DepotDownloader processes found");
        return;
    }

    crate::log_to_temp(&format!(
        "[depot-dl] Killing {} orphaned DepotDownloader processes: {:?}",
        pids.len(),
        pids
    ));

    for pid in &pids {
        unsafe {
            libc::kill(*pid as i32, libc::SIGTERM);
        }
    }
}

/// Kill any existing DepotDownloader process for a specific app_id.
#[cfg(target_os = "linux")]
fn kill_existing_for_app(app_id: u64) {
    use std::io::Read;

    let my_pid = std::process::id();
    let app_id_str = app_id.to_string();
    let mut pids: Vec<u32> = Vec::new();

    if let Ok(entries) = std::fs::read_dir("/proc") {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if let Ok(pid) = name_str.parse::<u32>() {
                if pid == my_pid { continue; }
                let cmdline_path = entry.path().join("cmdline");
                if let Ok(mut f) = std::fs::File::open(&cmdline_path) {
                    let mut buf = String::new();
                    let _ = f.read_to_string(&mut buf);
                    let lower = buf.to_lowercase();
                    if (lower.contains("depotdownloader") || lower.contains("depot_downloader"))
                        && buf.contains(&app_id_str)
                    {
                        pids.push(pid);
                    }
                }
            }
        }
    }

    for pid in &pids {
        crate::log_to_temp(&format!("[depot-dl] Killing existing process {} for app {}", pid, app_id));
        unsafe { libc::kill(*pid as i32, libc::SIGTERM); }
    }
}

/// Post-download: create ACF, move manifests, restart Steam.
pub fn post_download(job_id: &str, app_id: u64, game_name: &str) -> Result<serde_json::Value, String> {
    let steam = steam_root().ok_or("Steam root not found")?;
    let lua_dir = lua_dir().ok_or("Lua dir not found")?;
    let keys = resolve_keys_from_lua(&lua_dir, app_id);

    let safe_name: String = game_name
        .chars()
        .map(|c| if c.is_alphanumeric() || c == ' ' || c == '-' || c == '.' { c } else { '_' })
        .collect::<String>()
        .trim()
        .replace(' ', "_");
    let installdir = if safe_name.is_empty() {
        format!("App_{}", app_id)
    } else {
        safe_name
    };

    let mut steps: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();

    // 1. Create ACF
    let acf_path = steam.join("steamapps").join(format!("appmanifest_{}.acf", app_id));
    if let Some(parent) = acf_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let mut acf = String::new();
    acf.push_str("\"AppState\"\n{\n");
    acf.push_str(&format!("\t\"appid\"\t\t\"{}\"\n", app_id));
    acf.push_str("\t\"Universe\"\t\t\"1\"\n");
    acf.push_str(&format!("\t\"name\"\t\t\"{}\"\n", game_name.replace('\\', "\\\\").replace('"', "\\\"")));
    acf.push_str("\t\"StateFlags\"\t\t\"4\"\n");
    acf.push_str(&format!("\t\"installdir\"\t\t\"{}\"\n", installdir));
    acf.push_str(&format!("\t\"SizeOnDisk\"\t\t\"0\"\n"));
    acf.push_str("\t\"buildid\"\t\t\"0\"\n");
    acf.push_str("\t\"InstalledDepots\"\n");
    acf.push_str("\t{\n");
    for (depot_id, key) in &keys {
        acf.push_str(&format!("\t\t\"{}\"\n", depot_id));
        acf.push_str("\t\t{\n");
        acf.push_str(&format!("\t\t\t\"manifest\"\t\t\"\"\n"));
        acf.push_str("\t\t}\n");
    }
    acf.push_str("\t}\n");
    acf.push_str("\t\"UserConfig\"\n");
    acf.push_str("\t{\n");
    acf.push_str("\t\t\"platform_override_dest\"\t\t\"linux\"\n");
    acf.push_str("\t\t\"platform_override_source\"\t\t\"windows\"\n");
    acf.push_str("\t}\n");
    acf.push_str("\t\"MountedConfig\"\n");
    acf.push_str("\t{\n");
    acf.push_str("\t\t\"platform_override_dest\"\t\t\"linux\"\n");
    acf.push_str("\t\t\"platform_override_source\"\t\t\"windows\"\n");
    acf.push_str("\t}\n");
    acf.push_str("}\n");

    match std::fs::write(&acf_path, &acf) {
        Ok(_) => steps.push(format!("Created ACF: {}", acf_path.display())),
        Err(e) => warnings.push(format!("Failed to create ACF: {e}")),
    }

    // 2. Restart Steam
    crate::log_to_temp(&format!("[depot-dl] Restarting Steam for app {}", app_id));
    let kill_result = std::process::Command::new("pkill")
        .args(["-f", "[Ss]team"])
        .output();
    match kill_result {
        Ok(o) if !o.status.success() => {
            crate::log_to_temp("[depot-dl] pkill returned non-zero (no Steam processes?)");
        }
        Err(e) => {
            warnings.push(format!("Failed to kill Steam: {e}"));
        }
        _ => {}
    }

    std::thread::sleep(Duration::from_secs(2));

    let start_result = std::process::Command::new("steam").spawn();
    match start_result {
        Ok(_) => steps.push("Steam restarted".to_string()),
        Err(e) => warnings.push(format!("Failed to restart Steam: {e}")),
    }

    Ok(json!({
        "ok": warnings.is_empty(),
        "steps": steps,
        "warnings": warnings,
    }))
}

/// Full post-download Steam integration (8 steps matching luma-lite).
/// Creates ACF, moves manifests to depotcache, updates VDF, adds to SLS Steam config.
fn post_download_steam_integration(
    app_id: u64,
    game_name: &str,
    output_dir: &Path,
    depots: &[DepotSelection],
) -> Result<Vec<String>, String> {
    let steam = steam_root().ok_or("Steam root not found")?;
    let mut steps: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();

    crate::log_to_temp(&format!("[depot-dl] Integration start for app {}", app_id));

    // Step 1: Detect Steam library (use steam_root directly)
    let library_path = &steam;
    steps.push(format!("Using Steam library: {}", library_path.display()));
    crate::log_to_temp(&format!("[depot-dl] Step 1: Steam library = {}", library_path.display()));

    // Step 2: Ensure steamapps directory structure
    let steamapps = library_path.join("steamapps");
    let common = steamapps.join("common");
    let depotcache = library_path.join("depotcache");
    if let Err(e) = std::fs::create_dir_all(&steamapps) { warnings.push(format!("mkdir steamapps: {e}")); }
    if let Err(e) = std::fs::create_dir_all(&common) { warnings.push(format!("mkdir common: {e}")); }
    if let Err(e) = std::fs::create_dir_all(&depotcache) { warnings.push(format!("mkdir depotcache: {e}")); }
    steps.push("Ensured steamapps structure".to_string());
    crate::log_to_temp(&format!("[depot-dl] Step 2: steamapps structure ensured, warnings: {:?}", warnings));

    // Step 3: Build safe_name / installdir
    let safe_name: String = game_name
        .chars()
        .map(|c| if c.is_alphanumeric() || c == ' ' || c == '-' || c == '.' { c } else { '_' })
        .collect::<String>()
        .trim()
        .replace(' ', "_");
    let installdir = if safe_name.is_empty() {
        format!("App_{}", app_id)
    } else {
        safe_name
    };
    crate::log_to_temp(&format!("[depot-dl] Step 3: installdir = {}", installdir));

    // Step 4: Move manifests to depotcache
    let download_base = output_dir.parent().unwrap_or(output_dir); // steamapps/common -> steamapps
    let mut moved_count: u32 = 0;
    for depot in depots {
        let manifest_gid = &depot.manifest_id;
        if !manifest_gid.is_empty() && manifest_gid != "0" {
            let filename = format!("{}_{}.manifest", depot.depot_id, manifest_gid);
            let search_paths = [
                depotcache.join(&filename),                                          // <steam_root>/depotcache/ (HubcapDB puts them here)
                depotcache.join(&format!("../depotcache/{}", filename)),             // fallback: same but resolved
                download_base.join(&installdir).join(".DepotDownloader").join(&filename), // <steamapps>/<installdir>/.DepotDownloader/
                output_dir.join(".DepotDownloader").join(&filename),                 // <steamapps>/common/.DepotDownloader/
                output_dir.join(&installdir).join(&filename),                       // <steamapps>/<installdir>/
                download_base.join(&filename),                                      // <steamapps>/
                output_dir.join(&filename),                                         // <steamapps>/common/
            ];
            let mut found = false;
            for src in &search_paths {
                if src.exists() {
                    let dest = depotcache.join(&filename);
                    match std::fs::copy(src, &dest) {
                        Ok(_) => { moved_count += 1; found = true; break; }
                        Err(e) => warnings.push(format!("copy manifest {}: {}", filename, e)),
                    }
                }
            }
            if !found {
                let paths_debug: Vec<String> = search_paths.iter().map(|p| p.display().to_string()).collect();
                crate::log_to_temp(&format!("[depot-dl] Step 4: manifest {} not found. Searched: {:?}", filename, paths_debug));
            }
        }
    }
    steps.push(format!("Moved {} manifest(s) to depotcache", moved_count));
    crate::log_to_temp(&format!("[depot-dl] Step 4: moved {} manifests", moved_count));

    // Step 5: Create appmanifest_{appid}.acf
    // lua_dir() is non-fatal — if missing, create ACF without extra depot keys
    let keys = match lua_dir() {
        Some(ref lua) => resolve_keys_from_lua(lua, app_id),
        None => {
            crate::log_to_temp(&format!("[depot-dl] Step 5: lua_dir not found, creating ACF without keys"));
            std::collections::HashMap::new()
        }
    };

    let acf_path = steamapps.join(format!("appmanifest_{}.acf", app_id));
    let mut acf = String::new();
    acf.push_str("\"AppState\"\n{\n");
    acf.push_str(&format!("\t\"appid\"\t\t\"{}\"\n", app_id));
    acf.push_str("\t\"Universe\"\t\t\"1\"\n");
    acf.push_str(&format!("\t\"name\"\t\t\"{}\"\n", game_name.replace('\\', "\\\\").replace('"', "\\\"")));
    acf.push_str("\t\"StateFlags\"\t\t\"4\"\n");
    acf.push_str(&format!("\t\"installdir\"\t\t\"{}\"\n", installdir));
    acf.push_str(&format!("\t\"SizeOnDisk\"\t\t\"0\"\n"));
    acf.push_str("\t\"buildid\"\t\t\"0\"\n");
    acf.push_str("\t\"InstalledDepots\"\n");
    acf.push_str("\t{\n");
    for depot in depots {
        let manifest_gid = &depot.manifest_id;
        acf.push_str(&format!("\t\t\"{}\"\n", depot.depot_id));
        acf.push_str("\t\t{\n");
        acf.push_str(&format!("\t\t\t\"manifest\"\t\t\"{}\"\n", manifest_gid));
        if depot.size > 0 {
            acf.push_str(&format!("\t\t\t\"size\"\t\t\"{}\"\n", depot.size));
        }
        acf.push_str("\t\t}\n");
    }
    for (depot_id, _key) in &keys {
        if !depots.iter().any(|d| d.depot_id == *depot_id) {
            acf.push_str(&format!("\t\t\"{}\"\n", depot_id));
            acf.push_str("\t\t{\n");
            acf.push_str(&format!("\t\t\t\"manifest\"\t\t\"\"\n"));
            acf.push_str("\t\t}\n");
        }
    }
    acf.push_str("\t}\n");
    acf.push_str("\t\"UserConfig\"\n");
    acf.push_str("\t{\n");
    acf.push_str("\t\t\"platform_override_dest\"\t\t\"linux\"\n");
    acf.push_str("\t\t\"platform_override_source\"\t\t\"windows\"\n");
    acf.push_str("\t}\n");
    acf.push_str("\t\"MountedConfig\"\n");
    acf.push_str("\t{\n");
    acf.push_str("\t\t\"platform_override_dest\"\t\t\"linux\"\n");
    acf.push_str("\t\t\"platform_override_source\"\t\t\"windows\"\n");
    acf.push_str("\t}\n");
    acf.push_str("}\n");

    match std::fs::write(&acf_path, &acf) {
        Ok(_) => {
            steps.push(format!("Created ACF: {}", acf_path.display()));
            crate::log_to_temp(&format!("[depot-dl] Step 5: ACF created at {}", acf_path.display()));
        }
        Err(e) => {
            warnings.push(format!("Failed to create ACF: {e}"));
            crate::log_to_temp(&format!("[depot-dl] Step 5: FAILED to create ACF: {e}"));
        }
    }

    // Step 6: Update libraryfolders.vdf
    crate::log_to_temp(&format!("[depot-dl] Step 6: Updating libraryfolders.vdf"));
    let vdf_path = steamapps.join("libraryfolders.vdf");
    let app_id_str = app_id.to_string();
    let vdf_result = if vdf_path.exists() {
        std::fs::read_to_string(&vdf_path).map_err(|e| format!("Failed to read VDF: {e}"))
    } else {
        Ok(format!(
            "\"libraryfolders\"\n{{\n\t\"0\"\n\t{{\n\t\t\"path\"\t\t\"{}\"\n\t\t\"label\"\t\t\"\"\n\t\t\"apps\"\n\t\t{{\n\t\t}}\n\t}}\n}}\n",
            library_path.display()
        ))
    };
    match vdf_result {
        Ok(mut content) => {
            if !content.contains(&format!("\"{}\"", app_id_str)) {
                if let Some(pos) = content.find("\"apps\"") {
                    if let Some(brace_pos) = content[pos..].find('{') {
                        let insert_at = pos + brace_pos + 1;
                        let line = format!("\t\t\t\t\"{}\"\t\t\"0\"\n", app_id_str);
                        content.insert_str(insert_at, &line);
                        match std::fs::write(&vdf_path, &content) {
                            Ok(_) => {
                                steps.push("Updated libraryfolders.vdf".to_string());
                                crate::log_to_temp("[depot-dl] Step 6: VDF updated successfully");
                            }
                            Err(e) => {
                                warnings.push(format!("Failed to update VDF: {e}"));
                                crate::log_to_temp(&format!("[depot-dl] Step 6: FAILED to write VDF: {e}"));
                            }
                        }
                    } else {
                        warnings.push("No '{' after 'apps' in VDF".to_string());
                        crate::log_to_temp("[depot-dl] Step 6: No '{' found after 'apps' in VDF");
                    }
                } else {
                    warnings.push("No 'apps' section found in VDF".to_string());
                    crate::log_to_temp("[depot-dl] Step 6: No 'apps' section found in VDF");
                }
            } else {
                steps.push("VDF already contains app".to_string());
                crate::log_to_temp(&format!("[depot-dl] Step 6: VDF already contains app {}", app_id_str));
            }
        }
        Err(e) => {
            warnings.push(e.clone());
            crate::log_to_temp(&format!("[depot-dl] Step 6: FAILED to read VDF: {e}"));
        }
    }

    // Step 7: Add to SLS Steam AdditionalApps config
    crate::log_to_temp(&format!("[depot-dl] Step 7: Updating SLS Steam config"));
    let sls_config_path = std::env::var("XDG_CONFIG_HOME")
        .map(|x| PathBuf::from(x).join("SLSsteam").join("config.yaml"))
        .unwrap_or_else(|_| {
            get_home_dir()
                .join(".config").join("SLSsteam").join("config.yaml")
        });
    crate::log_to_temp(&format!("[depot-dl] Step 7: SLS config path = {} (exists={})", sls_config_path.display(), sls_config_path.exists()));
    if sls_config_path.exists() {
        match std::fs::read_to_string(&sls_config_path) {
            Ok(mut content) => {
                if !content.contains(&app_id_str) {
                    if let Some(pos) = content.find("AdditionalApps:") {
                        let section_start = pos + "AdditionalApps:".len();
                        let rest = &content[section_start..];
                        let last_item_end = rest.rfind("- ").map(|p| {
                            let line_end = rest[p..].find('\n').unwrap_or(rest.len() - p);
                            section_start + p + line_end + 1
                        }).unwrap_or(section_start + 1);
                        let entry = format!("  - {}   # {}\n", app_id_str, game_name);
                        content.insert_str(last_item_end, &entry);
                    } else {
                        content.push_str(&format!("\nAdditionalApps:\n  - {}   # {}\n", app_id_str, game_name));
                    }
                    match std::fs::write(&sls_config_path, &content) {
                        Ok(_) => {
                            steps.push(format!("Added app {} to SLS Steam AdditionalApps", app_id_str));
                            crate::log_to_temp(&format!("[depot-dl] Step 7: Added {} to SLS config", app_id_str));
                        }
                        Err(e) => {
                            warnings.push(format!("Failed to update SLS Steam config: {e}"));
                            crate::log_to_temp(&format!("[depot-dl] Step 7: FAILED to write SLS config: {e}"));
                        }
                    }
                } else {
                    steps.push("App already in SLS Steam AdditionalApps".to_string());
                    crate::log_to_temp(&format!("[depot-dl] Step 7: {} already in SLS config", app_id_str));
                }
            }
            Err(e) => {
                warnings.push(format!("Failed to read SLS config: {e}"));
                crate::log_to_temp(&format!("[depot-dl] Step 7: FAILED to read SLS config: {e}"));
            }
        }
    } else {
        steps.push("SLS Steam not installed, skipping config".to_string());
        crate::log_to_temp(&format!("[depot-dl] Step 7: SLS config not found at {}", sls_config_path.display()));
    }

    if !warnings.is_empty() {
        crate::log_to_temp(&format!("[depot-dl] Post-download warnings for app {}: {:?}", app_id, warnings));
    }

    crate::log_to_temp(&format!("[depot-dl] Integration complete for app {}: {} steps, {} warnings", app_id, steps.len(), warnings.len()));
    Ok(steps)
}

fn get_home_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

fn update_job<F>(job_id: &str, f: F) -> Result<(), String>
where
    F: FnOnce(&mut JobState),
{
    let mut jobs = JOBS.lock().map_err(|e| e.to_string())?;
    if let Some(job) = jobs.get_mut(job_id) {
        f(job);
        // Flush progress to disk every 5 seconds so Steam restarts preserve state
        if job.last_disk_flush.elapsed() > std::time::Duration::from_secs(5)
            && (job.status == "downloading" || job.status == "integrating" || job.status == "validating")
        {
            job.last_disk_flush = Instant::now();
            let snapshot = (job.progress, job.bytes_read, job.total_bytes, job.status.clone(), job.phase.clone());
            drop(jobs);
            let _ = flush_progress_to_queue(job_id, snapshot);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Persistent download queue
// ---------------------------------------------------------------------------

fn flush_progress_to_queue(job_id: &str, snapshot: (f64, u64, u64, String, String)) -> Result<(), String> {
    let mut qf = load_queue();
    let (progress, bytes_read, total_bytes, status, _phase) = snapshot;
    for item in &mut qf.queue {
        if item.job_id.as_deref() == Some(job_id) {
            item.progress = progress;
            item.bytes_downloaded = bytes_read;
            item.total_bytes = total_bytes;
            item.status = status;
            break;
        }
    }
    save_queue(&qf);
    Ok(())
}

fn queue_file_path() -> PathBuf {
    staging_dir().join("downloads_queue.json")
}

fn generate_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ts = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis();
    let rand: u32 = rand::random();
    format!("{}-{:08x}", ts, rand)
}

fn now_epoch() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

pub fn load_queue() -> QueueFile {
    let path = queue_file_path();
    match std::fs::read_to_string(&path) {
        Ok(content) => serde_json::from_str(&content).unwrap_or(QueueFile { queue: Vec::new(), history: Vec::new() }),
        Err(_) => QueueFile { queue: Vec::new(), history: Vec::new() },
    }
}

fn save_queue(qf: &QueueFile) {
    let path = queue_file_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(qf) {
        let _ = std::fs::write(&path, json);
    }
}

pub fn add_to_queue(mut item: QueueItem) -> String {
    let id = generate_id();
    item.id = id.clone();
    item.status = "queued".to_string();
    item.job_id = None;
    item.progress = 0.0;
    item.bytes_downloaded = 0;
    item.added_at = now_epoch();
    item.started_at = None;
    item.completed_at = None;
    item.error = None;

    let mut qf = load_queue();

    // Calculate total_bytes from depots
    item.total_bytes = item.depots.iter().map(|d| d.size).sum();

    qf.queue.push(item);
    save_queue(&qf);

    // If nothing is downloading, start next
    let has_downloading = qf.queue.iter().any(|j| j.status == "downloading");
    if !has_downloading {
        start_next_in_queue();
    }

    id
}

pub fn get_queue() -> QueueFile {
    let mut qf = load_queue();

    // Merge live progress from JOBS HashMap
    if let Ok(jobs) = JOBS.lock() {
        for item in &mut qf.queue {
            if let Some(ref job_id) = item.job_id {
                if let Some(job) = jobs.get(job_id) {
                    item.progress = job.progress;
                    item.bytes_downloaded = job.bytes_read;
                    item.status = job.status.clone();
                    item.total_bytes = job.total_bytes;
                }
            }
        }
    }

    // Detect orphaned "downloading" or "integrating" items (process died with Steam)
    let mut needs_save = false;
    for item in &mut qf.queue {
        if (item.status == "downloading" || item.status == "integrating") && item.job_id.is_some() {
            let job_id = item.job_id.as_ref().unwrap().clone();
            let is_alive = JOBS.lock().map(|j| j.contains_key(&job_id)).unwrap_or(false);
            if !is_alive {
                // Job process no longer exists (Steam restarted) — mark as interrupted
                item.status = "interrupted".to_string();
                item.job_id = None;
                needs_save = true;
            }
        }
    }
    if needs_save {
        save_queue(&qf);
    }

    qf
}

pub fn remove_from_queue(id: &str) -> bool {
    let mut qf = load_queue();
    let before_len = qf.queue.len();
    qf.queue.retain(|j| j.id != id);
    let removed = qf.queue.len() < before_len;
    if removed {
        save_queue(&qf);
    }
    removed
}

pub fn start_next_in_queue() {
    let mut qf = load_queue();

    // Find next queued or interrupted item
    let next_idx = qf.queue.iter().position(|j| j.status == "queued" || j.status == "interrupted");
    let next_idx = match next_idx {
        Some(i) => i,
        None => return, // Nothing queued
    };

    // Mark as downloading
    qf.queue[next_idx].status = "downloading".to_string();
    qf.queue[next_idx].started_at = Some(now_epoch());

    let item = qf.queue[next_idx].clone();
    save_queue(&qf);

    // Build DepotDownloadJob
    let job = DepotDownloadJob {
        job_id: None,
        app_id: item.app_id,
        game_name: item.game_name.clone(),
        output_dir: item.output_dir.clone(),
        depots: item.depots.clone(),
    };

    // Start download
    match start_download(job) {
        Ok(job_id) => {
            // Update queue item with the job_id
            let mut qf2 = load_queue();
            if let Some(qi) = qf2.queue.iter_mut().find(|j| j.id == item.id) {
                qi.job_id = Some(job_id);
            }
            save_queue(&qf2);
        }
        Err(e) => {
            on_download_complete(&item.id, &e);
        }
    }
}

pub fn on_download_complete(queue_id: &str, error: &str) {
    let mut qf = load_queue();

    // Find the item
    let item_idx = qf.queue.iter().position(|j| j.id == queue_id);
    let item_idx = match item_idx {
        Some(i) => i,
        None => return,
    };

    let item = qf.queue.remove(item_idx);
    let is_error = !error.is_empty();
    let now = now_epoch();

    // Get progress from JOBS if available
    let (final_progress, final_bytes) = if let Some(job_id) = &item.job_id {
        JOBS.lock().map(|jobs| {
            jobs.get(job_id).map(|j| (j.progress, j.bytes_read)).unwrap_or((item.progress, item.bytes_downloaded))
        }).unwrap_or((item.progress, item.bytes_downloaded))
    } else {
        (item.progress, item.bytes_downloaded)
    };

    // Add to history
    let history_item = HistoryItem {
        id: item.id,
        app_id: item.app_id,
        game_name: item.game_name,
        status: if is_error { "failed".to_string() } else { "completed".to_string() },
        progress: if is_error { final_progress } else { 100.0 },
        bytes_downloaded: final_bytes,
        total_bytes: item.total_bytes,
        completed_at: now,
        error: if is_error { Some(error.to_string()) } else { None },
    };

    // Keep only last 20 history items
    qf.history.insert(0, history_item);
    if qf.history.len() > 20 {
        qf.history.truncate(20);
    }

    save_queue(&qf);

    // Start next in queue
    start_next_in_queue();
}

pub fn get_queue_status_for_job(job_id: &str) -> Option<QueueItem> {
    let qf = load_queue();
    qf.queue.into_iter().find(|j| j.job_id.as_deref() == Some(job_id))
}

fn find_queue_id_for_job(job_id: &str) -> Option<String> {
    let qf = load_queue();
    qf.queue.iter().find(|j| j.job_id.as_deref() == Some(job_id)).map(|j| j.id.clone())
}

fn find_job_id_for_queue_id(queue_id: &str) -> Option<String> {
    let qf = load_queue();
    qf.queue.iter()
        .find(|j| j.id == queue_id)
        .and_then(|j| j.job_id.clone())
}

pub fn pause_queue_item(queue_id: &str) -> Result<bool, String> {
    let job_id = find_job_id_for_queue_id(queue_id)
        .ok_or("Queue item not found or no active job")?;
    pause_download(&job_id)
}

pub fn resume_queue_item(queue_id: &str) -> Result<bool, String> {
    let job_id = find_job_id_for_queue_id(queue_id)
        .ok_or("Queue item not found or no active job")?;
    resume_download(&job_id)
}

pub fn start_next() {
    start_next_in_queue();
}

pub fn clear_history() {
    let mut qf = load_queue();
    qf.history.clear();
    save_queue(&qf);
}

pub fn remove_history_item(id: &str) -> bool {
    let mut qf = load_queue();
    let before = qf.history.len();
    qf.history.retain(|h| h.id != id);
    if qf.history.len() < before {
        save_queue(&qf);
        true
    } else {
        false
    }
}
