use mlua::prelude::*;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::RwLock;

use crate::plugin::BackendConfig;

struct LuaBackend {
    lua: Lua,
    plugin_id: String,
    _plugin_dir: PathBuf,
}

pub struct LuaBackendRouter {
    backends: RwLock<Vec<LuaBackend>>,
}

struct RouteEntry {
    plugin_idx: usize,
    pattern: String,
    method: String,
}

pub struct LuaRequest {
    pub method: String,
    pub path: String,
    pub body: String,
    pub headers: HashMap<String, String>,
    pub query: String,
}

pub struct LuaResponse {
    pub status: u16,
    pub body: String,
    pub content_type: String,
}

static ROUTER: std::sync::OnceLock<LuaBackendRouter> = std::sync::OnceLock::new();

pub fn get_router() -> &'static LuaBackendRouter {
    ROUTER.get_or_init(|| LuaBackendRouter {
        backends: RwLock::new(Vec::new()),
    })
}

fn detect_steam_root() -> String {
    crate::platform::find_steam_install()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| {
            #[cfg(target_os = "windows")]
            {
                "C:\\Program Files (x86)\\Steam".to_string()
            }
            #[cfg(target_os = "linux")]
            {
                let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
                format!("{}/.steam/steam", home)
            }
        })
}

fn register_host_functions(lua: &Lua, plugin_dir: &PathBuf) -> LuaResult<()> {
    let globals = lua.globals();

    let pd = plugin_dir.clone();
    globals.set(
        "plugin_dir",
        lua.create_function(move |_, ()| -> LuaResult<String> {
            Ok(pd.to_string_lossy().to_string())
        })?,
    )?;

    globals.set(
        "steam_path",
        lua.create_function(move |_, ()| -> LuaResult<String> { Ok(detect_steam_root()) })?,
    )?;

    globals.set(
        "local_appdata",
        lua.create_function(move |_, ()| -> LuaResult<String> {
            Ok(crate::platform::local_data_dir().to_string_lossy().to_string())
        })?,
    )?;

    globals.set(
        "file_exists",
        lua.create_function(move |_, path: String| -> LuaResult<bool> {
            Ok(std::path::Path::new(&path).exists())
        })?,
    )?;

    globals.set(
        "read_file",
        lua.create_function(move |_, path: String| -> LuaResult<String> {
            match std::fs::read_to_string(&path) {
                Ok(s) => Ok(s),
                Err(e) => Err(LuaError::RuntimeError(format!("read_file failed: {}", e))),
            }
        })?,
    )?;

    globals.set(
        "write_file",
        lua.create_function(
            move |_, (path, content): (String, String)| -> LuaResult<bool> {
                match std::fs::write(&path, &content) {
                    Ok(_) => Ok(true),
                    Err(e) => {
                        crate::log_to_temp(&format!("[lua] write_file error: {}", e));
                        Ok(false)
                    }
                }
            },
        )?,
    )?;

    globals.set(
        "log",
        lua.create_function(move |_, msg: String| -> LuaResult<()> {
            crate::log_to_temp(&format!("[lua] {}", msg));
            Ok(())
        })?,
    )?;

    {
        let lc = lua.clone();
        globals.set(
            "http_get",
            lc.clone().create_function(
                move |_, (url, timeout_secs): (String, Option<u32>)| -> LuaResult<LuaValue> {
                    let timeout = std::time::Duration::from_secs(timeout_secs.unwrap_or(10) as u64);
                    let client = reqwest::blocking::Client::builder()
                        .timeout(timeout)
                        .danger_accept_invalid_certs(true)
                        .build()
                        .map_err(|e| LuaError::RuntimeError(format!("http client: {}", e)))?;

                    match client.get(&url).send() {
                        Ok(resp) => {
                            let status = resp.status().as_u16();
                            let headers: HashMap<String, String> = resp
                                .headers()
                                .iter()
                                .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
                                .collect();
                            let body = resp.text().unwrap_or_default();
                            let tbl = lc.create_table()?;
                            tbl.set("status", status)?;
                            tbl.set("body", body)?;
                            tbl.set("ok", status >= 200 && status < 300)?;
                            let h = lc.create_table()?;
                            for (k, v) in &headers {
                                h.set(k.as_str(), v.as_str())?;
                            }
                            tbl.set("headers", h)?;
                            Ok(LuaValue::Table(tbl))
                        }
                        Err(e) => {
                            let tbl = lc.create_table()?;
                            tbl.set("status", 0)?;
                            tbl.set("ok", false)?;
                            tbl.set("error", format!("{}", e))?;
                            Ok(LuaValue::Table(tbl))
                        }
                    }
                },
            )?,
        )?;
    }

    {
        let lc = lua.clone();
        globals.set(
            "http_get_headers",
            lc.clone().create_function(
                move |_,
                      (url, headers_tbl, timeout_secs): (
                    String,
                    Option<mlua::Table>,
                    Option<u32>,
                )|
                      -> LuaResult<LuaValue> {
                    let timeout = std::time::Duration::from_secs(timeout_secs.unwrap_or(10) as u64);
                    let client = reqwest::blocking::Client::builder()
                        .timeout(timeout)
                        .danger_accept_invalid_certs(true)
                        .build()
                        .map_err(|e| LuaError::RuntimeError(format!("http client: {}", e)))?;

                    let mut req = client.get(&url);
                    if let Some(hdrs) = headers_tbl {
                        for pair in hdrs.pairs::<String, String>() {
                            let (k, v) = pair.map_err(|e| {
                                LuaError::RuntimeError(format!("header pair: {}", e))
                            })?;
                            req = req.header(&k, &v);
                        }
                    }

                    match req.send() {
                        Ok(resp) => {
                            let status = resp.status().as_u16();
                            let headers: HashMap<String, String> = resp
                                .headers()
                                .iter()
                                .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
                                .collect();
                            let body = resp.text().unwrap_or_default();
                            let tbl = lc.create_table()?;
                            tbl.set("status", status)?;
                            tbl.set("body", body)?;
                            tbl.set("ok", status >= 200 && status < 300)?;
                            let h = lc.create_table()?;
                            for (k, v) in &headers {
                                h.set(k.as_str(), v.as_str())?;
                            }
                            tbl.set("headers", h)?;
                            Ok(LuaValue::Table(tbl))
                        }
                        Err(e) => {
                            let tbl = lc.create_table()?;
                            tbl.set("status", 0)?;
                            tbl.set("ok", false)?;
                            tbl.set("error", format!("{}", e))?;
                            Ok(LuaValue::Table(tbl))
                        }
                    }
                },
            )?,
        )?;
    }

    {
        let lc = lua.clone();
        globals.set(
            "http_post",
            lc.clone().create_function(
                move |_,
                      (url, body, timeout_secs): (String, Option<String>, Option<u32>)|
                      -> LuaResult<LuaValue> {
                    let timeout = std::time::Duration::from_secs(timeout_secs.unwrap_or(10) as u64);
                    let client = reqwest::blocking::Client::builder()
                        .timeout(timeout)
                        .danger_accept_invalid_certs(true)
                        .build()
                        .map_err(|e| LuaError::RuntimeError(format!("http client: {}", e)))?;

                    let body_str = body.unwrap_or_default();
                    match client
                        .post(&url)
                        .header("Content-Type", "application/json")
                        .body(body_str)
                        .send()
                    {
                        Ok(resp) => {
                            let status = resp.status().as_u16();
                            let resp_body = resp.text().unwrap_or_default();
                            let tbl = lc.create_table()?;
                            tbl.set("status", status)?;
                            tbl.set("body", resp_body)?;
                            tbl.set("ok", status >= 200 && status < 300)?;
                            Ok(LuaValue::Table(tbl))
                        }
                        Err(e) => {
                            let tbl = lc.create_table()?;
                            tbl.set("status", 0)?;
                            tbl.set("ok", false)?;
                            tbl.set("error", format!("{}", e))?;
                            Ok(LuaValue::Table(tbl))
                        }
                    }
                },
            )?,
        )?;
    }

    globals.set(
        "dir_exists",
        lua.create_function(move |_, path: String| -> LuaResult<bool> {
            Ok(std::path::Path::new(&path).is_dir())
        })?,
    )?;

    globals.set(
        "file_mtime",
        lua.create_function(move |_, path: String| -> LuaResult<i64> {
            match std::fs::metadata(&path) {
                Ok(meta) => match meta.modified() {
                    Ok(time) => {
                        let duration = time.duration_since(std::time::UNIX_EPOCH).map_err(|e| {
                            LuaError::RuntimeError(format!("mtime conversion: {}", e))
                        })?;
                        Ok(duration.as_secs() as i64)
                    }
                    Err(e) => Err(LuaError::RuntimeError(format!("file_mtime: {}", e))),
                },
                Err(e) => Err(LuaError::RuntimeError(format!("file_mtime: {}", e))),
            }
        })?,
    )?;

    globals.set(
        "list_dir",
        lua.create_function(move |_, path: String| -> LuaResult<Vec<String>> {
            let mut entries = Vec::new();
            if let Ok(rd) = std::fs::read_dir(&path) {
                for entry in rd.flatten() {
                    entries.push(entry.file_name().to_string_lossy().to_string());
                }
            }
            Ok(entries)
        })?,
    )?;

    {
        globals.set(
            "spawn_thread",
            lua.create_function(move |_, func: LuaFunction| -> LuaResult<()> {
                std::thread::spawn(move || match func.call::<()>(()) {
                    Ok(()) => {}
                    Err(e) => {
                        crate::log_to_temp(&format!("[lua] [spawn_thread] Thread crashed: {}", e));
                    }
                });
                Ok(())
            })?,
        )?;
    }

    globals.set(
        "json_encode",
        lua.create_function(move |_, val: LuaValue| -> LuaResult<String> {
            let json_val = lua_value_to_json(&val)?;
            Ok(serde_json::to_string(&json_val).unwrap_or_default())
        })?,
    )?;

    {
        let lc = lua.clone();
        globals.set(
            "json_decode",
            lc.clone()
                .create_function(move |_, s: String| -> LuaResult<LuaValue> {
                    let json_val: serde_json::Value = serde_json::from_str(&s)
                        .map_err(|e| LuaError::RuntimeError(format!("json decode: {}", e)))?;
                    json_to_lua_value(&lc, &json_val)
                })?,
        )?;
    }

    globals.set(
        "steam_open_library",
        lua.create_function(move |_, app_id: String| -> LuaResult<bool> {
            let uri = format!("steam://nav/games/details/{}", app_id);
            let _ = open::that(&uri);
            Ok(true)
        })?,
    )?;

    globals.set(
        "sleep_ms",
        lua.create_function(move |_, ms: u64| -> LuaResult<()> {
            std::thread::sleep(std::time::Duration::from_millis(ms));
            Ok(())
        })?,
    )?;

    globals.set(
        "lua_dir_path",
        lua.create_function(move |_, app_id: String| -> LuaResult<String> {
            let steam = detect_steam_root();
            let sep = crate::platform::SEP;
            Ok(format!(
                "{}{}config{}lua{}{}.lua",
                steam, sep, sep, sep, app_id
            ))
        })?,
    )?;

    globals.set(
        "manifest_dir_path",
        lua.create_function(move |_, app_id: String| -> LuaResult<String> {
            let steam = detect_steam_root();
            let sep = crate::platform::SEP;
            Ok(format!(
                "{}{}depotcache{}{}.manifest",
                steam, sep, sep, app_id
            ))
        })?,
    )?;

    globals.set(
        "base64_encode",
        lua.create_function(move |_, data: String| -> LuaResult<String> {
            use base64::Engine;
            Ok(base64::engine::general_purpose::STANDARD.encode(data.as_bytes()))
        })?,
    )?;

    globals.set(
        "base64_decode",
        lua.create_function(move |_, data: String| -> LuaResult<String> {
            use base64::Engine;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(data.as_bytes())
                .map_err(|e| LuaError::RuntimeError(format!("base64 decode: {}", e)))?;
            String::from_utf8(bytes).map_err(|e| LuaError::RuntimeError(format!("utf8: {}", e)))
        })?,
    )?;

    Ok(())
}

fn lua_value_to_json(val: &LuaValue) -> LuaResult<serde_json::Value> {
    match val {
        LuaValue::Nil => Ok(serde_json::Value::Null),
        LuaValue::Boolean(b) => Ok(serde_json::Value::Bool(*b)),
        LuaValue::Integer(n) => Ok(serde_json::json!(*n)),
        LuaValue::Number(n) => Ok(serde_json::json!(*n)),
        LuaValue::String(s) => Ok(serde_json::Value::String(s.to_str()?.to_string())),
        LuaValue::Table(t) => {
            let mut map = serde_json::Map::new();
            let mut is_array = true;
            let mut max_idx = 0;
            for pair in t.pairs::<LuaValue, LuaValue>().flatten() {
                if let LuaValue::Integer(idx) = &pair.0 {
                    if *idx >= 1 {
                        max_idx = max_idx.max(*idx as usize);
                    }
                } else {
                    is_array = false;
                }
            }
            if is_array && max_idx > 0 {
                let mut arr = Vec::with_capacity(max_idx);
                for i in 1..=max_idx {
                    let v: Option<LuaValue> = t.get(i).ok();
                    arr.push(match v {
                        Some(v) => lua_value_to_json(&v)?,
                        None => serde_json::Value::Null,
                    });
                }
                Ok(serde_json::Value::Array(arr))
            } else {
                for pair in t.pairs::<String, LuaValue>().flatten() {
                    map.insert(pair.0, lua_value_to_json(&pair.1)?);
                }
                Ok(serde_json::Value::Object(map))
            }
        }
        _ => Ok(serde_json::Value::Null),
    }
}

fn json_to_lua_value(lua: &Lua, val: &serde_json::Value) -> LuaResult<LuaValue> {
    match val {
        serde_json::Value::Null => Ok(LuaValue::Nil),
        serde_json::Value::Bool(b) => Ok(LuaValue::Boolean(*b)),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(LuaValue::Integer(i))
            } else if let Some(f) = n.as_f64() {
                Ok(LuaValue::Number(f))
            } else {
                Ok(LuaValue::Nil)
            }
        }
        serde_json::Value::String(s) => Ok(LuaValue::String(lua.create_string(s)?)),
        serde_json::Value::Array(arr) => {
            let tbl = lua.create_table()?;
            for (i, v) in arr.iter().enumerate() {
                tbl.set(i + 1, json_to_lua_value(lua, v)?)?;
            }
            Ok(LuaValue::Table(tbl))
        }
        serde_json::Value::Object(map) => {
            let tbl = lua.create_table()?;
            for (k, v) in map {
                tbl.set(k.as_str(), json_to_lua_value(lua, v)?)?;
            }
            Ok(LuaValue::Table(tbl))
        }
    }
}

pub fn load_lua_backend(
    plugin_id: &str,
    plugin_dir: &PathBuf,
    _config: &BackendConfig,
) -> LuaResult<()> {
    let backend_script = plugin_dir.join("backend.lua");
    if !backend_script.exists() {
        crate::log_to_temp(&format!("[lua] No backend.lua for plugin {}", plugin_id));
        return Ok(());
    }

    let lua = Lua::new();
    register_host_functions(&lua, plugin_dir)?;

    let script = std::fs::read_to_string(&backend_script)
        .map_err(|e| LuaError::RuntimeError(format!("read backend.lua: {}", e)))?;

    lua.load(&script)
        .set_name(format!("{}/backend.lua", plugin_id))
        .exec()
        .map_err(|e| {
            crate::log_to_temp(&format!("[lua] Script error in {}: {}", plugin_id, e));
            e
        })?;

    crate::log_to_temp(&format!("[lua] Loaded backend for plugin: {}", plugin_id));

    let router = get_router();
    let mut backends = router.backends.write().unwrap();
    backends.push(LuaBackend {
        lua,
        plugin_id: plugin_id.to_string(),
        _plugin_dir: plugin_dir.clone(),
    });

    Ok(())
}

pub fn handle_lua_request(req: &LuaRequest) -> Option<LuaResponse> {
    let router = get_router();
    let backends = router.backends.read().unwrap();

    for backend in backends.iter() {
        let globals = backend.lua.globals();

        let handlers: Option<LuaTable> = globals.get("routes").ok();
        if let Some(routes) = handlers {
            let route_key = format!("{} {}", req.method, req.path);
            let handler: Option<LuaFunction> = routes.get(route_key.as_str()).ok().flatten();

            if handler.is_none() {
                for pair in routes.pairs::<String, LuaFunction>().flatten() {
                    let pattern = pair.0;
                    let parts: Vec<&str> = pattern.splitn(2, ' ').collect();
                    if parts.len() == 2 && parts[0] == req.method {
                        let path_pattern = parts[1];
                        if path_matches(path_pattern, &req.path) {
                            let req_table = build_lua_request_table(&backend.lua, req).ok()?;
                            let result: LuaValue = pair.1.call(req_table).ok()?;

                            if let LuaValue::Table(resp_tbl) = result {
                                let status: u16 = resp_tbl.get("status").unwrap_or(200);
                                let body: String = resp_tbl.get("body").unwrap_or_default();
                                let ct: String = resp_tbl
                                    .get("content_type")
                                    .or_else(|_| resp_tbl.get("contentType"))
                                    .unwrap_or_else(|_| "application/json".to_string());

                                return Some(LuaResponse {
                                    status,
                                    body,
                                    content_type: ct,
                                });
                            }
                            return None;
                        }
                    }
                }
            } else if let Some(f) = handler {
                let req_table = build_lua_request_table(&backend.lua, req).ok()?;
                let result: LuaValue = f.call(req_table).ok()?;

                if let LuaValue::Table(resp_tbl) = result {
                    let status: u16 = resp_tbl.get("status").unwrap_or(200);
                    let body: String = resp_tbl.get("body").unwrap_or_default();
                    let ct: String = resp_tbl
                        .get("content_type")
                        .or_else(|_| resp_tbl.get("contentType"))
                        .unwrap_or_else(|_| "application/json".to_string());

                    return Some(LuaResponse {
                        status,
                        body,
                        content_type: ct,
                    });
                }
                return None;
            }
        }
    }

    None
}

fn path_matches(pattern: &str, path: &str) -> bool {
    let pattern_parts: Vec<&str> = pattern.trim_start_matches('/').split('/').collect();
    let path_parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();

    if pattern_parts.len() != path_parts.len() {
        return false;
    }

    for (pp, rp) in pattern_parts.iter().zip(path_parts.iter()) {
        if pp.starts_with(':') {
            continue;
        }
        if pp != rp {
            return false;
        }
    }
    true
}

fn build_lua_request_table(lua: &Lua, req: &LuaRequest) -> LuaResult<LuaTable> {
    let tbl = lua.create_table()?;
    tbl.set("method", req.method.as_str())?;
    tbl.set("path", req.path.as_str())?;
    tbl.set("body", req.body.as_str())?;
    tbl.set("query", req.query.as_str())?;

    let headers = lua.create_table()?;
    for (k, v) in &req.headers {
        headers.set(k.as_str(), v.as_str())?;
    }
    tbl.set("headers", headers)?;

    if let Ok(json_body) = serde_json::from_str::<serde_json::Value>(&req.body) {
        tbl.set("json", json_to_lua_value(lua, &json_body)?)?;
    }

    Ok(tbl)
}

pub fn reload_lua_backend(
    plugin_id: &str,
    plugin_dir: &PathBuf,
    config: &BackendConfig,
) -> LuaResult<()> {
    {
        let router = get_router();
        let mut backends = router.backends.write().unwrap();
        backends.retain(|b| b.plugin_id != plugin_id);
    }
    load_lua_backend(plugin_id, plugin_dir, config)
}


// ─── Temporary gate: extension backend time budget ──────────────────────────
// Loads the sibling steam-store-helper backend.lua and proves the two rules the
// bridge depends on: GET /api/sources answers inside its own budget even when
// every provider hangs, and GET /api/provider-stats stops hitting the network
// on every poll. Removed once the Tanda A gates are green.
#[cfg(test)]
mod bridge_budget_tests {
    use super::*;
    use std::path::Path;
    use std::time::{Duration, Instant};

    fn backend_source() -> Option<String> {
        let path = PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../lumaforge-extensions/extensions/steam-store-helper/backend.lua"
        ));
        std::fs::read_to_string(path).ok()
    }

    fn fixture_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("luma-budget-{}", tag));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Four remote providers: a hung first check burns its full 5s cap and
        // leaves 1s of the 6s budget — under the 2s floor, so the rest must be
        // reported as not checked rather than started and failed as well.

        let cfg = r#"{"downloads":{"providers":[
            {"id":"hubcapdb","name":"Hubcapdb","baseUrl":"https://hubcapmanifest.com","enabled":true,"apiKey":"k"},
            {"id":"ryuu","name":"Ryuu","baseUrl":"https://generator.ryuu.lol","enabled":true,"apiKey":"k"},
            {"id":"custom","name":"Custom","baseUrl":"https://api.example.com","enabled":true,"apiKey":""},
            {"id":"custom2","name":"Custom 2","baseUrl":"https://api.example.org","enabled":true,"apiKey":""},
            {"id":"steamkeys","name":"Key Generator","baseUrl":"","enabled":true}
        ]}}"#;
        std::fs::write(dir.join("config.json"), cfg).unwrap();
        dir
    }

    /// A provider that never answers: burns exactly the timeout it is handed
    /// and reports a transport failure — the worst case a route must bound.
    fn hang_http(lua: &Lua) {
        let lc = lua.clone();
        let stub = lc
            .clone()
            .create_function(
                move |_, (_url, _hdrs, timeout): (String, Option<mlua::Table>, Option<u32>)| {
                    std::thread::sleep(Duration::from_secs(timeout.unwrap_or(10) as u64));
                    let tbl = lc.create_table()?;
                    tbl.set("status", 0)?;
                    tbl.set("ok", false)?;
                    tbl.set("error", "simulated timeout")?;
                    Ok(LuaValue::Table(tbl))
                },
            )
            .unwrap();
        lua.globals().set("http_get_headers", stub).unwrap();
    }

    fn load_backend(dir: &Path, source: &str) -> Lua {
        let lua = Lua::new();
        let dir_str = dir.to_string_lossy().to_string();
        register_host_functions(&lua, &dir.to_path_buf()).unwrap();
        lua.globals()
            .set(
                "local_appdata",
                lua.create_function(move |_, ()| Ok(dir_str.clone())).unwrap(),
            )
            .unwrap();
        lua.load(source)
            .set_name("backend.lua")
            .exec()
            .expect("backend.lua must compile and run");
        lua
    }

    fn call_route(lua: &Lua, method: &str, path: &str) -> (Duration, String) {
        let routes: LuaTable = lua.globals().get("routes").expect("routes table");
        let exact = format!("{} {}", method, path);
        let handler: LuaFunction = match routes
            .get::<Option<LuaFunction>>(exact.as_str())
            .ok()
            .flatten()
        {
            Some(f) => f,
            None => {
                // Same fallback handle_lua_request uses for ":id" patterns.
                let mut found = None;
                for pair in routes.pairs::<String, LuaFunction>().flatten() {
                    let mut parts = pair.0.splitn(2, ' ');
                    let pattern_method = parts.next().unwrap_or("");
                    let pattern = parts.next().unwrap_or("");
                    if pattern_method == method && path_matches(pattern, path) {
                        found = Some(pair.1);
                        break;
                    }
                }
                found.expect("route")
            }
        };
        let req = lua.create_table().unwrap();
        req.set("method", method).unwrap();
        req.set("path", path).unwrap();
        req.set("body", "").unwrap();
        req.set("query", "").unwrap();
        req.set("headers", lua.create_table().unwrap()).unwrap();
        let started = Instant::now();
        let out: LuaValue = handler.call(req).unwrap();
        let elapsed = started.elapsed();
        let body = match out {
            LuaValue::Table(t) => t.get::<String>("body").unwrap_or_default(),
            other => format!("unexpected value: {}", other.type_name()),
        };
        (elapsed, body)
    }

    #[test]
    fn sources_answer_inside_the_bridge_budget() {
        let Some(src) = backend_source() else { return };
        let dir = fixture_dir("sources");
        let lua = load_backend(&dir, &src);
        hang_http(&lua);

        let (elapsed, body) = call_route(&lua, "GET", "/api/sources/976730");
        assert!(
            elapsed >= Duration::from_secs(4) && elapsed < Duration::from_millis(7500),
            "sources took {:?} (expected ~5s of bounded checks)",
            elapsed
        );
        // The transport failure must name itself (cap reached) instead of the
        // blanket "Provider unreachable" that read like a dead provider.
        assert!(
            body.contains("No answer in 5s"),
            "timeout detail missing, body: {}",
            body
        );
        assert!(
            body.contains("Not checked"),
            "budget never spent, every provider was queried: {}",
            body
        );
    }

    #[test]
    fn provider_stats_stops_hitting_the_network_every_poll() {
        let Some(src) = backend_source() else { return };
        let dir = fixture_dir("stats");
        let lua = load_backend(&dir, &src);
        hang_http(&lua);

        let (first, _) = call_route(&lua, "GET", "/api/provider-stats");
        let (second, _) = call_route(&lua, "GET", "/api/provider-stats");
        assert!(
            first >= Duration::from_secs(2),
            "first call took {:?} — the stub was never used",
            first
        );
        assert!(
            second < Duration::from_secs(1),
            "cached call took {:?} — TTL cache is not serving the snapshot",
            second
        );
    }
}
