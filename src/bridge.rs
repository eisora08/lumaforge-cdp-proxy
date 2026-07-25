use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use serde_json::json;

const BRIDGE_PORT: u16 = 21775;
const CORS_HEADERS: &str = "\
Access-Control-Allow-Origin: *\r\n\
Access-Control-Allow-Methods: GET, POST, OPTIONS\r\n\
Access-Control-Allow-Headers: Content-Type\r\n\
Access-Control-Max-Age: 86400\r\n";

pub fn start_bridge_server() {
    std::thread::spawn(|| {
        let listener = match TcpListener::bind(format!("127.0.0.1:{}", BRIDGE_PORT)) {
            Ok(l) => l,
            Err(e) => {
                crate::log_to_temp(&format!("[bridge] Failed to bind port {}: {}", BRIDGE_PORT, e));
                return;
            }
        };
        crate::log_to_temp(&format!("[bridge] Mini-bridge listening on port {}", BRIDGE_PORT));

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
    stream.set_read_timeout(Some(std::time::Duration::from_secs(5))).ok();
    stream.set_write_timeout(Some(std::time::Duration::from_secs(5))).ok();

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

    crate::log_to_temp(&format!("[bridge] {} {}", method, &path[..path.len().min(120)]));

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
    if let Some(rust_resp) = crate::package_installer::try_handle_route(method, &clean_path, body) {
        return rust_resp;
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
        "message": "Connect LumaLite for download sources"
    });
    (200, response.to_string())
}

fn handle_providers() -> (u16, String) {
    let response = json!({
        "ok": true,
        "providers": [],
        "message": "No providers connected. Start LumaLite to enable download sources."
    });
    (200, response.to_string())
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
        if content_type.is_empty() { "text/plain" } else { content_type },
        body.len(),
        body
    );

    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}
