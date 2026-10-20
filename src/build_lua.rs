use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::sync::{OnceLock, RwLock};

// ---------------------------------------------------------------------------
// Build-Locked Lua provider — same job machinery as steam_keys so the
// frontend poll (GET /api/download-status/{id}) works unchanged.
//
// Routes:
//   POST /api/download               (only when sourceId == "build-lua")
//   GET  /api/download-status/{id}   (for our own jobs only)
// ---------------------------------------------------------------------------

const MAX_LUA_BYTES: usize = 1024 * 1024; // 1 MB
const DOWNLOAD_TIMEOUT_SECS: u64 = 30; // files are a few KB

// ---------------------------------------------------------------------------
// Download job — same shape as package_installer/steam_keys
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize)]
struct BuildLuaJob {
    status: String,
    progress: u8,
    message: String,
    error: Option<String>,
    app_id: String,
    lua_count: u32,
    manifest_count: u32,
}

fn jobs() -> &'static RwLock<HashMap<String, BuildLuaJob>> {
    static INSTANCE: OnceLock<RwLock<HashMap<String, BuildLuaJob>>> = OnceLock::new();
    INSTANCE.get_or_init(|| RwLock::new(HashMap::new()))
}

fn set_job(request_id: &str, job: BuildLuaJob) {
    if let Ok(mut map) = jobs().write() {
        map.insert(request_id.to_string(), job);
    }
}

fn update_job<F: FnOnce(&mut BuildLuaJob)>(request_id: &str, f: F) {
    if let Ok(mut map) = jobs().write() {
        if let Some(job) = map.get_mut(request_id) {
            f(job);
        }
    }
}

fn status_response(request_id: &str) -> (u16, String) {
    let map = jobs().read().unwrap();
    match map.get(request_id) {
        Some(job) => (
            200,
            json!({
                "ok": true,
                "status": job.status,
                "progress": job.progress,
                "message": job.message,
                "errorCode": job.error,
                "appId": job.app_id,
                "luaCount": job.lua_count,
                "manifestCount": job.manifest_count
            })
            .to_string(),
        ),
        None => (
            200,
            json!({
                "ok": false,
                "status": "failed",
                "progress": 0,
                "message": "Download not found",
                "errorCode": "NOT_FOUND"
            })
            .to_string(),
        ),
    }
}

fn fail_job(request_id: &str, code: &str, message: &str) {
    update_job(request_id, |job| {
        job.status = "failed".to_string();
        job.progress = 100;
        job.message = message.to_string();
        job.error = Some(code.to_string());
    });
    crate::log_to_temp(&format!(
        "[build-lua] job {} failed ({}): {}",
        request_id, code, message
    ));
}

// ---------------------------------------------------------------------------
// Catalog lookup — the download URL comes from our own fixes catalog
// (cached 5 min by game_fix::fetch_catalog), never from the client.
// ---------------------------------------------------------------------------

fn find_lua_entry(app_id: &str) -> Result<(String, Option<String>), String> {
    let body = crate::game_fix::fetch_catalog()?;
    let v: Value =
        serde_json::from_str(&body).map_err(|e| format!("catalog parse: {e}"))?;
    let entries = v
        .get("entries")
        .and_then(|e| e.as_array())
        .ok_or_else(|| "catalog has no entries".to_string())?;
    for entry in entries {
        if entry.get("type").and_then(|t| t.as_str()) != Some("lua") {
            continue;
        }
        if entry.get("appId").and_then(|a| a.as_str()) == Some(app_id) {
            let url = entry
                .get("downloadUrl")
                .and_then(|d| d.as_str())
                .unwrap_or("")
                .to_string();
            let build = entry
                .get("buildNumber")
                .and_then(|b| b.as_str())
                .map(|s| s.to_string());
            if url.is_empty() {
                return Err("Lua entry has no downloadUrl".to_string());
            }
            return Ok((url, build));
        }
    }
    Err(format!("No build-locked Lua entry for app {app_id}"))
}

// ---------------------------------------------------------------------------
// Job — download the .lua from R2 and place it in Steam/config/lua/{appId}.lua
// ---------------------------------------------------------------------------

fn start_build_lua_job(app_id: u64) -> String {
    let request_id = format!(
        "{}-{}-{}",
        app_id,
        std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        rand::random::<u16>()
    );

    set_job(
        &request_id,
        BuildLuaJob {
            status: "queued".to_string(),
            progress: 0,
            message: "Queued".to_string(),
            error: None,
            app_id: app_id.to_string(),
            lua_count: 0,
            manifest_count: 0,
        },
    );

    let rid = request_id.clone();
    std::thread::spawn(move || {
        // 1. Resolve the download URL from the fixes catalog.
        update_job(&rid, |job| {
            job.status = "downloading".to_string();
            job.progress = 10;
            job.message = "Downloading Lua script...".to_string();
        });
        let app_id_str = app_id.to_string();
        let (download_url, build) = match find_lua_entry(&app_id_str) {
            Ok(found) => found,
            Err(e) => {
                fail_job(&rid, "NOT_IN_CATALOG", &e);
                return;
            }
        };

        // 2. Download with a 30s budget (files are a few KB).
        let client = match reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(DOWNLOAD_TIMEOUT_SECS))
            .danger_accept_invalid_certs(true)
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                fail_job(&rid, "CLIENT_ERROR", &format!("HTTP client error: {e}"));
                return;
            }
        };
        let resp = match client.get(&download_url).send() {
            Ok(r) => r,
            Err(e) => {
                fail_job(&rid, "NETWORK_ERROR", &format!("Download failed: {e}"));
                return;
            }
        };
        if !resp.status().is_success() {
            let code = format!("HTTP_{}", resp.status().as_u16());
            fail_job(&rid, &code, &format!("Download failed: HTTP {}", resp.status()));
            return;
        }
        let bytes = match resp.bytes() {
            Ok(b) => b,
            Err(e) => {
                fail_job(&rid, "READ_ERROR", &format!("Failed to read response: {e}"));
                return;
            }
        };
        if bytes.is_empty() || bytes.len() > MAX_LUA_BYTES {
            fail_job(&rid, "INVALID_LUA", "Invalid Lua file size");
            return;
        }
        let content = String::from_utf8_lossy(&bytes).to_string();
        if !content.contains("addappid") {
            fail_job(&rid, "INVALID_LUA", "Not a valid Lua script");
            return;
        }

        // 3. Write to Steam/config/lua/{appId}.lua (backup existing).
        update_job(&rid, |job| {
            job.status = "processing".to_string();
            job.progress = 70;
            job.message = "Installing Lua script...".to_string();
        });
        let steam_root = match crate::depot_downloader::steam_root() {
            Some(p) => p,
            None => {
                fail_job(&rid, "STEAM_NOT_FOUND", "Steam installation not found");
                return;
            }
        };
        let lua_dir = steam_root.join("config").join("lua");
        if let Err(e) = fs::create_dir_all(&lua_dir) {
            fail_job(&rid, "WRITE_ERROR", &format!("Failed to create lua directory: {e}"));
            return;
        }
        let dest = lua_dir.join(format!("{app_id}.lua"));
        if dest.exists() {
            let bak = dest.with_extension("lua.bak");
            let _ = fs::rename(&dest, &bak);
        }
        if let Err(e) = fs::write(&dest, content.as_bytes()) {
            fail_job(&rid, "WRITE_ERROR", &format!("Write failed: {e}"));
            return;
        }

        // 4. Complete.
        let build_note = build
            .map(|b| format!(" (build {})", b))
            .unwrap_or_default();
        update_job(&rid, |job| {
            job.status = "completed".to_string();
            job.progress = 100;
            job.lua_count = 1;
            job.message = format!("Lua script installed for app {}{}", app_id, build_note);
        });
        crate::log_to_temp(&format!(
            "[build-lua] installed {} for app {}{}",
            dest.display(),
            app_id,
            build_note
        ));
    });

    request_id
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

fn body_app_id(body: &str) -> Option<(u64, Option<String>)> {
    let v: Value = serde_json::from_str(body).ok()?;
    let app = v.get("appId").or_else(|| v.get("app_id"))?;
    let app_id = app
        .as_u64()
        .or_else(|| app.as_str().and_then(|s| s.parse::<u64>().ok()))?;
    let source = v
        .get("sourceId")
        .or_else(|| v.get("source_id"))
        .and_then(|s| s.as_str())
        .map(|s| s.to_string());
    Some((app_id, source))
}

pub fn try_handle_route(method: &str, path: &str, body: &str) -> Option<(u16, String)> {
    // Intercept POST /api/download for the build-lua source only — before
    // package_installer claims it (a raw .lua is not a ZIP package).
    if method == "POST" && path == "/api/download" {
        if let Some((app_id, source)) = body_app_id(body) {
            if source.as_deref() == Some("build-lua") {
                let request_id = start_build_lua_job(app_id);
                return Some((200, json!({"ok": true, "requestId": request_id}).to_string()));
            }
        }
        return None;
    }

    // Serve download-status for our own jobs; fall through otherwise.
    if method == "GET" && path.starts_with("/api/download-status/") {
        let id = path.trim_start_matches("/api/download-status/");
        let known = jobs().read().map(|m| m.contains_key(id)).unwrap_or(false);
        if known {
            return Some(status_response(id));
        }
        return None;
    }

    None
}
