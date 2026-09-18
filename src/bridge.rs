use serde_json::json;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;

const BRIDGE_PORT: u16 = 21775;
const BRIDGE_PORT_FALLBACK: u16 = 21776;
const CORS_HEADERS: &str = "\
Access-Control-Allow-Origin: *\r\n\
Access-Control-Allow-Methods: GET, POST, OPTIONS\r\n\
Access-Control-Allow-Headers: Content-Type\r\n\
Access-Control-Max-Age: 86400\r\n";

pub fn start_bridge_server() {
    std::thread::spawn(|| {
        // Try primary port first (luma-lite's port), fall back to 21776
        let listener = match TcpListener::bind(format!("127.0.0.1:{}", BRIDGE_PORT)) {
            Ok(l) => {
                crate::log_to_temp(&format!(
                    "[bridge] Mini-bridge listening on port {} (luma-lite not detected)",
                    BRIDGE_PORT
                ));
                l
            }
            Err(e) => {
                crate::log_to_temp(&format!(
                    "[bridge] Port {} in use (luma-lite running?), trying fallback port {}: {}",
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

fn route_request(method: &str, path: &str, body: &str) -> (u16, String) {
    let query = path.splitn(2, '?').nth(1).unwrap_or("").to_string();
    let clean_path = path.splitn(2, '?').next().unwrap_or(path).to_string();

    let headers_map: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let lua_req = crate::lua_backend::LuaRequest {
        method: method.to_string(),
        path: clean_path.clone(),
        body: body.to_string(),
        headers: headers_map,
        query,
    };

    // Rust-native routes: intercept binary-heavy operations before Lua
    #[cfg(target_os = "windows")]
    if let Some(rust_resp) = crate::package_installer::try_handle_route(method, &clean_path, body) {
        return rust_resp;
    }

    // Depot download routes (Linux only)
    #[cfg(target_os = "linux")]
    {
        if let Some(resp) = handle_depot_route(method, &clean_path, body) {
            return resp;
        }
        if let Some(resp) = handle_slssteam_route(method, &clean_path, body) {
            return resp;
        }
    }

    if let Some(lua_resp) = crate::lua_backend::handle_lua_request(&lua_req) {
        return (lua_resp.status, lua_resp.body);
    }

    if clean_path.starts_with("/api/local-status/") {
        let app_id = clean_path.trim_start_matches("/api/local-status/");
        handle_local_status(app_id)
    } else if clean_path.starts_with("/api/sources/") {
        let app_id = clean_path.trim_start_matches("/api/sources/");
        handle_sources(app_id)
    } else if clean_path == "/api/providers" {
        handle_providers()
    } else if clean_path.starts_with("/api/open-library/") && method == "POST" {
        let _app_id = clean_path.trim_start_matches("/api/open-library/");
        (200, json!({"ok": true}).to_string())
    } else {
        (404, json!({"error": "not found"}).to_string())
    }
}

fn handle_local_status(app_id: &str) -> (u16, String) {
    let response = json!({
        "ok": true,
        "appId": app_id,
        "inLibrary": false
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
                Ok(depots) => Some((200, json!({"ok": true, "depots": depots}).to_string())),
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
    } else {
        None
    }
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
