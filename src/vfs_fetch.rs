//! Linux VFS fetch pump — serves `https://lumaforge.local/...` theme and
//! plugin URLs from disk via CDP Fetch interception.
//!
//! Windows gets this in-process from cef_hook (browser-level Fetch.enable +
//! fulfillRequest from process birth). A CEF quirk (documented in cef_hook
//! src/lib.rs ~3720): **browser-level Fetch.enable only pauses requests from
//! store-origin documents** — steamloopback docs (Library, SharedJSContext,
//! popups) never pause at browser scope. Interception is therefore hybrid,
//! mirroring Windows:
//!
//! - ONE browser-scope root pump covers store documents from birth (no
//!   per-target attach race on the store).
//! - One per-target pump thread (own WebSocket + session-scoped Fetch.enable)
//!   covers every non-store page target. Started BEFORE the injector places
//!   `<link>`/`<script>` tags, so the fetches they trigger are already
//!   pausable — no DNS fallback.
//!
//! The main injection client is untouched (its read_response discards
//! events, so Fetch must never be enabled on that session).

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Mutex;
use std::thread::JoinHandle;

const VFS_HOST: &str = "lumaforge.local";

static ROOT_PUMP: Mutex<Option<JoinHandle<()>>> = Mutex::new(None);
static PUMPS: Mutex<Option<HashMap<String, JoinHandle<()>>>> = Mutex::new(None);
static LOGGED_READ_ERRS: Mutex<Option<HashSet<String>>> = Mutex::new(None);

/// Reconcile the per-target pump set with the non-store page set: reap
/// finished pumps (target died / Steam closed the WS) and start one per
/// target that doesn't have a live pump yet. Call BEFORE injecting a target
/// so its fetches are pausable from the first tag. Store targets are
/// excluded — the root pump covers them at browser scope.
pub fn sync(target_ids: &std::collections::HashSet<String>, port: u16) {
    let mut guard = match PUMPS.lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    let pumps = guard.get_or_insert_with(HashMap::new);
    pumps.retain(|_, h| !h.is_finished());
    for id in target_ids {
        if id.is_empty() || pumps.contains_key(id) {
            continue;
        }
        match start_pump(port, id.clone()) {
            Ok(handle) => {
                pumps.insert(id.clone(), handle);
            }
            Err(e) => {
                crate::log_to_temp(&format!(
                    "[vfs_fetch] pump start failed target#{}: {}",
                    &id[..id.len().min(12)],
                    e
                ));
            }
        }
    }
}

/// Make sure the browser-scope root pump is running for `port`. Reaps a
/// finished thread (its WS died with the Steam session) and starts a new one
/// if needed. Cheap when the pump is already live: one mutex + `is_finished`.
pub fn ensure_root_pump(port: u16) {
    let mut guard = match ROOT_PUMP.lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    let needs_start = match guard.as_ref() {
        Some(h) => h.is_finished(),
        None => true,
    };
    if !needs_start {
        return;
    }
    *guard = None;
    match start_root_pump(port) {
        Ok(handle) => {
            *guard = Some(handle);
        }
        Err(e) => {
            crate::log_to_temp(&format!("[vfs_fetch] root pump start failed: {}", e));
        }
    }
}

/// Forget every pump handle (Steam session ended / port switch). Threads
/// exit on their own when their WS dies; the next sync/ensure starts fresh.
pub fn stop_all() {
    if let Ok(mut guard) = ROOT_PUMP.lock() {
        guard.take();
    }
    if let Ok(mut guard) = PUMPS.lock() {
        guard.take();
    }
}

fn start_pump(port: u16, target_id: String) -> Result<JoinHandle<()>, String> {
    let mut client = crate::cdp::CdpClient::connect_to(port, &target_id)?;
    client.send_cdp_wait(
        &json!({
            "id": 1,
            "method": "Fetch.enable",
            "params": {
                "patterns": [
                    {"urlPattern": format!("*{}*", VFS_HOST), "requestStage": "Request"}
                ]
            }
        }),
        1,
    )?;
    crate::log_to_temp(&format!(
        "[vfs_fetch] pump attached target#{}",
        &target_id[..target_id.len().min(12)]
    ));
    Ok(std::thread::spawn(move || {
        pump_loop(client, Some(target_id))
    }))
}

fn start_root_pump(port: u16) -> Result<JoinHandle<()>, String> {
    let mut client = crate::cdp::CdpClient::connect_browser(port)?;
    client.send_cdp_wait(
        &json!({
            "id": 1,
            "method": "Fetch.enable",
            "params": {
                "patterns": [
                    {"urlPattern": format!("*{}*", VFS_HOST), "requestStage": "Request"}
                ]
            }
        }),
        1,
    )?;
    crate::log_to_temp("[vfs_fetch] root pump Fetch.enable ok (browser scope)");
    Ok(std::thread::spawn(move || pump_loop(client, None)))
}

fn pump_loop(mut client: crate::cdp::CdpClient, target_id: Option<String>) {
    let label = match &target_id {
        Some(id) => format!("target#{}", &id[..id.len().min(12)]),
        None => "root".to_string(),
    };
    loop {
        let event = match client.read_next_event() {
            Ok(e) => e,
            Err(e) => {
                crate::log_to_temp(&format!("[vfs_fetch] pump {} closed: {}", label, e));
                return;
            }
        };
        if event.get("method").and_then(|m| m.as_str()) != Some("Fetch.requestPaused") {
            continue;
        }
        let Some(params) = event.get("params") else {
            continue;
        };
        let Some(request_id) = params.get("requestId").and_then(|v| v.as_str()) else {
            continue;
        };
        let url = params
            .get("request")
            .and_then(|r| r.get("url"))
            .and_then(|u| u.as_str())
            .unwrap_or("");

        if !url.contains(VFS_HOST) {
            // Pattern filter should prevent this; never stall a request.
            let _ = client.send_cdp(&json!({
                "id": 0,
                "method": "Fetch.continueRequest",
                "params": {"requestId": request_id}
            }));
            continue;
        }

        match serve_vfs(url) {
            Ok(body) => {
                let mime = guess_mime_type(url);
                let body_b64 = STANDARD.encode(&body);
                let len = body.len();
                if let Err(e) = client.send_cdp(&json!({
                    "id": 0,
                    "method": "Fetch.fulfillRequest",
                    "params": {
                        "requestId": request_id,
                        "responseCode": 200,
                        "responseHeaders": [
                            {"name": "Content-Type", "value": mime},
                            {"name": "Access-Control-Allow-Origin", "value": "*"},
                            {"name": "Cache-Control", "value": "no-cache"},
                            {"name": "Content-Length", "value": len.to_string()}
                        ],
                        "body": body_b64
                    }
                })) {
                    crate::log_to_temp(&format!("[vfs_fetch] fulfill send error: {}", e));
                    return;
                }
            }
            Err(()) => {
                let _ = client.send_cdp(&json!({
                    "id": 0,
                    "method": "Fetch.failRequest",
                    "params": {"requestId": request_id, "errorReason": "NameNotResolved"}
                }));
            }
        }
    }
}

/// Resolve a lumaforge.local URL to disk bytes (mirror of cef_hook's
/// handle_vfs_request, with an added path-traversal guard on the theme
/// branch). Missing files degrade to an empty 200 like on Windows.
fn serve_vfs(url: &str) -> Result<Vec<u8>, ()> {
    let plugins_prefix = format!("https://{}/plugins/", VFS_HOST);
    if let Some(rest) = url.strip_prefix(&plugins_prefix) {
        let cut = rest.find(|c| c == '?' || c == '#').unwrap_or(rest.len());
        let decoded = percent_decode(&rest[..cut]);
        if decoded.contains("..") {
            return Err(());
        }
        let file_path = crate::platform::plugins_dir().join(&decoded);
        return match std::fs::read(&file_path) {
            Ok(bytes) => Ok(bytes),
            Err(e) => {
                log_read_error_once(&file_path.display().to_string(), &e);
                Ok(Vec::new())
            }
        };
    }

    let prefix = format!("https://{}/themes/", VFS_HOST);
    let fallback_prefix = format!("https://{}/", VFS_HOST);
    let relative = if let Some(rest) = url.strip_prefix(&prefix) {
        rest.to_string()
    } else if let Some(rest) = url.strip_prefix(&fallback_prefix) {
        rest.to_string()
    } else {
        return Err(());
    };
    let cut = relative
        .find(|c| c == '?' || c == '#')
        .unwrap_or(relative.len());
    let decoded = percent_decode(&relative[..cut]);
    if decoded.contains("..") {
        return Err(());
    }

    let theme_dir = current_theme_dir()?;
    let file_path = theme_dir.join(&decoded);
    crate::log_to_temp(&format!("[vfs_fetch] {} -> {}", url, file_path.display()));
    match std::fs::read(&file_path) {
        Ok(bytes) => Ok(bytes),
        Err(e) => {
            log_read_error_once(&file_path.display().to_string(), &e);
            Ok(Vec::new())
        }
    }
}

/// Log a read error once per path — a patch referencing a missing file
/// (e.g. screenshotManager.css at the wrong depth) would otherwise re-log
/// on every document load.
fn log_read_error_once(path: &str, e: &std::io::Error) {
    if let Ok(mut guard) = LOGGED_READ_ERRS.lock() {
        let set = guard.get_or_insert_with(HashSet::new);
        if !set.insert(path.to_string()) {
            return;
        }
    }
    crate::log_to_temp(&format!(
        "[vfs_fetch] read error: {} ({}) [further logs for this path suppressed]",
        path, e
    ));
}

/// Theme dir from the manifest the theme exporter writes — re-read per
/// request so a theme switch is picked up without restarting the pumps.
fn current_theme_dir() -> Result<PathBuf, ()> {
    let manifest_path = crate::platform::runtime_dir().join("theme-manifest.json");
    let content = std::fs::read_to_string(manifest_path).map_err(|_| ())?;
    let v: serde_json::Value = serde_json::from_str(&content).map_err(|_| ())?;
    let dir = v.get("dir").and_then(|d| d.as_str()).ok_or(())?;
    if dir.is_empty() {
        return Err(());
    }
    Ok(PathBuf::from(dir))
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
            if let Ok(b) = u8::from_str_radix(hex, 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_default()
}

fn guess_mime_type(path: &str) -> &'static str {
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".css") {
        "text/css"
    } else if lower.ends_with(".js") {
        "application/javascript"
    } else if lower.ends_with(".json") {
        "application/json"
    } else if lower.ends_with(".svg") {
        "image/svg+xml"
    } else if lower.ends_with(".png") {
        "image/png"
    } else if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        "image/jpeg"
    } else if lower.ends_with(".gif") {
        "image/gif"
    } else if lower.ends_with(".ico") {
        "image/x-icon"
    } else if lower.ends_with(".woff") {
        "font/woff"
    } else if lower.ends_with(".woff2") {
        "font/woff2"
    } else if lower.ends_with(".ttf") {
        "font/ttf"
    } else if lower.ends_with(".html") || lower.ends_with(".htm") {
        "text/html"
    } else {
        "application/octet-stream"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_decode_basic() {
        assert_eq!(percent_decode("a%20b.css"), "a b.css");
        assert_eq!(percent_decode("plain.js"), "plain.js");
        assert_eq!(percent_decode("bad%zz"), "bad%zz");
    }

    #[test]
    fn mime_mapping() {
        assert_eq!(guess_mime_type("x/style.css"), "text/css");
        assert_eq!(guess_mime_type("x/app.JS"), "application/javascript");
        assert_eq!(guess_mime_type("fonts/f.woff2"), "font/woff2");
        assert_eq!(guess_mime_type("img/i.svg"), "image/svg+xml");
        assert_eq!(guess_mime_type("bin/x.bin"), "application/octet-stream");
    }

    #[test]
    fn serve_vfs_rejects_traversal() {
        assert!(serve_vfs("https://lumaforge.local/themes/../../etc/passwd").is_err());
        assert!(serve_vfs("https://lumaforge.local/plugins/../../secret").is_err());
    }

    #[test]
    fn serve_vfs_rejects_non_host() {
        assert!(serve_vfs("https://evil.example/themes/x.css").is_err());
    }
}
