use serde_json::json;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;

const BRIDGE_PORT: u16 = 21775;
const BRIDGE_PORT_FALLBACK: u16 = 21776;
const CORS_HEADERS: &str = "\
Access-Control-Allow-Origin: *\r\n\
Access-Control-Allow-Methods: GET, POST, DELETE, OPTIONS\r\n\
Access-Control-Allow-Headers: Content-Type\r\n\
Access-Control-Max-Age: 86400\r\n";

pub fn start_bridge_server() {
    std::thread::spawn(|| {
        // Try primary port first, fall back to 21776
        let listener = match TcpListener::bind(format!("127.0.0.1:{}", BRIDGE_PORT)) {
            Ok(l) => {
                crate::log_to_temp(&format!(
                    "[bridge] Mini-bridge listening on port {}",
                    BRIDGE_PORT
                ));
                l
            }
            Err(e) => {
                crate::log_to_temp(&format!(
                    "[bridge] Port {} in use, trying fallback port {}: {}",
                    BRIDGE_PORT, BRIDGE_PORT_FALLBACK, e
                ));
                match TcpListener::bind(format!("127.0.0.1:{}", BRIDGE_PORT_FALLBACK)) {
                    Ok(l) => {
                        crate::log_to_temp(&format!(
                            "[bridge] Mini-bridge listening on fallback port {}",
                            BRIDGE_PORT_FALLBACK
                        ));
                        l
                    }
                    Err(e2) => {
                        crate::log_to_temp(&format!(
                            "[bridge] Failed to bind both ports {} and {}: {}, {}",
                            BRIDGE_PORT, BRIDGE_PORT_FALLBACK, e, e2
                        ));
                        return;
                    }
                }
            }
        };

        for stream in listener.incoming() {
            match stream {
                Ok(stream) => {
                    std::thread::spawn(move || {
                        handle_connection(stream);
                    });
                }
                Err(e) => {
                    crate::log_to_temp(&format!("[bridge] Accept error: {}", e));
                }
            }
        }
    });
}

fn handle_connection(mut stream: std::net::TcpStream) {
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .ok();
    stream
        .set_write_timeout(Some(std::time::Duration::from_secs(5)))
        .ok();

    let cloned = match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    };
    let mut reader = BufReader::new(cloned);

    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() {
        return;
    }

    let parts: Vec<&str> = request_line.trim().split_whitespace().collect();
    if parts.len() < 2 {
        return;
    }

    let method = parts[0];
    let path = parts[1];

    crate::log_to_temp(&format!(
        "[bridge] {} {} from {:?}",
        method,
        path,
        stream.peer_addr().ok()
    ));

    let mut content_length: usize = 0;
    let mut headers: Vec<(String, String)> = Vec::new();

    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() {
            return;
        }
        let trimmed = line.trim().to_string();
        if trimmed.is_empty() {
            break;
        }
        if let Some(pos) = trimmed.find(':') {
            let key = trimmed[..pos].trim().to_lowercase();
            let val = trimmed[pos + 1..].trim().to_string();
            if key == "content-length" {
                content_length = val.parse().unwrap_or(0);
            }
            headers.push((key, val));
        }
    }

    let body = if content_length > 0 {
        let mut buf = vec![0u8; content_length];
        if reader.read_exact(&mut buf).is_err() {
            return;
        }
        String::from_utf8_lossy(&buf).to_string()
    } else {
        String::new()
    };

    if method == "OPTIONS" {
        send_response(&mut stream, 204, "", "");
        return;
    }

    let (status, response_body) = route_request(method, path, &body);

    send_response(&mut stream, status, &response_body, "application/json");
}

fn handle_open_url(body: &str) -> (u16, String) {
    let parsed: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(e) => {
            return (
                400,
                json!({"ok": false, "message": format!("Invalid JSON: {e}")}).to_string(),
            )
        }
    };
    let url = parsed
        .get("url")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if !url.starts_with("https://") {
        return (
            400,
            json!({"ok": false, "message": "Only https:// URLs are allowed"}).to_string(),
        );
    }

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let mut cmd = std::process::Command::new("cmd");
        cmd.args(["/c", "start", "", url]).creation_flags(CREATE_NO_WINDOW);
        if let Err(e) = cmd.spawn() {
            return (
                500,
                json!({"ok": false, "message": format!("Failed to open URL: {e}")}).to_string(),
            );
        }
    }
    #[cfg(target_os = "linux")]
    {
        if let Err(e) = std::process::Command::new("xdg-open").arg(url).spawn() {
            return (
                500,
                json!({"ok": false, "message": format!("Failed to open URL: {e}")}).to_string(),
            );
        }
    }

    (200, json!({"ok": true}).to_string())
}

fn route_request(method: &str, path: &str, body: &str) -> (u16, String) {
    let query = path.splitn(2, '?').nth(1).unwrap_or("").to_string();
    let clean_path = path.splitn(2, '?').next().unwrap_or(path).to_string();

    // Health check endpoint (for bridge detection)
    if clean_path == "/health" && method == "GET" {
        return (200, json!({"status": "ok", "bridge": "lumaforge-cdp-proxy"}).to_string());
    }

    let headers_map: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let lua_req = crate::lua_backend::LuaRequest {
        method: method.to_string(),
        path: clean_path.clone(),
        body: body.to_string(),
        headers: headers_map,
        query,
    };

    // Steam Keys provider (lua generation, manifest fetch, pin/unpin) — must
    // intercept POST /api/download for sourceId==steamkeys before package_installer
    if let Some(rust_resp) = crate::steam_keys::try_handle_route(method, &clean_path, body) {
        return rust_resp;
    }

    // Rust-native routes: intercept binary-heavy operations before Lua
    #[cfg(target_os = "windows")]
    if let Some(rust_resp) = crate::package_installer::try_handle_route(method, &clean_path, body) {
        return rust_resp;
    }

    // Third-party tools (install/update/uninstall + list) — cross-platform
    if let Some(rust_resp) = crate::thirdparty::try_handle_route(method, &clean_path, body) {
        return rust_resp;
    }

    // Game fixes (apply/unfix SmokeAPI, Steamless, Goldberg, OnlineFix, catalog)
    if let Some(rust_resp) = crate::game_fix::try_handle_route(method, &clean_path, body) {
        return rust_resp;
    }

    // Steam account settings (Web API key, SteamID64/32, loginusers detect)
    if let Some(rust_resp) = crate::steam_account::try_handle_route(method, &clean_path, body) {
        return rust_resp;
    }

    // Open external URL (https only) — e.g. "Get API key" button
    if clean_path == "/api/open-url" && method == "POST" {
        return handle_open_url(body);
    }

    // Depot download routes (Linux only)
    #[cfg(target_os = "linux")]
    {
        if let Some(resp) = handle_depot_route(method, &clean_path, body) {
            return resp;
        }
        if let Some(resp) = handle_steam_library_folders(method, &clean_path) {
            return resp;
        }
        if let Some(resp) = handle_slssteam_route(method, &clean_path, body) {
            return resp;
        }
    }

    // Lua file management routes (cross-platform: config/lua lives under Steam root)
    {
        if let Some(resp) = handle_lua_files_route(method, &clean_path, body) {
            return resp;
        }
    }

    // Rust-native routes — run BEFORE Lua backend so our ACF detection wins
    if clean_path.starts_with("/api/local-status/") {
        let app_id = clean_path.trim_start_matches("/api/local-status/");
        return handle_local_status(app_id);
    }

    if let Some(lua_resp) = crate::lua_backend::handle_lua_request(&lua_req) {
        return (lua_resp.status, lua_resp.body);
    }

    if clean_path.starts_with("/api/sources/") {
        let app_id = clean_path.trim_start_matches("/api/sources/");
        handle_sources(app_id)
    } else if clean_path == "/api/providers" {
        handle_providers()
    } else if clean_path.starts_with("/api/open-library/") && method == "POST" {
        let _app_id = clean_path.trim_start_matches("/api/open-library/");
        (200, json!({"ok": true}).to_string())
    } else if clean_path == "/api/restart-steam" && method == "POST" {
        #[cfg(target_os = "linux")]
        {
            match crate::slssteam::kill_steam() {
                Ok(_) => {
                    std::thread::spawn(|| {
                        // Poll until Steam is dead (max 10s)
                        for _ in 0..20 {
                            std::thread::sleep(std::time::Duration::from_millis(500));
                            let alive = std::process::Command::new("pgrep")
                                .args(["-f", "[Ss]team"])
                                .output()
                                .map(|o| o.status.success())
                                .unwrap_or(false);
                            if !alive { break; }
                        }
                        crate::log_to_temp("[restart] Old Steam process terminated, starting new instance");
                        // Try with SLS first, fall back to plain Steam
                        match crate::slssteam::start_steam(true) {
                            Ok(_) => {
                                crate::log_to_temp("[restart] Steam started with SLS Steam");
                            }
                            Err(e) => {
                                crate::log_to_temp(&format!("[restart] SLS start failed: {}, trying plain start", e));
                                match crate::slssteam::start_steam(false) {
                                    Ok(_) => crate::log_to_temp("[restart] Steam started (no SLS)"),
                                    Err(e2) => crate::log_to_temp(&format!("[restart] Plain start also failed: {}", e2)),
                                }
                            }
                        }
                    });
                    (200, json!({"ok": true, "message": "Steam restarting"}).to_string())
                }
                Err(e) => (200, json!({"ok": false, "message": e}).to_string()),
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            crate::thirdparty::restart_steam_request()
        }
    } else {
        (404, json!({"error": "not found"}).to_string())
    }
}

fn handle_local_status(app_id: &str) -> (u16, String) {
    let mut in_library = false;
    let mut has_acf = false;

    if let Some(steam_root) = crate::depot_downloader::steam_root() {
        // Check if lua file exists → in library
        let lua_path = steam_root.join("config").join("lua").join(format!("{}.lua", app_id));
        if lua_path.exists() {
            in_library = true;
        }

        // Check ACF in default library
        let acf_name = format!("appmanifest_{}.acf", app_id);
        let default_acf = steam_root.join("steamapps").join(&acf_name);
        if default_acf.exists() {
            has_acf = true;
        }

        // Also check other library folders from libraryfolders.vdf
        if !has_acf {
            let vdf_path = steam_root.join("steamapps").join("libraryfolders.vdf");
            if let Ok(content) = std::fs::read_to_string(&vdf_path) {
                let mut current_path: Option<String> = None;
                for line in content.lines() {
                    let trimmed = line.trim();
                    if trimmed.starts_with("\"path\"") {
                        let rest = &trimmed[6..];
                        if let Some(q1) = rest.find('"') {
                            let after_q1 = &rest[q1 + 1..];
                            if let Some(q2) = after_q1.find('"') {
                                let value = &after_q1[..q2];
                                current_path = Some(value.replace("\\\\", "/").replace("\\", "/"));
                            }
                        }
                    }
                    if trimmed == "}" {
                        if let Some(ref p) = current_path {
                            let lib_acf = std::path::PathBuf::from(p).join("steamapps").join(&acf_name);
                            if lib_acf.exists() {
                                has_acf = true;
                                break;
                            }
                            current_path = None;
                        }
                    }
                }
            }
        }
    }

    let response = json!({
        "ok": true,
        "appId": app_id,
        "inLibrary": in_library,
        "has_acf": has_acf,
        "installed": has_acf
    });
    (200, response.to_string())
}

fn handle_sources(_app_id: &str) -> (u16, String) {
    let response = json!({
        "ok": true,
        "sources": [],
        "unavailableSources": [],
        "message": "No download sources available. Configure providers in Settings."
    });
    (200, response.to_string())
}

fn handle_providers() -> (u16, String) {
    let response = json!({
        "ok": true,
        "providers": [],
        "message": "No providers configured. Open Settings (gear icon) to configure providers."
    });
    (200, response.to_string())
}

// ---------------------------------------------------------------------------
// Depot download routes (Linux only)
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
fn handle_depot_route(method: &str, path: &str, body: &str) -> Option<(u16, String)> {
    use crate::depot_downloader;

    if path == "/api/depots" && method == "GET" {
        // This route is handled by the query param version below
        return None;
    }

    if path.starts_with("/api/depots/") && method == "GET" {
        let app_id_str = path.trim_start_matches("/api/depots/");
        if let Ok(app_id) = app_id_str.parse::<u64>() {
            match depot_downloader::resolve_depots(app_id) {
                Ok(result) => Some((200, json!({"ok": true, "depots": result.depots, "gameName": result.game_name, "outputDir": result.output_dir}).to_string())),
                Err(e) => Some((200, json!({"ok": false, "message": e}).to_string())),
            }
        } else {
            Some((400, json!({"ok": false, "message": "Invalid appId"}).to_string()))
        }
    } else if path == "/api/depot-download" && method == "POST" {
        let parsed: Result<serde_json::Value, _> = serde_json::from_str(body);
        match parsed {
            Ok(val) => {
                let job: Result<crate::depot_downloader::DepotDownloadJob, _> =
                    serde_json::from_value(val);
                match job {
                    Ok(j) => match depot_downloader::start_download(j) {
                        Ok(job_id) => Some((200, json!({"ok": true, "jobId": job_id}).to_string())),
                        Err(e) => Some((200, json!({"ok": false, "message": e}).to_string())),
                    },
                    Err(e) => Some((400, json!({"ok": false, "message": format!("Invalid job: {e}")}).to_string())),
                }
            }
            Err(e) => Some((400, json!({"ok": false, "message": format!("Invalid JSON: {e}")}).to_string())),
        }
    } else if path.starts_with("/api/depot-download-status/") && method == "GET" {
        let job_id = path.trim_start_matches("/api/depot-download-status/");
        Some((200, depot_downloader::get_status(job_id).to_string()))
    } else if path.starts_with("/api/depot-download-pause/") && method == "POST" {
        let job_id = path.trim_start_matches("/api/depot-download-pause/");
        match depot_downloader::pause_download(job_id) {
            Ok(ok) => Some((200, json!({"ok": ok}).to_string())),
            Err(e) => Some((200, json!({"ok": false, "message": e}).to_string())),
        }
    } else if path.starts_with("/api/depot-download-resume/") && method == "POST" {
        let job_id = path.trim_start_matches("/api/depot-download-resume/");
        match depot_downloader::resume_download(job_id) {
            Ok(ok) => Some((200, json!({"ok": ok}).to_string())),
            Err(e) => Some((200, json!({"ok": false, "message": e}).to_string())),
        }
    } else if path.starts_with("/api/depot-download-cancel/") && method == "POST" {
        let job_id = path.trim_start_matches("/api/depot-download-cancel/");
        match depot_downloader::cancel_download(job_id) {
            Ok(ok) => Some((200, json!({"ok": ok}).to_string())),
            Err(e) => Some((200, json!({"ok": false, "message": e}).to_string())),
        }
    } else if path.starts_with("/api/depot-post-download/") && method == "POST" {
        let app_id_str = path.trim_start_matches("/api/depot-post-download/");
        if let Ok(app_id) = app_id_str.parse::<u64>() {
            let parsed: Result<serde_json::Value, _> = serde_json::from_str(body);
            match parsed {
                Ok(val) => {
                    let job_id = val.get("jobId").and_then(|v| v.as_str()).unwrap_or("");
                    let game_name = val.get("gameName").and_then(|v| v.as_str()).unwrap_or("Unknown Game");
                    match depot_downloader::post_download(job_id, app_id, game_name) {
                        Ok(result) => Some((200, result.to_string())),
                        Err(e) => Some((200, json!({"ok": false, "message": e}).to_string())),
                    }
                }
                Err(e) => Some((400, json!({"ok": false, "message": format!("Invalid JSON: {e}")}).to_string())),
            }
        } else {
            Some((400, json!({"ok": false, "message": "Invalid appId"}).to_string()))
        }
    } else if path == "/api/downloads-queue" && method == "GET" {
        let qf = depot_downloader::get_queue();
        Some((200, json!({"ok": true, "queue": qf.queue, "history": qf.history}).to_string()))
    } else if path == "/api/downloads-queue/add" && method == "POST" {
        let parsed: Result<crate::depot_downloader::QueueItem, _> = serde_json::from_str(body);
        match parsed {
            Ok(item) => {
                let id = depot_downloader::add_to_queue(item);
                Some((200, json!({"ok": true, "id": id}).to_string()))
            }
            Err(e) => Some((400, json!({"ok": false, "message": format!("Invalid: {e}")}).to_string()))
        }
    } else if path.starts_with("/api/downloads-queue/remove/") && method == "POST" {
        let id = path.trim_start_matches("/api/downloads-queue/remove/");
        let ok = depot_downloader::remove_from_queue(id);
        Some((200, json!({"ok": ok}).to_string()))
    } else if path.starts_with("/api/downloads-queue/pause/") && method == "POST" {
        let id = path.trim_start_matches("/api/downloads-queue/pause/");
        match depot_downloader::pause_queue_item(id) {
            Ok(ok) => Some((200, json!({"ok": ok}).to_string())),
            Err(e) => Some((200, json!({"ok": false, "message": e}).to_string())),
        }
    } else if path.starts_with("/api/downloads-queue/resume/") && method == "POST" {
        let id = path.trim_start_matches("/api/downloads-queue/resume/");
        match depot_downloader::resume_queue_item(id) {
            Ok(ok) => Some((200, json!({"ok": ok}).to_string())),
            Err(e) => Some((200, json!({"ok": false, "message": e}).to_string())),
        }
    } else if path == "/api/downloads-queue/start-next" && method == "POST" {
        depot_downloader::start_next();
        Some((200, json!({"ok": true}).to_string()))
    } else if path == "/api/downloads-queue/clear-history" && method == "POST" {
        depot_downloader::clear_history();
        Some((200, json!({"ok": true}).to_string()))
    } else if path.starts_with("/api/downloads-queue/remove-history/") && method == "POST" {
        let id = path.trim_start_matches("/api/downloads-queue/remove-history/");
        let ok = depot_downloader::remove_history_item(id);
        Some((200, json!({"ok": ok}).to_string()))
    } else {
        None
    }
}

#[cfg(target_os = "linux")]
fn handle_steam_library_folders(method: &str, path: &str) -> Option<(u16, String)> {
    if path != "/api/steam-library-folders" || method != "GET" {
        return None;
    }

    let mut folders: Vec<serde_json::Value> = Vec::new();

    if let Some(steam_root) = crate::depot_downloader::steam_root() {
        let common = steam_root.join("steamapps").join("common");
        if common.exists() {
            folders.push(json!({
                "path": steam_root.to_string_lossy(),
                "commonPath": common.to_string_lossy(),
                "label": "Default"
            }));
        }

        let vdf_path = steam_root.join("steamapps").join("libraryfolders.vdf");
        if let Ok(content) = std::fs::read_to_string(&vdf_path) {
            let mut current_path: Option<String> = None;
            for line in content.lines() {
                let trimmed = line.trim();
                if trimmed.starts_with("\"path\"") {
                    // Parse: "path"		"/path/to/lib"
                    let rest = &trimmed[6..];
                    if let Some(q1) = rest.find('"') {
                        let after_q1 = &rest[q1 + 1..];
                        if let Some(q2) = after_q1.find('"') {
                            let value = &after_q1[..q2];
                            current_path = Some(value.replace("\\\\", "/").replace("\\", "/"));
                        }
                    }
                }
                if trimmed == "}" {
                    if let Some(ref p) = current_path {
                        let path_buf = std::path::PathBuf::from(p);
                        let common = path_buf.join("steamapps").join("common");
                        if common.exists() && !folders.iter().any(|f| f["path"].as_str() == Some(p)) {
                            folders.push(json!({
                                "path": p,
                                "commonPath": common.to_string_lossy(),
                                "label": path_buf.file_name().unwrap_or_default().to_string_lossy()
                            }));
                        }
                        current_path = None;
                    }
                }
            }
        }
    }

    Some((200, json!({"ok": true, "folders": folders}).to_string()))
}

// ---------------------------------------------------------------------------
// SLS Steam routes (Linux only)
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
fn handle_slssteam_route(method: &str, path: &str, body: &str) -> Option<(u16, String)> {
    use crate::slssteam;

    if path == "/api/slssteam/status" && method == "GET" {
        Some((200, slssteam::get_status().to_string()))
    } else if path == "/api/slssteam/kill-steam" && method == "POST" {
        match slssteam::kill_steam() {
            Ok(ok) => Some((200, json!({"ok": ok}).to_string())),
            Err(e) => Some((200, json!({"ok": false, "message": e}).to_string())),
        }
    } else if path == "/api/slssteam/start-steam" && method == "POST" {
        let parsed: Result<serde_json::Value, _> = serde_json::from_str(body);
        let with_ld_audit = parsed.as_ref().ok()
            .and_then(|v| v.get("withLdAudit"))
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        match slssteam::start_steam(with_ld_audit) {
            Ok(result) => Some((200, result.to_string())),
            Err(e) => Some((200, json!({"ok": false, "message": e}).to_string())),
        }
    } else if path == "/api/slssteam/setup" && method == "POST" {
        match slssteam::full_setup() {
            Ok(result) => Some((200, result.to_string())),
            Err(e) => Some((200, json!({"ok": false, "message": e}).to_string())),
        }
    } else if path == "/api/slssteam/patch-steam-sh" && method == "POST" {
        match slssteam::patch_steam_sh() {
            Ok(ok) => Some((200, json!({"ok": ok}).to_string())),
            Err(e) => Some((200, json!({"ok": false, "message": e}).to_string())),
        }
    } else if path == "/api/slssteam/config/add-app" && method == "POST" {
        let parsed: Result<serde_json::Value, _> = serde_json::from_str(body);
        match parsed {
            Ok(val) => {
                let app_id = val.get("appId").and_then(|v| v.as_str()).unwrap_or("");
                let comment = val.get("comment").and_then(|v| v.as_str()).unwrap_or("");
                match slssteam::config_add_app(app_id, comment) {
                    Ok(existed) => Some((200, json!({"ok": true, "added": !existed}).to_string())),
                    Err(e) => Some((200, json!({"ok": false, "message": e}).to_string())),
                }
            }
            Err(e) => Some((400, json!({"ok": false, "message": format!("Invalid JSON: {e}")}).to_string())),
        }
    } else if path == "/api/slssteam/config/remove-app" && method == "POST" {
        let parsed: Result<serde_json::Value, _> = serde_json::from_str(body);
        match parsed {
            Ok(val) => {
                let app_id = val.get("appId").and_then(|v| v.as_str()).unwrap_or("");
                match slssteam::config_remove_app(app_id) {
                    Ok(removed) => Some((200, json!({"ok": true, "removed": removed}).to_string())),
                    Err(e) => Some((200, json!({"ok": false, "message": e}).to_string())),
                }
            }
            Err(e) => Some((400, json!({"ok": false, "message": format!("Invalid JSON: {e}")}).to_string())),
        }
    } else if path == "/api/slssteam/config/apps" && method == "GET" {
        match slssteam::config_get_apps() {
            Ok(apps) => Some((200, json!({"ok": true, "apps": apps}).to_string())),
            Err(e) => Some((200, json!({"ok": false, "message": e}).to_string())),
        }
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Lua file management routes (cross-platform)
// ---------------------------------------------------------------------------

fn handle_lua_files_route(method: &str, path: &str, _body: &str) -> Option<(u16, String)> {
    if path == "/api/lua-files" && method == "GET" {
        let steam_root = crate::depot_downloader::steam_root();
        let lua_dir = steam_root.map(|r| r.join("config").join("lua"));

        let lua_dir = match lua_dir {
            Some(d) if d.exists() => d,
            _ => return Some((200, json!({"ok": true, "files": []}).to_string())),
        };

        let mut files: Vec<serde_json::Value> = Vec::new();

        if let Ok(entries) = std::fs::read_dir(&lua_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                if !name_str.ends_with(".lua") || name_str.contains(".backup") {
                    continue;
                }
                let app_id = name_str.trim_end_matches(".lua").to_string();

                // Extract game name from comment on line 2
                let mut game_name = String::new();
                if let Ok(content) = std::fs::read_to_string(entry.path()) {
                    for line in content.lines().take(3) {
                        let trimmed = line.trim();
                        if trimmed.starts_with("--") && trimmed.len() > 3 {
                            let candidate = trimmed[2..].trim();
                            if !candidate.is_empty()
                                && !candidate.contains("Lua")
                                && !candidate.contains("LumaForge")
                                && !candidate.contains("Manifest")
                                && !candidate.contains("Created")
                                && !candidate.contains("Website")
                                && !candidate.contains("Total")
                                && !candidate.contains("MAIN APPLICATION")
                            {
                                game_name = candidate.to_string();
                                break;
                            }
                        }
                    }
                }

                let meta = std::fs::metadata(entry.path()).ok();
                let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
                let modified = meta
                    .as_ref()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);

                files.push(json!({
                    "appId": app_id,
                    "filename": name_str,
                    "name": game_name,
                    "size": size,
                    "modified": modified
                }));
            }
        }

        // Sort by app id
        files.sort_by(|a, b| {
            let a_id = a["appId"].as_str().unwrap_or("");
            let b_id = b["appId"].as_str().unwrap_or("");
            a_id.cmp(b_id)
        });

        Some((200, json!({"ok": true, "files": files}).to_string()))
    } else if path.starts_with("/api/lua-files/") && method == "DELETE" {
        let app_id = path.trim_start_matches("/api/lua-files/");

        let steam_root = match crate::depot_downloader::steam_root() {
            Some(r) => r,
            None => return Some((200, json!({"ok": false, "message": "Steam root not found"}).to_string())),
        };

        let lua_path = steam_root.join("config").join("lua").join(format!("{}.lua", app_id));

        let mut removed_file = false;
        if lua_path.exists() {
            match std::fs::remove_file(&lua_path) {
                Ok(_) => {
                    removed_file = true;
                    crate::log_to_temp(&format!("[lua-files] Deleted {}", lua_path.display()));
                }
                Err(e) => {
                    return Some((200, json!({"ok": false, "message": format!("Failed to delete: {e}")}).to_string()));
                }
            }
        }

        // Also remove from SLS Steam config (Linux only — slssteam is Linux-only)
        #[cfg(target_os = "linux")]
        let mut removed_config = false;
        #[cfg(not(target_os = "linux"))]
        let removed_config = false;
        #[cfg(target_os = "linux")]
        match crate::slssteam::config_remove_app(app_id) {
            Ok(removed) => {
                removed_config = removed;
                if removed {
                    crate::log_to_temp(&format!("[lua-files] Removed {} from SLS config", app_id));
                }
            }
            Err(e) => {
                crate::log_to_temp(&format!("[lua-files] Warning: failed to remove from SLS config: {e}"));
            }
        }

        Some((200, json!({"ok": true, "removedFile": removed_file, "removedConfig": removed_config}).to_string()))
    } else {
        None
    }
}

fn send_response(stream: &mut std::net::TcpStream, status: u16, body: &str, content_type: &str) {
    let status_text = match status {
        200 => "OK",
        204 => "No Content",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "Unknown",
    };

    let response = format!(
        "HTTP/1.1 {} {}\r\n\
         {}\
         Content-Type: {}\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n\
         {}",
        status,
        status_text,
        CORS_HEADERS,
        if content_type.is_empty() {
            "text/plain"
        } else {
            content_type
        },
        body.len(),
        body
    );

    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}
