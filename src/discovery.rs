use std::fs;
use std::net::TcpListener;
use std::path::PathBuf;

const DEFAULT_DEBUG_PORT: u16 = 9222;
const DYNAMIC_PORT_MIN: u16 = 10_000;
const DYNAMIC_PORT_MAX: u16 = 60_000;

pub fn resolve_debug_port() -> u16 {
    if let Ok(val) = std::env::var("STEAMCDP_PORT") {
        if let Ok(port) = val.parse::<u16>() {
            if port >= DYNAMIC_PORT_MIN && port <= DYNAMIC_PORT_MAX {
                crate::log_to_temp(&format!("[steamcdp] Using STEAMCDP_PORT env var: {}", port));
                return port;
            }
        }
    }

    if let Ok(listener) = TcpListener::bind("127.0.0.1:0") {
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        crate::log_to_temp(&format!("[steamcdp] OS assigned free port: {}", port));
        return port;
    }

    crate::log_to_temp(&format!(
        "[steamcdp] Falling back to default port: {}",
        DEFAULT_DEBUG_PORT
    ));
    DEFAULT_DEBUG_PORT
}

fn discovery_path() -> Option<PathBuf> {
    let local_app_data = std::env::var("LOCALAPPDATA").ok()?;
    Some(
        PathBuf::from(local_app_data)
            .join("LumaForge")
            .join("runtime")
            .join("steam-cdp.json"),
    )
}

pub fn publish_port(port: u16) {
    let path = match discovery_path() {
        Some(p) => p,
        None => {
            crate::log_to_temp("[steamcdp] Could not determine LOCALAPPDATA, skipping publish");
            return;
        }
    };

    if let Some(parent) = path.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            crate::log_to_temp(&format!(
                "[steamcdp] Failed to create directory {:?}: {}",
                parent, e
            ));
            return;
        }
    }

    let tmp_path = path.with_extension("json.tmp");
    let json = format!(r#"{{"port":{}}}"#, port);

    if let Err(e) = fs::write(&tmp_path, &json) {
        crate::log_to_temp(&format!("[steamcdp] Failed to write {:?}: {}", tmp_path, e));
        return;
    }

    let _ = fs::remove_file(&path);

    if let Err(e) = fs::rename(&tmp_path, &path) {
        crate::log_to_temp(&format!(
            "[steamcdp] Failed to rename {:?} -> {:?}: {}",
            tmp_path, path, e
        ));
        return;
    }

    crate::log_to_temp(&format!("[steamcdp] Published port {} to {:?}", port, path));
}
