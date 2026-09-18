use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use crate::platform;

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

fn resolve_keys_from_lua(lua_dir: &Path, app_id: u64) -> HashMap<u64, String> {
    let mut keys = HashMap::new();

    let lua_path = lua_dir.join(format!("{app_id}.lua"));
    if lua_path.exists() {
        if let Ok(content) = std::fs::read_to_string(&lua_path) {
            for entry in parse_lua_content(&content) {
                if let Some(key) = &entry.key {
                    keys.insert(entry.depot_id, key.clone());
                }
            }
        }
    }

    let disabled_path = lua_dir.join(format!("{app_id}.lua.disabled"));
    if disabled_path.exists() {
        if let Ok(content) = std::fs::read_to_string(&disabled_path) {
            for entry in parse_lua_content(&content) {
                if let Some(key) = &entry.key {
                    keys.entry(entry.depot_id).or_insert_with(|| key.clone());
                }
            }
        }
    }

    keys
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DepotInfo {
    pub depot_id: u64,
    pub name: String,
    pub manifest_id: String,
    pub manifest_path: Option<String>,
    pub size: u64,
    pub key: Option<String>,
    pub encrypted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DepotDownloadJob {
    pub job_id: String,
    pub app_id: u64,
    pub game_name: String,
    pub output_dir: String,
    pub depots: Vec<DepotInfo>,
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
}

static JOBS: LazyLock<Mutex<HashMap<String, JobState>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static PAUSED_PIDS: LazyLock<Mutex<Vec<u32>>> = LazyLock::new(|| Mutex::new(Vec::new()));

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn steam_root() -> Option<PathBuf> {
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

/// Resolve depots for a given app_id from .lua files + manifest parsing.
pub fn resolve_depots(app_id: u64) -> Result<Vec<DepotInfo>, String> {
    let lua_dir = lua_dir().ok_or("Lua directory not found")?;
    let keys = resolve_keys_from_lua(&lua_dir, app_id);

    if keys.is_empty() {
        return Err(format!("No depot keys found for app {}. Make sure the .lua file is installed.", app_id));
    }

    let mut depots: Vec<DepotInfo> = Vec::new();
    let depotcache = depotcache_dir();

    for (depot_id, key) in &keys {
        // Find and parse the actual .manifest file using manifest_parser
        let (manifest_id, manifest_path_str, size, encrypted) = if let Some(dc) = &depotcache {
            match find_manifest_for_depot(dc, *depot_id) {
                Some(path) => {
                    if let Some(info) = crate::manifest_parser::try_read_manifest(&path) {
                        (
                            info.gid_manifest.to_string(),
                            Some(path.to_string_lossy().to_string()),
                            info.size_on_disk,
                            info.filenames_encrypted,
                        )
                    } else {
                        (String::new(), None, 0, false)
                    }
                }
                None => (String::new(), None, 0, false),
            }
        } else {
            (String::new(), None, 0, false)
        };

        depots.push(DepotInfo {
            depot_id: *depot_id,
            name: format!("Depot {}", depot_id),
            manifest_id,
            manifest_path: manifest_path_str,
            size,
            key: Some(key.clone()),
            encrypted: !key.is_empty() || encrypted,
        });
    }

    depots.sort_by_key(|d| d.depot_id);
    Ok(depots)
}

/// Find a manifest file for a depot in the depotcache, parsing it to validate.
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
            // Parse the manifest to validate it matches this depot
            if let Some(info) = crate::manifest_parser::try_read_manifest(&path) {
                if info.depot_id == depot_id {
                    return Some(path);
                }
            }
        }
    }

    None
}

/// Start a depot download job.
pub fn start_download(job: DepotDownloadJob) -> Result<String, String> {
    let exe = depot_downloader_exe()
        .ok_or("DepotDownloaderMod is not installed. Install it from Settings > Third-Party Tools.")?;

    let lua_dir = lua_dir().ok_or("Lua directory not found")?;
    let keys = resolve_keys_from_lua(&lua_dir, job.app_id);
    if keys.is_empty() {
        return Err("No depot keys found".to_string());
    }

    // Write keys file
    let staging = staging_dir();
    std::fs::create_dir_all(&staging).ok();
    let keys_path = staging.join(format!("depotkeys_{}.txt", &job.job_id));
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
        jobs.insert(job.job_id.clone(), JobState {
            job_id: job.job_id.clone(),
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
        });
    }

    let job_id = job.job_id.clone();
    let job_id_return = job.job_id.clone();
    let app_id = job.app_id;
    let depots_clone = job.depots.clone();

    // Spawn download thread
    std::thread::spawn(move || {
        let mut cumulative_bytes: u64 = 0;

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

            let depot_output = output_dir.join(app_id.to_string());
            std::fs::create_dir_all(&depot_output).ok();

            args.push("-dir".to_string());
            args.push(depot_output.display().to_string());
            args.push("-max-downloads".to_string());
            args.push("32".to_string());
            args.push("-validate".to_string());

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
                            return;
                        }

                        std::thread::sleep(Duration::from_millis(500));
                    }
                    Err(e) => {
                        let _ = update_job(&job_id, |j| {
                            j.status = "failed".to_string();
                            j.error = Some(format!("Process wait error: {e}"));
                        });
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
                return;
            }

            cumulative_bytes += depot.size;
        }

        // All depots done
        let _ = update_job(&job_id, |j| {
            j.status = "completed".to_string();
            j.progress = 100.0;
            j.message = "Download completed".to_string();
        });
        let _ = std::fs::remove_file(&keys_path);
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

fn update_job<F>(job_id: &str, f: F) -> Result<(), String>
where
    F: FnOnce(&mut JobState),
{
    let mut jobs = JOBS.lock().map_err(|e| e.to_string())?;
    if let Some(job) = jobs.get_mut(job_id) {
        f(job);
    }
    Ok(())
}
