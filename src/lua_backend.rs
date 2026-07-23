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
    let candidates = [
        "C:\\Program Files (x86)\\Steam",
        "C:\\Program Files (x86)\\Steam Luma",
    ];
    for c in &candidates {
        if std::path::Path::new(c).join("steam.exe").exists() {
            return c.to_string();
        }
    }
    if let Ok(local_appdata) = std::env::var("LOCALAPPDATA") {
        let p = std::path::PathBuf::from(&local_appdata).join("Steam");
        if p.join("steam.exe").exists() {
            return p.to_string_lossy().to_string();
        }
    }
    "C:\\Program Files (x86)\\Steam".to_string()
}

fn register_host_functions(lua: &Lua, plugin_dir: &PathBuf) -> LuaResult<()> {
    let globals = lua.globals();

    let pd = plugin_dir.clone();
    globals.set("plugin_dir", lua.create_function(move |_, ()| -> LuaResult<String> {
        Ok(pd.to_string_lossy().to_string())
    })?)?;

    globals.set("steam_path", lua.create_function(move |_, ()| -> LuaResult<String> {
        Ok(detect_steam_root())
    })?)?;

    globals.set("local_appdata", lua.create_function(move |_, ()| -> LuaResult<String> {
        Ok(std::env::var("LOCALAPPDATA").unwrap_or_default())
    })?)?;

    globals.set("file_exists", lua.create_function(move |_, path: String| -> LuaResult<bool> {
        Ok(std::path::Path::new(&path).exists())
    })?)?;

    globals.set("read_file", lua.create_function(move |_, path: String| -> LuaResult<String> {
        match std::fs::read_to_string(&path) {
            Ok(s) => Ok(s),
            Err(e) => Err(LuaError::RuntimeError(format!("read_file failed: {}", e))),
        }
    })?)?;

    globals.set("write_file", lua.create_function(move |_, (path, content): (String, String)| -> LuaResult<bool> {
        match std::fs::write(&path, &content) {
            Ok(_) => Ok(true),
            Err(e) => {
                crate::log_to_temp(&format!("[lua] write_file error: {}", e));
                Ok(false)
            }
        }
    })?)?;

    globals.set("log", lua.create_function(move |_, msg: String| -> LuaResult<()> {
        crate::log_to_temp(&format!("[lua] {}", msg));
        Ok(())
    })?)?;

    {
        let lc = lua.clone();
        globals.set("http_get", lc.clone().create_function(move |_, (url, timeout_secs): (String, Option<u32>)| -> LuaResult<LuaValue> {
            let timeout = std::time::Duration::from_secs(timeout_secs.unwrap_or(10) as u64);
            let client = reqwest::blocking::Client::builder()
                .timeout(timeout)
                .danger_accept_invalid_certs(true)
                .build()
                .map_err(|e| LuaError::RuntimeError(format!("http client: {}", e)))?;

            match client.get(&url).send() {
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    let headers: HashMap<String, String> = resp.headers()
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
        })?)?;
    }

    {
        let lc = lua.clone();
        globals.set("http_get_headers", lc.clone().create_function(move |_, (url, headers_tbl, timeout_secs): (String, Option<mlua::Table>, Option<u32>)| -> LuaResult<LuaValue> {
            let timeout = std::time::Duration::from_secs(timeout_secs.unwrap_or(10) as u64);
            let client = reqwest::blocking::Client::builder()
                .timeout(timeout)
                .danger_accept_invalid_certs(true)
                .build()
                .map_err(|e| LuaError::RuntimeError(format!("http client: {}", e)))?;

            let mut req = client.get(&url);
            if let Some(hdrs) = headers_tbl {
                for pair in hdrs.pairs::<String, String>() {
                    let (k, v) = pair.map_err(|e| LuaError::RuntimeError(format!("header pair: {}", e)))?;
                    req = req.header(&k, &v);
                }
            }

            match req.send() {
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    let headers: HashMap<String, String> = resp.headers()
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
        })?)?;
    }

    {
        let lc = lua.clone();
        globals.set("http_post", lc.clone().create_function(move |_, (url, body, timeout_secs): (String, Option<String>, Option<u32>)| -> LuaResult<LuaValue> {
            let timeout = std::time::Duration::from_secs(timeout_secs.unwrap_or(10) as u64);
            let client = reqwest::blocking::Client::builder()
                .timeout(timeout)
                .danger_accept_invalid_certs(true)
                .build()
                .map_err(|e| LuaError::RuntimeError(format!("http client: {}", e)))?;

            let body_str = body.unwrap_or_default();
            match client.post(&url)
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
        })?)?;
    }

    globals.set("dir_exists", lua.create_function(move |_, path: String| -> LuaResult<bool> {
        Ok(std::path::Path::new(&path).is_dir())
    })?)?;

    globals.set("list_dir", lua.create_function(move |_, path: String| -> LuaResult<Vec<String>> {
        let mut entries = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&path) {
            for entry in rd.flatten() {
                entries.push(entry.file_name().to_string_lossy().to_string());
            }
        }
        Ok(entries)
    })?)?;

    {
        globals.set("spawn_thread", lua.create_function(move |_, func: LuaFunction| -> LuaResult<()> {
            std::thread::spawn(move || {
                let _ = func.call::<()>(());
            });
            Ok(())
        })?)?;
    }

    globals.set("json_encode", lua.create_function(move |_, val: LuaValue| -> LuaResult<String> {
        let json_val = lua_value_to_json(&val)?;
        Ok(serde_json::to_string(&json_val).unwrap_or_default())
    })?)?;

    {
        let lc = lua.clone();
        globals.set("json_decode", lc.clone().create_function(move |_, s: String| -> LuaResult<LuaValue> {
            let json_val: serde_json::Value = serde_json::from_str(&s)
                .map_err(|e| LuaError::RuntimeError(format!("json decode: {}", e)))?;
            json_to_lua_value(&lc, &json_val)
        })?)?;
    }

    globals.set("steam_open_library", lua.create_function(move |_, app_id: String| -> LuaResult<bool> {
        let uri = format!("steam://nav/games/details/{}", app_id);
        let _ = std::process::Command::new("cmd")
            .args(["/c", "start", &uri])
            .spawn();
        Ok(true)
    })?)?;

    globals.set("sleep_ms", lua.create_function(move |_, ms: u64| -> LuaResult<()> {
        std::thread::sleep(std::time::Duration::from_millis(ms));
        Ok(())
    })?)?;

    globals.set("lua_dir_path", lua.create_function(move |_, app_id: String| -> LuaResult<String> {
        let steam = detect_steam_root();
        Ok(format!("{}\\config\\lua\\{}.lua", steam, app_id))
    })?)?;

    globals.set("manifest_dir_path", lua.create_function(move |_, app_id: String| -> LuaResult<String> {
        let steam = detect_steam_root();
        Ok(format!("{}\\depotcache\\{}.manifest", steam, app_id))
    })?)?;

    globals.set("base64_encode", lua.create_function(move |_, data: String| -> LuaResult<String> {
        use base64::Engine;
        Ok(base64::engine::general_purpose::STANDARD.encode(data.as_bytes()))
    })?)?;

    globals.set("base64_decode", lua.create_function(move |_, data: String| -> LuaResult<String> {
        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD.decode(data.as_bytes())
            .map_err(|e| LuaError::RuntimeError(format!("base64 decode: {}", e)))?;
        String::from_utf8(bytes).map_err(|e| LuaError::RuntimeError(format!("utf8: {}", e)))
    })?)?;

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

pub fn load_lua_backend(plugin_id: &str, plugin_dir: &PathBuf, _config: &BackendConfig) -> LuaResult<()> {
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
                                let ct: String = resp_tbl.get("content_type")
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
                    let ct: String = resp_tbl.get("content_type")
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

pub fn reload_lua_backend(plugin_id: &str, plugin_dir: &PathBuf, config: &BackendConfig) -> LuaResult<()> {
    {
        let router = get_router();
        let mut backends = router.backends.write().unwrap();
        backends.retain(|b| b.plugin_id != plugin_id);
    }
    load_lua_backend(plugin_id, plugin_dir, config)
}
