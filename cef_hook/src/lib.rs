use std::ffi::c_void;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tungstenite::{connect, Message};
use serde_json::{json, Value};
use base64::{Engine as _, engine::general_purpose::STANDARD};

fn log_to_temp(msg: &str) {
    let Ok(local_appdata) = std::env::var("LOCALAPPDATA") else {
        return;
    };
    let log_path = PathBuf::from(local_appdata)
        .join("LumaForge")
        .join("runtime")
        .join("cef_hook.log");
    let _ = std::fs::create_dir_all(log_path.parent().unwrap());
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        let _ = writeln!(f, "{}", msg);
    }
}

fn resolve_debug_port() -> Option<u16> {
    if let Ok(port_str) = std::env::var("STEAMCDP_PORT") {
        if let Ok(port) = port_str.parse::<u16>() {
            log_to_temp(&format!("[cef_hook] Using port from env: {}", port));
            return Some(port);
        }
    }

    let cmd_line = std::env::args().collect::<Vec<String>>().join(" ");
    if let Some(pos) = cmd_line.to_lowercase().find("--remote-debugging-port=") {
        let start = pos + "--remote-debugging-port=".len();
        let remaining = &cmd_line[start..];
        let end = remaining.find(|c: char| !c.is_ascii_digit()).unwrap_or(remaining.len());
        if let Ok(port) = remaining[..end].parse::<u16>() {
            log_to_temp(&format!("[cef_hook] Using port from args: {}", port));
            return Some(port);
        }
    }

    if let Ok(local_appdata) = std::env::var("LOCALAPPDATA") {
        let discovery_path = PathBuf::from(local_appdata)
            .join("LumaForge")
            .join("runtime")
            .join("steam-cdp.json");
        if let Ok(content) = fs::read_to_string(&discovery_path) {
            if let Ok(json) = serde_json::from_str::<Value>(&content) {
                if let Some(port) = json.get("port").and_then(|p| p.as_u64()) {
                    if let Some(updated) = json.get("updatedAt").and_then(|u| u.as_u64()) {
                        let now = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap()
                            .as_secs();
                        if now.saturating_sub(updated) < 120 {
                            log_to_temp(&format!("[cef_hook] Using port from discovery: {}", port));
                            return Some(port as u16);
                        }
                    }
                }
            }
        }
    }

    None
}

struct ThemeState {
    css: Option<String>,
    js: Option<String>,
    css_mtime: Option<u64>,
    js_mtime: Option<u64>,
    last_signal_mtime: Option<u64>,
    plugins: Vec<LoadedPlugin>,
    plugins_mtime: Option<u64>,
}

#[derive(Clone)]
struct LoadedPlugin {
    name: String,
    code: String,
    target_url: Option<String>,
}

fn file_mtime_secs(path: &PathBuf) -> Option<u64> {
    fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
}

fn load_theme_files(state: &mut ThemeState) {
    let Ok(local_appdata) = std::env::var("LOCALAPPDATA") else {
        return;
    };
    let themes_dir = PathBuf::from(local_appdata).join("LumaForge").join("themes");
    let css_path = themes_dir.join("current.css");
    let js_path = themes_dir.join("current.js");

    let new_css_mtime = file_mtime_secs(&css_path);
    if new_css_mtime != state.css_mtime {
        state.css = fs::read_to_string(&css_path).ok().filter(|s| !s.is_empty());
        state.css_mtime = new_css_mtime;
        log_to_temp(&format!(
            "[cef_hook] Reloaded CSS: {}",
            state.css.as_ref().map_or(0, |s| s.len())
        ));
    }

    let new_js_mtime = file_mtime_secs(&js_path);
    if new_js_mtime != state.js_mtime {
        state.js = fs::read_to_string(&js_path).ok().filter(|s| !s.is_empty());
        state.js_mtime = new_js_mtime;
        log_to_temp(&format!(
            "[cef_hook] Reloaded JS: {}",
            state.js.as_ref().map_or(0, |s| s.len())
        ));
    }
}

fn load_plugins(state: &mut ThemeState) {
    let Ok(local_appdata) = std::env::var("LOCALAPPDATA") else {
        log_to_temp("[cef_hook] load_plugins: no LOCALAPPDATA");
        return;
    };
    let plugins_dir = PathBuf::from(local_appdata).join("LumaForge").join("plugins");

    let dir_mtime = file_mtime_secs(&plugins_dir);
    if state.plugins_mtime.is_some() && dir_mtime == state.plugins_mtime {
        return;
    }
    state.plugins_mtime = dir_mtime;

    state.plugins.clear();

    log_to_temp(&format!("[cef_hook] Loading plugins from {}", plugins_dir.display()));

    let Ok(entries) = fs::read_dir(&plugins_dir) else {
        log_to_temp("[cef_hook] load_plugins: read_dir failed");
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let manifest_path = path.join("manifest.json");
        let config_path = path.join("extension-config.json");

        let manifest_str = match fs::read_to_string(&manifest_path) {
            Ok(s) => s,
            Err(e) => {
                log_to_temp(&format!("[cef_hook] Failed to read {}: {}", manifest_path.display(), e));
                continue;
            }
        };
        let manifest: Value = match serde_json::from_str(&manifest_str) {
            Ok(v) => v,
            Err(e) => {
                log_to_temp(&format!("[cef_hook] Failed to parse {}: {}", manifest_path.display(), e));
                continue;
            }
        };

        let is_enabled = if config_path.exists() {
            fs::read_to_string(&config_path)
                .ok()
                .and_then(|s| serde_json::from_str::<Value>(&s).ok())
                .and_then(|v| v.get("enabled").and_then(|e| e.as_bool()).or_else(|| v.get("isEnabled").and_then(|e| e.as_bool())))
                .unwrap_or(true)
        } else {
            true
        };

        if !is_enabled {
            continue;
        }

        let plugin_id = manifest.get("id")
            .or_else(|| manifest.get("pluginId"))
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let activation = manifest.get("activation");

        let cef_config = activation;

        let inject_path = if let Some(cef) = cef_config {
            cef.get("injectScript")
                .or_else(|| cef.get("inject_script"))
                .and_then(|v| v.as_str())
                .map(|s| path.join(s))
        } else {
            log_to_temp(&format!("[cef_hook] Plugin {} has no activation block, skipping", plugin_id));
            continue;
        };

        let inject_path = match inject_path {
            Some(p) => p,
            None => continue,
        };

        let target_url = cef_config
            .and_then(|c| {
                c.get("targetUrl")
                    .or_else(|| c.get("target_url"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            });

        let code = match fs::read_to_string(&inject_path) {
            Ok(s) => s,
            Err(e) => {
                log_to_temp(&format!("[cef_hook] Failed to read plugin {}: {}", plugin_id, e));
                continue;
            }
        };

        log_to_temp(&format!("[cef_hook] Loaded plugin: {} (url={:?})", plugin_id, target_url));
        state.plugins.push(LoadedPlugin {
            name: plugin_id.to_string(),
            code,
            target_url,
        });
    }

    log_to_temp(&format!("[cef_hook] Total plugins loaded: {}", state.plugins.len()));
}

fn get_browser_ws_url(port: u16) -> Option<String> {
    use std::io::{BufRead, BufReader, Write};
    let mut stream = std::net::TcpStream::connect(format!("127.0.0.1:{}", port)).ok()?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(3))).ok()?;
    let request = format!("GET /json/version HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n", port);
    stream.write_all(request.as_bytes()).ok()?;

    let mut reader = BufReader::new(stream);
    let mut headers_done = false;
    let mut body = String::new();
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                if headers_done {
                    body.push_str(&line);
                } else if line.trim().is_empty() {
                    headers_done = true;
                }
            }
            Err(_) => break,
        }
    }

    let json: Value = serde_json::from_str(&body).ok()?;
    json.get("webSocketDebuggerUrl")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

fn check_theme_reload_signal(state: &mut ThemeState) -> bool {
    let Ok(local_appdata) = std::env::var("LOCALAPPDATA") else {
        return false;
    };
    let signal_path = PathBuf::from(local_appdata)
        .join("LumaForge")
        .join("runtime")
        .join("theme-reload");

    let signal_mtime = file_mtime_secs(&signal_path);
    if signal_mtime.is_none() || signal_mtime == state.last_signal_mtime {
        return false;
    }

    state.last_signal_mtime = signal_mtime;
    log_to_temp("[cef_hook] Theme reload signal detected, reloading theme files");
    load_theme_files(state);
    true
}

fn inject_theme_html(html: &str, css: Option<&str>, js: Option<&str>, plugins: &[LoadedPlugin], url: &str) -> String {
    let mut result = html.to_string();

    if let Some(css_content) = css {
        let tag = format!("<style data-lumaforge=\"theme\">{}\n</style>", css_content);
        if let Some(pos) = result.to_lowercase().rfind("</head>") {
            result.insert_str(pos, &tag);
        } else if let Some(pos) = result.to_lowercase().rfind("<body") {
            result.insert_str(pos, &tag);
        } else {
            result.push_str(&tag);
        }
    }

    let mut script_tags = String::new();

    if let Some(js_content) = js {
        script_tags.push_str(&format!(
            "<script data-lumaforge=\"theme\">\n{}\n</script>\n",
            js_content
        ));
    }

    for plugin in plugins {
        let matches = plugin.target_url.as_ref().map_or(true, |pattern| {
            url.contains(pattern.as_str())
        });
        if matches {
            script_tags.push_str(&format!(
                "<script data-lumaforge-plugin=\"{}\">\n{}\n</script>\n",
                plugin.name, plugin.code
            ));
        }
    }

    if !script_tags.is_empty() {
        if let Some(pos) = result.to_lowercase().rfind("</body>") {
            result.insert_str(pos, &script_tags);
        } else if let Some(pos) = result.to_lowercase().rfind("</html>") {
            result.insert_str(pos, &script_tags);
        } else {
            result.push_str(&script_tags);
        }
    }

    result
}

fn send_cdp(socket: &mut tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>, msg: &Value) -> bool {
    if let Err(e) = socket.send(Message::Text(msg.to_string())) {
        log_to_temp(&format!("[cef_hook] WebSocket send error: {}", e));
        return false;
    }
    true
}

fn recv_cdp_response(socket: &mut tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>, expected_id: u64) -> Option<Value> {
    let deadline = SystemTime::now() + std::time::Duration::from_secs(10);
    loop {
        if SystemTime::now() > deadline {
            log_to_temp(&format!("[cef_hook] Timeout waiting for response id={}", expected_id));
            return None;
        }
        match socket.read() {
            Ok(Message::Text(text)) => {
                if let Ok(msg) = serde_json::from_str::<Value>(&text) {
                    if msg.get("id").and_then(|i| i.as_u64()) == Some(expected_id) {
                        return Some(msg);
                    }
                }
            }
            Ok(_) => {}
            Err(tungstenite::Error::Io(ref e))
                if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                std::thread::sleep(std::time::Duration::from_millis(50));
                continue;
            }
            Err(e) => {
                log_to_temp(&format!("[cef_hook] WebSocket read error: {}", e));
                return None;
            }
        }
    }
}

fn handle_cdp_connection(port: u16) {
    loop {
        let browser_ws_url = match get_browser_ws_url(port) {
            Some(url) => url,
            None => {
                log_to_temp("[cef_hook] Could not get browser WebSocket URL, retrying in 3s");
                std::thread::sleep(std::time::Duration::from_secs(3));
                continue;
            }
        };
        log_to_temp(&format!("[cef_hook] Connecting to CDP: {}", browser_ws_url));

        let (mut socket, _) = match connect(&browser_ws_url) {
            Ok(conn) => conn,
            Err(e) => {
                log_to_temp(&format!("[cef_hook] Failed to connect: {}, retrying in 3s", e));
                std::thread::sleep(std::time::Duration::from_secs(3));
                continue;
            }
        };
        log_to_temp("[cef_hook] Connected to CDP browser endpoint");

        let mut msg_id = 1u64;

        let enable_fetch = json!({
            "id": msg_id,
            "method": "Fetch.enable",
            "params": {
                "patterns": [
                    {"urlPattern": "*/luma-bridge/*", "requestStage": "Request"},
                    {"urlPattern": "*", "requestStage": "Response"}
                ]
            }
        });
        if !send_cdp(&mut socket, &enable_fetch) {
            log_to_temp("[cef_hook] Failed to enable Fetch, reconnecting...");
            std::thread::sleep(std::time::Duration::from_secs(2));
            continue;
        }
        msg_id += 1;
        log_to_temp("[cef_hook] Fetch.enable sent");

        let enable_runtime = json!({
            "id": msg_id,
            "method": "Runtime.enable",
            "params": {}
        });
        if !send_cdp(&mut socket, &enable_runtime) {
            log_to_temp("[cef_hook] Failed to enable Runtime, reconnecting...");
            std::thread::sleep(std::time::Duration::from_secs(2));
            continue;
        }
        msg_id += 1;
        log_to_temp("[cef_hook] Runtime.enable sent");

        let enable_page = json!({
            "id": msg_id,
            "method": "Page.enable",
            "params": {}
        });
        if !send_cdp(&mut socket, &enable_page) {
            log_to_temp("[cef_hook] Failed to enable Page, reconnecting...");
            std::thread::sleep(std::time::Duration::from_secs(2));
            continue;
        }
        msg_id += 1;

        let bypass_csp = json!({
            "id": msg_id,
            "method": "Page.setBypassCSP",
            "params": {"enabled": true}
        });
        if !send_cdp(&mut socket, &bypass_csp) {
            log_to_temp("[cef_hook] Failed to setBypassCSP, reconnecting...");
            std::thread::sleep(std::time::Duration::from_secs(2));
            continue;
        }
        msg_id += 1;
        log_to_temp("[cef_hook] Page.enable + setBypassCSP sent");

        let mut theme_state = ThemeState {
            css: None,
            js: None,
            css_mtime: None,
            js_mtime: None,
            last_signal_mtime: None,
            plugins: Vec::new(),
            plugins_mtime: None,
        };
        load_theme_files(&mut theme_state);
        load_plugins(&mut theme_state);

        re_register_script(&mut socket, &mut msg_id, &theme_state);

        install_bridge_shim(&mut socket, &mut msg_id);

        let mut lost = false;
        let mut loop_iter = 0u64;
        while !lost {
            loop_iter += 1;
            if loop_iter % 5 == 0 {
                if check_theme_reload_signal(&mut theme_state) {
                    re_register_script(&mut socket, &mut msg_id, &theme_state);
                }
                load_plugins(&mut theme_state);
            }

            match socket.read() {
                Ok(Message::Text(text)) => {
                    if let Ok(msg) = serde_json::from_str::<Value>(&text) {
                        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");

                        match method {
                            "Fetch.requestPaused" => {
                                handle_fetch_paused(
                                    &mut socket,
                                    &msg,
                                    &mut msg_id,
                                    &mut theme_state,
                                );
                            }
                            "Runtime.bindingCalled" => {
                                let name = msg.get("params").and_then(|p| p.get("name")).and_then(|n| n.as_str()).unwrap_or("");
                                let payload = msg.get("params").and_then(|p| p.get("payload")).and_then(|p| p.as_str()).unwrap_or("");
                                if name == "__lumaNativeBridge" {
                                    handle_bridge_binding(&mut socket, &mut msg_id, payload);
                                }
                            }
                            "Runtime.consoleAPICalled" => {
                                let args = msg.get("params").and_then(|p| p.get("args")).and_then(|a| a.as_array());
                                if let Some(arr) = args {
                                    let parts: Vec<String> = arr.iter().filter_map(|a| a.get("value").and_then(|v| v.as_str()).map(|s| s.to_string())).collect();
                                    let text = parts.join(" ");
                                    if text.contains("LUMA") || text.contains("Bridge") || text.contains("luma") || text.contains("bridge") {
                                        log_to_temp(&format!("[cef_hook] JS console: {}", &text[..text.len().min(200)]));
                                    }
                                }
                            }
                            "Page.frameNavigated" => {
                                let frame = msg.get("params").and_then(|p| p.get("frame"));
                                let url = frame.and_then(|f| f.get("url")).and_then(|u| u.as_str()).unwrap_or("");
                                let is_main = frame.and_then(|f| f.get("parentId")).is_none();
                                log_to_temp(&format!(
                                    "[cef_hook] Navigation: {} (main={})",
                                    url, is_main
                                ));

                                if is_main {
                                    load_theme_files(&mut theme_state);
                                    load_plugins(&mut theme_state);
                                    re_register_script(&mut socket, &mut msg_id, &theme_state);
                                    install_bridge_shim(&mut socket, &mut msg_id);
                                }
                            }
                            _ => {}
                        }
                    }
                }
                Ok(Message::Close(_)) => {
                    log_to_temp("[cef_hook] WebSocket closed, reconnecting...");
                    lost = true;
                }
                Err(e) => {
                    log_to_temp(&format!("[cef_hook] WebSocket error: {}, reconnecting...", e));
                    lost = true;
                }
                _ => {}
            }
        }

        log_to_temp("[cef_hook] Reconnecting in 2s...");
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
}

fn handle_fetch_paused(
    socket: &mut tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>,
    msg: &Value,
    msg_id: &mut u64,
    theme_state: &mut ThemeState,
) {
    let params = match msg.get("params") {
        Some(p) => p,
        None => return,
    };
    let request_id_str = params.get("requestId").and_then(|r| r.as_str()).unwrap_or("");
    let url = params
        .get("request")
        .and_then(|r| r.get("url"))
        .and_then(|u| u.as_str())
        .unwrap_or("");
    let status = params
        .get("responseStatusCode")
        .and_then(|s| s.as_u64());
    let is_request_stage = status.is_none();
    let status_code = status.unwrap_or(200);
    let response_headers = params.get("responseHeaders").cloned();

    if url.contains("/luma-bridge/") && is_request_stage {
        let method = params.get("request")
            .and_then(|r| r.get("method"))
            .and_then(|m| m.as_str())
            .unwrap_or("GET");
        let post_data = params.get("request")
            .and_then(|r| r.get("postData"))
            .and_then(|p| p.as_str());
        let idx = url.find("/luma-bridge/").unwrap_or(0) + "/luma-bridge".len();
        let path = if idx < url.len() { &url[idx..] } else { "/" };

        log_to_temp(&format!("[cef_hook] Bridge proxy: {} {}", method, path));

        let body = match proxy_bridge_request(path, method, post_data) {
            Ok(b) => b,
            Err(e) => {
                log_to_temp(&format!("[cef_hook] Bridge proxy error: {}", e));
                let fail_msg = json!({
                    "id": *msg_id,
                    "method": "Fetch.failRequest",
                    "params": {"requestId": request_id_str, "errorReason": "Failed"}
                });
                *msg_id += 1;
                send_cdp(socket, &fail_msg);
                return;
            }
        };

        let body_b64 = STANDARD.encode(body.as_bytes());
        let content_length = body.len();
        let resp_headers = json!([
            {"name": "Content-Type", "value": "application/json"},
            {"name": "Access-Control-Allow-Origin", "value": "*"},
            {"name": "Content-Length", "value": content_length.to_string()}
        ]);
        let fulfill = json!({
            "id": *msg_id,
            "method": "Fetch.fulfillRequest",
            "params": {
                "requestId": request_id_str,
                "responseCode": 200,
                "responseHeaders": resp_headers,
                "body": body_b64
            }
        });
        *msg_id += 1;
        send_cdp(socket, &fulfill);
        return;
    }

    let lower_url = url.to_lowercase();

    if is_request_stage {
        let continue_msg = json!({
            "id": *msg_id,
            "method": "Fetch.continueRequest",
            "params": {"requestId": request_id_str}
        });
        *msg_id += 1;
        send_cdp(socket, &continue_msg);
        return;
    }

    let is_html_url = lower_url.ends_with(".html")
        || lower_url.ends_with(".htm");

    let mut content_type_str = String::new();
    let is_html_content_type = response_headers.as_ref().and_then(|h| {
        if let Value::Array(arr) = h {
            for item in arr {
                if let Value::Object(map) = item {
                    let name = map.get("name").and_then(|v| v.as_str()).unwrap_or("").to_lowercase();
                    let value = map.get("value").and_then(|v| v.as_str()).unwrap_or("").to_lowercase();
                    if name == "content-type" {
                        content_type_str = value.clone();
                        if value.contains("text/html") {
                            return Some(true);
                        }
                    }
                }
            }
        }
        None
    }).unwrap_or(false);

    let is_html = is_html_content_type || is_html_url;

    if !is_html {
        log_to_temp(&format!("[cef_hook] Skipping (ct={}): {}", &content_type_str[..content_type_str.len().min(40)], &url[..url.len().min(100)]));
    }

    if !is_html {
        let continue_msg = json!({
            "id": *msg_id,
            "method": "Fetch.continueResponse",
            "params": {"requestId": request_id_str}
        });
        *msg_id += 1;
        send_cdp(socket, &continue_msg);
        return;
    }

    log_to_temp(&format!("[cef_hook] Intercepting HTML: {}", &url[..url.len().min(120)]));

    let get_body = json!({
        "id": *msg_id,
        "method": "Fetch.getResponseBody",
        "params": {"requestId": request_id_str}
    });
    let current_id = *msg_id;
    *msg_id += 1;

    if !send_cdp(socket, &get_body) {
        return;
    }

    let body_response = recv_cdp_response(socket, current_id);
    let body_msg = match body_response {
        Some(m) => m,
        None => return,
    };

    let result = match body_msg.get("result") {
        Some(r) => r,
        None => return,
    };
    let body = result.get("body").and_then(|b| b.as_str()).unwrap_or("");
    let is_base64 = result
        .get("base64Encoded")
        .and_then(|b| b.as_bool())
        .unwrap_or(false);

    let decoded_body = if is_base64 {
        STANDARD.decode(body).unwrap_or_default()
    } else {
        body.as_bytes().to_vec()
    };

    let body_str = String::from_utf8_lossy(&decoded_body).to_string();

    load_theme_files(theme_state);
    let modified = inject_theme_html(&body_str, theme_state.css.as_deref(), theme_state.js.as_deref(), &theme_state.plugins, url);

    let encoded = STANDARD.encode(modified.as_bytes());
    let mut fulfill_params = json!({
        "requestId": request_id_str,
        "responseCode": status_code,
        "body": encoded
    });

    if let Some(headers) = response_headers {
        if let Some(obj) = fulfill_params.as_object_mut() {
            obj.insert("responseHeaders".to_string(), headers);
        }
    }

    let fulfill = json!({
        "id": *msg_id,
        "method": "Fetch.fulfillRequest",
        "params": fulfill_params
    });
    *msg_id += 1;
    send_cdp(socket, &fulfill);
}

const BRIDGE_SHIM_JS: &str = r#"(function(){
  var _cid=0,_p={};
  window.__luma_bridge_resolve=function(id,json){
    var p=_p[id];if(p){delete _p[id];
      var resp={ok:true,status:200,statusText:'OK',
        json:function(){return Promise.resolve(typeof json==='string'?JSON.parse(json):json)},
        text:function(){return Promise.resolve(typeof json==='string'?json:JSON.stringify(json))},
        clone:function(){return resp}};
      p(resp);}
  };
  window.__luma_bridge_reject=function(id,err){
    var p=_p[id];if(p){delete _p[id];p.reject(new Error(err))}
  };
  window.__luma_bridge_call=function(path,opts){
    var id=++_cid;
    return new Promise(function(resolve,reject){
      _p[id]={resolve:resolve,reject:reject};
      var body=(opts&&opts.body)?(typeof opts.body==='string'?opts.body:JSON.stringify(opts.body)):null;
      var payload=JSON.stringify({id:id,path:path,method:(opts&&opts.method)||'GET',body:body});
      window.__lumaNativeBridge(payload);
    });
  };
  var _origFetch=window.fetch;
  window.fetch=function(url,opts){
    if(typeof url==='string'&&url.indexOf('http://127.0.0.1:21775')===0){
      var path=url.substring(22);
      return window.__luma_bridge_call(path,opts);
    }
    return _origFetch.apply(this,arguments);
  };
  console.log('[LUMA] Bridge shim installed');
})();"#;

fn proxy_bridge_request(path: &str, method: &str, body: Option<&str>) -> Result<String, String> {
    let mut stream = TcpStream::connect("127.0.0.1:21775").map_err(|e| format!("connect: {}", e))?;
    stream.set_read_timeout(Some(Duration::from_secs(30))).ok();

    let mut req = format!(
        "GET {} HTTP/1.1\r\nHost: 127.0.0.1:21775\r\nConnection: close\r\n",
        path
    );
    let method_upper = method.to_uppercase();
    if method_upper == "POST" {
        if let Some(body_str) = body {
            let body_bytes = body_str.as_bytes();
            req = format!(
                "POST {} HTTP/1.1\r\nHost: 127.0.0.1:21775\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                path, body_bytes.len(), body_str
            );
        } else {
            req = format!(
                "POST {} HTTP/1.1\r\nHost: 127.0.0.1:21775\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
                path
            );
        }
    } else {
        req.push_str("\r\n");
    }

    stream.write_all(req.as_bytes()).map_err(|e| format!("write: {}", e))?;

    let mut resp = Vec::new();
    stream.read_to_end(&mut resp).map_err(|e| format!("read: {}", e))?;

    let resp_str = String::from_utf8_lossy(&resp);
    if let Some(pos) = resp_str.find("\r\n\r\n") {
        let resp_body = &resp_str[pos + 4..];
        // Extract status code from first line
        let status_ok = resp_str.starts_with("HTTP/1.1 200")
            || resp_str.starts_with("HTTP/1.0 200")
            || resp_str.starts_with("HTTP/1.1 204")
            || resp_str.starts_with("HTTP/1.0 204");
        if status_ok {
            Ok(resp_body.to_string())
        } else {
            Err(format!("HTTP {}", &resp_str[9..12]))
        }
    } else {
        Err("malformed response".to_string())
    }
}

fn handle_bridge_binding(
    socket: &mut tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>,
    msg_id: &mut u64,
    payload: &str,
) {
    let req: Value = match serde_json::from_str(payload) {
        Ok(v) => v,
        Err(_) => return,
    };
    let call_id = req.get("id").and_then(|i| i.as_u64()).unwrap_or(0);
    let path = req.get("path").and_then(|p| p.as_str()).unwrap_or("/");
    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("GET");
    let body = req.get("body").and_then(|b| b.as_str());

    log_to_temp(&format!("[cef_hook] Bridge proxy: {} {}", method, path));

    let result_json = match proxy_bridge_request(path, method, body) {
        Ok(body) => body,
        Err(e) => {
            log_to_temp(&format!("[cef_hook] Bridge proxy error: {}", e));
            let reject_fn = json!({
                "id": *msg_id,
                "method": "Runtime.callFunctionOn",
                "params": {
                    "functionDeclaration": "(function(id, err) { window.__luma_bridge_reject(id, err); })",
                    "arguments": [
                        {"type": "number", "value": call_id},
                        {"type": "string", "value": &e}
                    ]
                }
            });
            *msg_id += 1;
            send_cdp(socket, &reject_fn);
            return;
        }
    };

    let resolve_fn = json!({
        "id": *msg_id,
        "method": "Runtime.callFunctionOn",
        "params": {
            "functionDeclaration": "(function(id, json) { window.__luma_bridge_resolve(id, json); })",
            "arguments": [
                {"type": "number", "value": call_id},
                {"type": "string", "value": &result_json}
            ]
        }
    });
    *msg_id += 1;
    send_cdp(socket, &resolve_fn);
}

fn install_bridge_shim(
    socket: &mut tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>,
    msg_id: &mut u64,
) {
    let add_binding = json!({
        "id": *msg_id,
        "method": "Runtime.addBinding",
        "params": {"name": "__lumaNativeBridge"}
    });
    *msg_id += 1;
    if !send_cdp(socket, &add_binding) {
        log_to_temp("[cef_hook] Failed to addBinding");
        return;
    }
    log_to_temp("[cef_hook] Runtime.addBinding sent");

    let add_script = json!({
        "id": *msg_id,
        "method": "Page.addScriptToEvaluateOnNewDocument",
        "params": {
            "source": BRIDGE_SHIM_JS,
            "runImmediately": true
        }
    });
    *msg_id += 1;
    send_cdp(socket, &add_script);
    log_to_temp("[cef_hook] Bridge shim injected into all documents");
}

fn re_register_script(
    socket: &mut tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>,
    msg_id: &mut u64,
    theme_state: &ThemeState,
) {
    if let Some(ref js_content) = theme_state.js {
        let enable_page = json!({
            "id": *msg_id,
            "method": "Page.enable",
            "params": {}
        });
        *msg_id += 1;
        send_cdp(socket, &enable_page);

        let bypass_csp = json!({
            "id": *msg_id,
            "method": "Page.setBypassCSP",
            "params": {"enabled": true}
        });
        *msg_id += 1;
        send_cdp(socket, &bypass_csp);

        let add_script = json!({
            "id": *msg_id,
            "method": "Page.addScriptToEvaluateOnNewDocument",
            "params": {
                "source": js_content,
                "worldName": "",
                "runImmediately": true
            }
        });
        *msg_id += 1;
        send_cdp(socket, &add_script);
        log_to_temp("[cef_hook] Re-registered Page.enable + setBypassCSP + addScript");
    }
}

unsafe extern "system" fn dll_main_thread(_param: *mut c_void) -> u32 {
    log_to_temp("[cef_hook] DLL loaded into webhelper process");

    std::thread::sleep(std::time::Duration::from_millis(500));

    let port = match resolve_debug_port() {
        Some(p) => p,
        None => {
            log_to_temp("[cef_hook] No debug port found, exiting");
            return 0;
        }
    };

    handle_cdp_connection(port);

    0
}

#[no_mangle]
pub unsafe extern "system" fn DllMain(
    _hinst_dll: *mut c_void,
    fdw_reason: u32,
    _lpv_reserved: *mut c_void,
) -> i32 {
    match fdw_reason {
        1 => {
            std::thread::spawn(|| {
                dll_main_thread(std::ptr::null_mut());
            });
            1
        }
        0 | 2 | 3 => 1,
        _ => 0,
    }
}
