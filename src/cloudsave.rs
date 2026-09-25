pub fn try_handle_route(method: &str, path: &str, body: &str) -> Option<(u16, String)> {
    if !path.starts_with("/api/cloudsave") {
        return None;
    }
    w::handle(method, path, body)
}

/// Lightweight cloudsave rows for the unified catalog (no network, no names —
/// the catalog resolves names from its local appnames cache).
pub fn catalog_rows() -> Vec<serde_json::Value> {
    w::catalog_rows()
}

#[cfg(windows)]
mod w {
    use serde_json::{json, Value};
    use std::net::{TcpListener, TcpStream};
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    const GDRIVE_CLIENT_ID: &str =
        "1072944905499-vm2v2i5dvn0a0d2o4ca36i1vge8cvbn0.apps.googleusercontent.com";
    const GDRIVE_CLIENT_SECRET: &str = "v6V3fKV_zWU7iw1DrpO1rknX";
    const GDRIVE_SCOPE: &str = "https://www.googleapis.com/auth/drive.file";
    const GDRIVE_AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
    const GDRIVE_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
    const ONEDRIVE_CLIENT_ID: &str = "b15665d9-eda6-4092-8539-0eec376afd59";
    const ONEDRIVE_CLIENT_SECRET: &str = "qtyfaBBYA403=unZUP40~_#";
    const ONEDRIVE_SCOPE: &str = "offline_access https://graph.microsoft.com/Files.ReadWrite";
    const ONEDRIVE_AUTH_URL: &str = "https://login.microsoftonline.com/consumers/oauth2/v2.0/authorize";
    const ONEDRIVE_TOKEN_URL: &str = "https://login.microsoftonline.com/consumers/oauth2/v2.0/token";
    const ONEDRIVE_PORT: u16 = 53682;
    const DRIVE_API: &str = "https://www.googleapis.com/drive/v3/files";
    const DRIVE_FOLDER_MIME: &str = "application/vnd.google-apps.folder";

    struct AuthState {
        state: &'static str,
        provider: &'static str,
        log: Vec<String>,
    }

    impl AuthState {
        const fn new() -> Self {
            AuthState {
                state: "idle",
                provider: "",
                log: Vec::new(),
            }
        }
    }

    static AUTH: Mutex<AuthState> = Mutex::new(AuthState::new());

    fn now() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    fn truncate(s: &str, max: usize) -> String {
        if s.len() <= max {
            return s.to_string();
        }
        let mut end = max;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}...", &s[..end])
    }

    fn err_resp(msg: impl Into<String>) -> (u16, String) {
        (200, json!({"ok": false, "message": msg.into()}).to_string())
    }

    fn ok_resp() -> (u16, String) {
        (200, json!({"ok": true}).to_string())
    }

    // -------------------------------------------------------------------------
    // Paths / config
    // -------------------------------------------------------------------------

    fn cr_dir() -> Option<PathBuf> {
        Some(dirs::config_dir()?.join("CloudRedirect"))
    }

    fn config_path() -> Option<PathBuf> {
        Some(cr_dir()?.join("config.json"))
    }

    fn steam_root() -> Option<PathBuf> {
        crate::depot_downloader::steam_root()
    }

    fn cr_steam_dir() -> Option<PathBuf> {
        Some(steam_root()?.join("cloud_redirect"))
    }

    fn storage_root() -> Option<PathBuf> {
        Some(cr_steam_dir()?.join("storage"))
    }

    fn installed() -> bool {
        let dll_name = if cfg!(target_os = "linux") {
            "cloud_redirect.so"
        } else {
            "cloud_redirect.dll"
        };
        steam_root()
            .map(|r| r.join(dll_name).exists())
            .unwrap_or(false)
    }

    fn read_json_file(path: &Path) -> Option<Value> {
        let text = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&text).ok()
    }

    fn atomic_write(path: &Path, data: &[u8]) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let tmp = path.with_file_name(format!(".{name}.tmp"));
        std::fs::write(&tmp, data).map_err(|e| format!("Cannot write {}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, path).map_err(|e| format!("Cannot replace {}: {e}", path.display()))
    }

    fn write_json_file(path: &Path, v: &Value) -> Result<(), String> {
        let data = serde_json::to_vec_pretty(v).map_err(|e| e.to_string())?;
        atomic_write(path, &data)
    }

    fn read_config() -> Value {
        config_path()
            .and_then(|p| read_json_file(&p))
            .unwrap_or_else(|| json!({}))
    }

    fn update_config<F: FnOnce(&mut Value)>(f: F) -> Result<(), String> {
        let path = config_path().ok_or_else(|| "CloudRedirect config folder not found".to_string())?;
        let mut v = read_config();
        if !v.is_object() {
            v = json!({});
        }
        f(&mut v);
        write_json_file(&path, &v)
    }

    fn provider_name() -> String {
        read_config()
            .get("provider")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    }

    fn token_path_for(provider: &str) -> Option<PathBuf> {
        if let Some(p) = read_config()
            .get("token_paths")
            .and_then(|m| m.get(provider))
            .and_then(|v| v.as_str())
        {
            if !p.is_empty() {
                return Some(PathBuf::from(p));
            }
        }
        let name = match provider {
            "gdrive" => "google_tokens.json",
            "onedrive" => "onedrive_tokens.json",
            "r2" => "r2_credentials.json",
            "s3" => "s3_credentials.json",
            _ => return None,
        };
        Some(cr_dir()?.join(name))
    }

    fn set_token_path(cfg: &mut Value, provider: &str, path: &str) {
        if !cfg.get("token_paths").map(|v| v.is_object()).unwrap_or(false) {
            cfg["token_paths"] = json!({});
        }
        cfg["token_paths"][provider] = json!(path);
        cfg["provider"] = json!(provider);
        cfg["token_path"] = json!(path);
    }

    // -------------------------------------------------------------------------
    // DPAPI (same blobs as .NET ProtectedData / CloudRedirect TokenFile)
    // -------------------------------------------------------------------------

    fn dpapi_protect(data: &[u8]) -> Result<Vec<u8>, String> {
        use windows_sys::Win32::Foundation::LocalFree;
        use windows_sys::Win32::Security::Cryptography::{CryptProtectData, CRYPT_INTEGER_BLOB};
        unsafe {
            let input = CRYPT_INTEGER_BLOB {
                cbData: data.len() as u32,
                pbData: data.as_ptr() as *mut u8,
            };
            let mut output = CRYPT_INTEGER_BLOB {
                cbData: 0,
                pbData: std::ptr::null_mut(),
            };
            let ok = CryptProtectData(
                &input,
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                1,
                &mut output,
            );
            if ok == 0 {
                return Err(format!(
                    "CryptProtectData failed: {}",
                    std::io::Error::last_os_error()
                ));
            }
            let out = if output.pbData.is_null() || output.cbData == 0 {
                Vec::new()
            } else {
                std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec()
            };
            if !output.pbData.is_null() {
                LocalFree(output.pbData as *mut _);
            }
            Ok(out)
        }
    }

    fn dpapi_unprotect(data: &[u8]) -> Result<Vec<u8>, String> {
        use windows_sys::Win32::Foundation::LocalFree;
        use windows_sys::Win32::Security::Cryptography::{CryptUnprotectData, CRYPT_INTEGER_BLOB};
        unsafe {
            let input = CRYPT_INTEGER_BLOB {
                cbData: data.len() as u32,
                pbData: data.as_ptr() as *mut u8,
            };
            let mut output = CRYPT_INTEGER_BLOB {
                cbData: 0,
                pbData: std::ptr::null_mut(),
            };
            let mut desc: windows_sys::core::PWSTR = std::ptr::null_mut();
            let ok = CryptUnprotectData(
                &input,
                &mut desc,
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                1,
                &mut output,
            );
            if !desc.is_null() {
                LocalFree(desc as *mut _);
            }
            if ok == 0 {
                return Err(format!(
                    "CryptUnprotectData failed: {}",
                    std::io::Error::last_os_error()
                ));
            }
            let out = if output.pbData.is_null() || output.cbData == 0 {
                Vec::new()
            } else {
                std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec()
            };
            if !output.pbData.is_null() {
                LocalFree(output.pbData as *mut _);
            }
            Ok(out)
        }
    }

    fn read_token_file(path: &Path) -> Result<Value, String> {
        let raw = std::fs::read(path).map_err(|e| format!("Cannot read {}: {e}", path.display()))?;
        let json_bytes = if raw.first() == Some(&b'{') {
            raw
        } else {
            dpapi_unprotect(&raw)?
        };
        let text = String::from_utf8(json_bytes).map_err(|_| "Invalid token encoding".to_string())?;
        serde_json::from_str(&text).map_err(|e| format!("Invalid token file: {e}"))
    }

    fn write_token_file(path: &Path, v: &Value) -> Result<(), String> {
        let data = serde_json::to_vec_pretty(v).map_err(|e| e.to_string())?;
        let blob = dpapi_protect(&data)?;
        atomic_write(path, &blob)
    }

    // -------------------------------------------------------------------------
    // Auth state helpers
    // -------------------------------------------------------------------------

    fn auth_push(msg: &str) {
        if let Ok(mut a) = AUTH.lock() {
            a.log.push(msg.to_string());
            if a.log.len() > 80 {
                let excess = a.log.len() - 80;
                a.log.drain(0..excess);
            }
        }
    }

    fn auth_set_state(state: &'static str, provider: &'static str) {
        if let Ok(mut a) = AUTH.lock() {
            a.state = state;
            if !provider.is_empty() {
                a.provider = provider;
            }
        }
    }

    fn auth_fail(msg: &str) {
        auth_push(msg);
        auth_set_state("error", "");
        crate::log_to_temp(&format!("[cloudsave] {msg}"));
    }

    fn auth_snapshot() -> Value {
        match AUTH.lock() {
            Ok(a) => json!({"state": a.state, "provider": a.provider, "log": a.log}),
            Err(_) => json!({"state": "idle", "provider": "", "log": []}),
        }
    }

    // -------------------------------------------------------------------------
    // Storage scan
    // -------------------------------------------------------------------------

    fn is_meta_name(name: &str) -> bool {
        matches!(
            name,
            "cn.dat"
                | "cn.cloudredirect"
                | "root_token.dat"
                | "root_token.cloudredirect"
                | "file_tokens.dat"
                | "file_tokens.cloudredirect"
                | "pending_ops.cloudredirect"
                | "manifest.cloudredirect"
        ) || (name.starts_with("manifest.") && name.ends_with(".cloudredirect"))
    }

    fn scan_app_dir(dir: &Path) -> (u64, u64) {
        fn walk(dir: &Path, files: &mut u64, size: &mut u64) {
            let Ok(rd) = std::fs::read_dir(dir) else { return };
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                let Ok(ft) = e.file_type() else { continue };
                if ft.is_dir() {
                    walk(&e.path(), files, size);
                } else if ft.is_file() && !is_meta_name(&name) {
                    *files += 1;
                    *size += e.metadata().map(|m| m.len()).unwrap_or(0);
                }
            }
        }
        let mut files = 0u64;
        let mut size = 0u64;
        walk(dir, &mut files, &mut size);
        (files, size)
    }

    fn read_cn(app_dir: &Path) -> String {
        for name in ["cn.cloudredirect", "cn.dat"] {
            if let Ok(text) = std::fs::read_to_string(app_dir.join(name)) {
                let t = text.trim();
                if !t.is_empty() {
                    return t.to_string();
                }
            }
        }
        "0".to_string()
    }

    fn account_ids() -> Vec<String> {
        let mut out = Vec::new();
        if let Some(root) = storage_root() {
            if let Ok(rd) = std::fs::read_dir(root) {
                for e in rd.flatten() {
                    if e.path().is_dir() {
                        let name = e.file_name().to_string_lossy().into_owned();
                        if !name.is_empty() && name != "0" && name != "stats" {
                            out.push(name);
                        }
                    }
                }
            }
        }
        out.sort();
        out
    }

    fn appnames_path() -> PathBuf {
        crate::platform::local_data_dir().join("appnames.json")
    }

    fn fetch_app_names(ids: &[String]) -> std::collections::HashMap<String, String> {
        let mut cache: std::collections::HashMap<String, String> =
            read_json_file(&appnames_path())
                .and_then(|v| {
                    v.as_object().map(|o| {
                        o.iter()
                            .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                            .collect()
                    })
                })
                .unwrap_or_default();
        let missing: Vec<String> = ids
            .iter()
            .filter(|id| !cache.contains_key(*id) && id.parse::<u64>().is_ok())
            .cloned()
            .collect();
        if missing.is_empty() {
            return cache;
        }
        let client = match reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(8))
            .build()
        {
            Ok(c) => c,
            Err(_) => return cache,
        };
        // None = transient failure (retry next time), Some(None) = definitive miss
        let results: Vec<(String, Option<Option<String>>)> = std::thread::scope(|s| {
            let client_ref = &client;
            let handles: Vec<_> = missing
                .iter()
                .map(|id| {
                    let id = id.clone();
                    s.spawn(move || {
                        let url = format!(
                            "https://store.steampowered.com/api/appdetails?appids={id}&filters=basic&l=english"
                        );
                        let r = match client_ref.get(&url).send() {
                            Ok(r) => r,
                            Err(_) => return (id, None),
                        };
                        let v: Value = match r.json() {
                            Ok(v) => v,
                            Err(_) => return (id, None),
                        };
                        let entry = match v.get(id.as_str()).or_else(|| {
                            v.as_object().and_then(|o| o.values().next())
                        }) {
                            Some(e) => e,
                            None => return (id, Some(None)),
                        };
                        match entry.get("success").and_then(|x| x.as_bool()) {
                            Some(false) => (id, Some(None)),
                            Some(true) => {
                                let name = entry
                                    .get("data")
                                    .and_then(|d| d.get("name"))
                                    .and_then(|n| n.as_str())
                                    .map(|n| n.to_string());
                                match name {
                                    Some(n) => (id, Some(Some(n))),
                                    None => (id, Some(None)),
                                }
                            }
                            None => (id, None),
                        }
                    })
                })
                .collect();
            handles
                .into_iter()
                .filter_map(|h| h.join().ok())
                .collect()
        });
        let mut changed = false;
        for (id, res) in results {
            match res {
                Some(Some(name)) => {
                    cache.insert(id, name);
                    changed = true;
                }
                Some(None) => {
                    cache.insert(id, String::new());
                    changed = true;
                }
                None => {}
            }
        }
        if changed {
            if let Ok(v) = serde_json::to_value(&cache) {
                let _ = write_json_file(&appnames_path(), &v);
            }
        }
        cache
    }

    // -------------------------------------------------------------------------
    // Routes: status / apps
    // -------------------------------------------------------------------------

    fn status_json() -> (u16, String) {
        let cfg = read_config();
        let provider = cfg
            .get("provider")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let sync_path = cfg
            .get("sync_path")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let installed = installed();
        let mut token_path = String::new();
        let mut logged_in = false;
        if !provider.is_empty() {
            if let Some(p) = token_path_for(&provider) {
                token_path = p.display().to_string();
                logged_in = match provider.as_str() {
                    "gdrive" | "onedrive" => read_token_file(&p)
                        .map(|t| {
                            !t.get("refresh_token")
                                .and_then(|r| r.as_str())
                                .unwrap_or("")
                                .is_empty()
                        })
                        .unwrap_or(false),
                    "r2" | "s3" => read_token_file(&p)
                        .map(|t| {
                            let has = |k: &str| {
                                !t.get(k).and_then(|x| x.as_str()).unwrap_or("").is_empty()
                            };
                            has("access_key_id") && has("secret_access_key") && has("bucket")
                        })
                        .unwrap_or(false),
                    "folder" => !sync_path.is_empty(),
                    _ => false,
                };
            }
        }
        let accounts = account_ids();
        let account_id = accounts.first().cloned().unwrap_or_default();
        let mut apps_synced = 0u64;
        if let Some(root) = storage_root() {
            for a in &accounts {
                if let Ok(rd) = std::fs::read_dir(root.join(a)) {
                    apps_synced += rd.flatten().filter(|e| e.path().is_dir()).count() as u64;
                }
            }
        }
        let stats = json!({
            "achievements": cfg.get("sync_achievements").and_then(|v| v.as_bool()).unwrap_or(false),
            "playtime": cfg.get("sync_playtime").and_then(|v| v.as_bool()).unwrap_or(false),
        });
        let mut tokens = json!({});
        for p in ["gdrive", "onedrive", "r2", "s3"] {
            let exists = token_path_for(p).map(|t| t.exists()).unwrap_or(false);
            tokens[p] = json!(exists);
        }
        (
            200,
            json!({
                "ok": true,
                "installed": installed,
                "provider": provider,
                "loggedIn": logged_in,
                "tokenPath": token_path,
                "syncPath": sync_path,
                "tokensAvailable": tokens,
                "appsSynced": apps_synced,
                "accountId": account_id,
                "statsSync": stats,
                "auth": auth_snapshot(),
            })
            .to_string(),
        )
    }

    fn apps_json() -> (u16, String) {
        struct Row {
            app_id: String,
            account: String,
            files: u64,
            size: u64,
            cn: String,
            modified: u64,
        }
        let mut rows: Vec<Row> = Vec::new();
        if let Some(root) = storage_root() {
            for acct in account_ids() {
                if let Ok(rd) = std::fs::read_dir(root.join(&acct)) {
                    for e in rd.flatten() {
                        if !e.path().is_dir() {
                            continue;
                        }
                        let app_id = e.file_name().to_string_lossy().into_owned();
                        if app_id == "0" || app_id.is_empty() {
                            continue;
                        }
                        let (files, size) = scan_app_dir(&e.path());
                        let cn = read_cn(&e.path());
                        let modified = e
                            .metadata()
                            .ok()
                            .and_then(|m| m.modified().ok())
                            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                            .map(|d| d.as_millis() as u64)
                            .unwrap_or(0);
                        rows.push(Row {
                            app_id,
                            account: acct.clone(),
                            files,
                            size,
                            cn,
                            modified,
                        });
                    }
                }
            }
        }
        rows.sort_by_key(|r| r.app_id.parse::<u64>().unwrap_or(u64::MAX));
        let ids: Vec<String> = rows.iter().map(|r| r.app_id.clone()).collect();
        let names = fetch_app_names(&ids);
        let files: Vec<Value> = rows
            .iter()
            .map(|r| {
                json!({
                    "appId": r.app_id,
                    "accountId": r.account,
                    "name": names
                        .get(&r.app_id)
                        .cloned()
                        .filter(|n| !n.is_empty())
                        .unwrap_or_else(|| r.app_id.clone()),
                    "fileCount": r.files,
                    "sizeBytes": r.size,
                    "cn": r.cn,
                    "modified": r.modified,
                })
            })
            .collect();
        (200, json!({"ok": true, "files": files}).to_string())
    }

    /// Catalog rows: appId + fileCount/sizeBytes/modified per app dir.
    pub(super) fn catalog_rows() -> Vec<Value> {
        let mut rows: Vec<Value> = Vec::new();
        let Some(root) = storage_root() else {
            return rows;
        };
        for acct in account_ids() {
            let Ok(rd) = std::fs::read_dir(root.join(&acct)) else {
                continue;
            };
            for e in rd.flatten() {
                if !e.path().is_dir() {
                    continue;
                }
                let app_id = e.file_name().to_string_lossy().into_owned();
                if app_id == "0" || app_id.is_empty() || app_id.parse::<u64>().is_err() {
                    continue;
                }
                let (files, size) = scan_app_dir(&e.path());
                let modified = e
                    .metadata()
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                rows.push(json!({
                    "appId": app_id,
                    "fileCount": files,
                    "sizeBytes": size,
                    "modified": modified,
                }));
            }
        }
        rows
    }

    // -------------------------------------------------------------------------
    // Routes: provider config / logout / stats toggles
    // -------------------------------------------------------------------------

    fn save_provider(body: &str) -> (u16, String) {
        let v: Value = match serde_json::from_str(body) {
            Ok(v) => v,
            Err(e) => return err_resp(format!("Invalid JSON: {e}")),
        };
        let provider = v.get("provider").and_then(|p| p.as_str()).unwrap_or("");
        match provider {
            "folder" => {
                let path = v
                    .get("path")
                    .and_then(|p| p.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string();
                if path.is_empty() {
                    return err_resp("Folder path is required");
                }
                if let Err(e) = update_config(|c| {
                    c["provider"] = json!("folder");
                    c["sync_path"] = json!(path);
                }) {
                    return err_resp(e);
                }
                crate::log_to_temp("[cloudsave] provider set to folder");
                ok_resp()
            }
            "r2" | "s3" => {
                let Some(obj) = v.get(provider).and_then(|o| o.as_object()) else {
                    return err_resp(format!("Missing '{provider}' settings"));
                };
                let Some(path) = token_path_for(provider) else {
                    return err_resp("Cannot resolve credentials path");
                };
                let mut creds = read_token_file(&path).unwrap_or_else(|_| json!({}));
                if !creds.is_object() {
                    creds = json!({});
                }
                for (k, val) in obj {
                    if val.as_str().map(|s| s.trim().is_empty()).unwrap_or(false) {
                        continue;
                    }
                    creds[k] = val.clone();
                }
                let required: &[&str] = if provider == "r2" {
                    &["account_id", "access_key_id", "secret_access_key", "bucket"]
                } else {
                    &["access_key_id", "secret_access_key", "bucket", "endpoint"]
                };
                for k in required {
                    if creds
                        .get(*k)
                        .and_then(|x| x.as_str())
                        .map(|s| s.trim().is_empty())
                        .unwrap_or(true)
                    {
                        return err_resp(format!("Missing field: {k}"));
                    }
                }
                if let Err(e) = write_token_file(&path, &creds) {
                    return err_resp(e);
                }
                let path_str = path.display().to_string();
                if let Err(e) = update_config(|c| set_token_path(c, provider, &path_str)) {
                    return err_resp(e);
                }
                crate::log_to_temp(&format!("[cloudsave] provider set to {provider}"));
                ok_resp()
            }
            "gdrive" | "onedrive" => {
                let Some(path) = token_path_for(provider) else {
                    return err_resp("Cannot resolve token path");
                };
                if !path.exists() {
                    return err_resp("Not signed in yet — use the Sign in button");
                }
                let path_str = path.display().to_string();
                if let Err(e) = update_config(|c| set_token_path(c, provider, &path_str)) {
                    return err_resp(e);
                }
                crate::log_to_temp(&format!("[cloudsave] provider set to {provider}"));
                ok_resp()
            }
            _ => err_resp("Unknown provider"),
        }
    }

    fn logout() -> (u16, String) {
        let provider = provider_name();
        if provider.is_empty() || provider == "folder" {
            return (200, json!({"ok": true, "message": "No account to sign out"}).to_string());
        }
        let path = token_path_for(&provider);
        let mut removed = false;
        if let Some(p) = &path {
            if p.exists() {
                match std::fs::remove_file(p) {
                    Ok(()) => removed = true,
                    Err(e) => {
                        return err_resp(format!("Cannot remove {}: {e}", p.display()));
                    }
                }
            }
        }
        let msg = if removed { "Signed out" } else { "Already signed out" };
        crate::log_to_temp(&format!("[cloudsave] logout {provider}: {msg}"));
        (200, json!({"ok": true, "message": msg}).to_string())
    }

    fn stats_sync(body: &str) -> (u16, String) {
        let v: Value = match serde_json::from_str(body) {
            Ok(v) => v,
            Err(e) => return err_resp(format!("Invalid JSON: {e}")),
        };
        let mut keys: Vec<(&str, bool)> = Vec::new();
        if let Some(b) = v.get("achievements").and_then(|x| x.as_bool()) {
            keys.push(("sync_achievements", b));
        }
        if let Some(b) = v.get("playtime").and_then(|x| x.as_bool()) {
            keys.push(("sync_playtime", b));
        }
        if keys.is_empty() {
            return err_resp("Nothing to update");
        }
        if let Err(e) = update_config(|c| {
            for (k, b) in &keys {
                c[*k] = json!(b);
            }
        }) {
            return err_resp(e);
        }
        crate::log_to_temp(&format!("[cloudsave] stats sync updated: {keys:?}"));
        ok_resp()
    }

    // -------------------------------------------------------------------------
    // OAuth (browser sign-in for gdrive / onedrive)
    // -------------------------------------------------------------------------

    fn rand_b64(n: usize) -> String {
        use base64::Engine;
        use rand::RngCore;
        let mut buf = vec![0u8; n];
        rand::thread_rng().fill_bytes(&mut buf);
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&buf)
    }

    fn enc<I, K, V>(pairs: I) -> String
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        let mut s = url::form_urlencoded::Serializer::new(String::new());
        s.extend_pairs(pairs);
        s.finish()
    }

    fn parse_qs(query: &str) -> Vec<(String, String)> {
        url::form_urlencoded::parse(query.as_bytes())
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect()
    }

    fn qs_get(qs: &[(String, String)], key: &str) -> Option<String> {
        qs.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
    }

    fn page(title: &str, body: &str) -> String {
        format!(
            "<!DOCTYPE html><html><head><meta charset=\"utf-8\"><title>LumaForge</title>\
             <style>body{{background:#111318;color:#e6e6e6;font-family:'Segoe UI',sans-serif;\
             display:flex;align-items:center;justify-content:center;height:100vh;margin:0}}\
             .c{{text-align:center;max-width:440px;padding:24px}}h1{{font-size:20px}}\
             p{{color:#9aa4b2;font-size:14px;line-height:1.5;word-break:break-word}}</style>\
             </head><body><div class=\"c\"><h1>{title}</h1><p>{body}</p></div></body></html>"
        )
    }

    fn respond(stream: &mut TcpStream, status: &str, body: &str) {
        let resp = format!(
            "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = stream.write_all(resp.as_bytes());
        let _ = stream.flush();
    }

    fn read_request(stream: &mut TcpStream) -> String {
        let mut data = Vec::new();
        for _ in 0..8 {
            let mut buf = [0u8; 4096];
            match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    data.extend_from_slice(&buf[..n]);
                    if data.windows(4).any(|w| w == b"\r\n\r\n") || data.len() > 65536 {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        String::from_utf8_lossy(&data).into_owned()
    }

    fn start_login(body: &str) -> (u16, String) {
        let v: Value = match serde_json::from_str(body) {
            Ok(v) => v,
            Err(e) => return err_resp(format!("Invalid JSON: {e}")),
        };
        let provider = v.get("provider").and_then(|p| p.as_str()).unwrap_or("");
        let plabel: &'static str = match provider {
            "gdrive" => "gdrive",
            "onedrive" => "onedrive",
            _ => "",
        };
        if plabel.is_empty() {
            return err_resp("Provider must be gdrive or onedrive");
        }
        {
            let Ok(mut a) = AUTH.lock() else {
                return err_resp("Auth state unavailable");
            };
            if a.state == "running" {
                return err_resp("Sign-in already in progress");
            }
            a.state = "running";
            a.provider = plabel;
            a.log.clear();
        }
        crate::log_to_temp(&format!("[cloudsave] sign-in started: {plabel}"));
        std::thread::spawn(move || run_oauth(plabel));
        ok_resp()
    }

    fn run_oauth(provider: &'static str) {
        let label = if provider == "gdrive" {
            "Google Drive"
        } else {
            "Microsoft OneDrive"
        };
        let (auth_base, token_url, client_id, client_secret, scope, fixed_port): (
            &str,
            &str,
            &str,
            &str,
            &str,
            Option<u16>,
        ) = match provider {
            "gdrive" => (
                GDRIVE_AUTH_URL,
                GDRIVE_TOKEN_URL,
                GDRIVE_CLIENT_ID,
                GDRIVE_CLIENT_SECRET,
                GDRIVE_SCOPE,
                None,
            ),
            _ => (
                ONEDRIVE_AUTH_URL,
                ONEDRIVE_TOKEN_URL,
                ONEDRIVE_CLIENT_ID,
                ONEDRIVE_CLIENT_SECRET,
                ONEDRIVE_SCOPE,
                Some(ONEDRIVE_PORT),
            ),
        };
        auth_push(&format!("Starting {label} sign-in..."));
        let listener = match fixed_port {
            Some(p) => match TcpListener::bind(("127.0.0.1", p)) {
                Ok(l) => l,
                Err(e) => {
                    auth_fail(&format!("Cannot bind port {p} (in use?): {e}"));
                    return;
                }
            },
            None => match TcpListener::bind(("127.0.0.1", 0)) {
                Ok(l) => l,
                Err(e) => {
                    auth_fail(&format!("Cannot bind a local port: {e}"));
                    return;
                }
            },
        };
        let port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
        let redirect = format!("http://localhost:{port}/callback");
        auth_push(&format!("Waiting for browser on port {port}"));

        let state = rand_b64(32);
        let verifier = rand_b64(64);
        use base64::Engine;
        use sha2::Digest;
        let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(sha2::Sha256::digest(verifier.as_bytes()));
        let mut pairs: Vec<(&str, &str)> = vec![
            ("client_id", client_id),
            ("redirect_uri", &redirect),
            ("response_type", "code"),
            ("scope", scope),
            ("state", &state),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
            ("prompt", "consent"),
        ];
        if provider == "gdrive" {
            pairs.push(("access_type", "offline"));
            pairs.push(("include_granted_scopes", "true"));
        }
        let url = format!("{auth_base}?{}", enc(pairs));
        auth_push("Opening browser...");
        if let Err(e) = open::that(&url) {
            auth_push(&format!("Could not open browser: {e}"));
            auth_push(&format!("Open this URL manually: {url}"));
        }

        if let Err(e) = listener.set_nonblocking(true) {
            auth_fail(&format!("Listener error: {e}"));
            return;
        }
        let deadline = Instant::now() + Duration::from_secs(300);
        let code = loop {
            if Instant::now() >= deadline {
                auth_fail("Timed out waiting for browser authorization (5 minutes)");
                return;
            }
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                    let text = read_request(&mut stream);
                    let target = text.split_whitespace().nth(1).unwrap_or("").to_string();
                    let qs = parse_qs(target.splitn(2, '?').nth(1).unwrap_or(""));
                    if let Some(err) = qs_get(&qs, "error") {
                        let desc = qs_get(&qs, "error_description").unwrap_or_default();
                        respond(
                            &mut stream,
                            "200 OK",
                            &page("Sign-in failed", &format!("{err} {desc}")),
                        );
                        auth_fail(&format!("Authorization failed: {err} {desc}"));
                        return;
                    }
                    match (qs_get(&qs, "code"), qs_get(&qs, "state")) {
                        (Some(c), Some(s)) if s == state => {
                            respond(
                                &mut stream,
                                "200 OK",
                                &page(
                                    "Sign-in complete",
                                    "You can close this tab and return to Steam.",
                                ),
                            );
                            break c;
                        }
                        (Some(_), Some(_)) => {
                            respond(
                                &mut stream,
                                "200 OK",
                                &page("Sign-in failed", "State mismatch — please retry."),
                            );
                            auth_fail("State mismatch in OAuth callback");
                            return;
                        }
                        _ => respond(&mut stream, "204 No Content", ""),
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(120));
                }
                Err(e) => {
                    auth_fail(&format!("Listener error: {e}"));
                    return;
                }
            }
        };
        drop(listener);

        auth_push("Exchanging authorization code for tokens...");
        let client = match reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                auth_fail(&format!("HTTP client error: {e}"));
                return;
            }
        };
        let resp = client
            .post(token_url)
            .form(&[
                ("client_id", client_id),
                ("client_secret", client_secret),
                ("code", code.as_str()),
                ("redirect_uri", redirect.as_str()),
                ("grant_type", "authorization_code"),
                ("code_verifier", verifier.as_str()),
                ("scope", scope),
            ])
            .send();
        let resp = match resp {
            Ok(r) => r,
            Err(e) => {
                auth_fail(&format!("Token request failed: {e}"));
                return;
            }
        };
        let status = resp.status();
        let text = resp.text().unwrap_or_default();
        if !status.is_success() {
            auth_fail(&format!(
                "Token exchange failed ({status}): {}",
                truncate(&text, 300)
            ));
            return;
        }
        let mut tok: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(e) => {
                auth_fail(&format!("Invalid token response: {e}"));
                return;
            }
        };
        if !tok.is_object() {
            auth_fail("Invalid token response payload");
            return;
        }
        let access = tok
            .get("access_token")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let refresh = tok
            .get("refresh_token")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        if access.is_empty() || refresh.is_empty() {
            auth_fail("No access/refresh token returned — revoke the app access and retry");
            return;
        }
        let expires_in = tok.get("expires_in").and_then(|x| x.as_u64()).unwrap_or(3600);
        tok["expires_at"] = json!(now() + expires_in);
        let Some(path) = token_path_for(provider) else {
            auth_fail("Cannot resolve token path");
            return;
        };
        if let Err(e) = write_token_file(&path, &tok) {
            auth_fail(&format!("Cannot write token file: {e}"));
            return;
        }
        let path_str = path.display().to_string();
        if let Err(e) = update_config(|c| set_token_path(c, provider, &path_str)) {
            auth_fail(&format!("Cannot update config: {e}"));
            return;
        }
        auth_push(&format!("Signed in to {label}."));
        auth_set_state("success", provider);
        crate::log_to_temp(&format!("[cloudsave] sign-in success: {provider}"));
    }

    // -------------------------------------------------------------------------
    // Google Drive REST (delete support)
    // -------------------------------------------------------------------------

    fn gd_http(
        client: &reqwest::blocking::Client,
        method: &str,
        url: &str,
        token: &str,
    ) -> Result<(u16, String), String> {
        let req = match method {
            "GET" => client.get(url),
            "DELETE" => client.delete(url),
            other => return Err(format!("Unsupported method {other}")),
        };
        let resp = req
            .bearer_auth(token)
            .send()
            .map_err(|e| format!("Drive request failed: {e}"))?;
        let status = resp.status().as_u16();
        let text = resp.text().unwrap_or_default();
        Ok((status, text))
    }

    fn gd_ensure_access() -> Result<String, String> {
        let path = token_path_for("gdrive").ok_or("Google Drive token path not found")?;
        let mut tok = read_token_file(&path).map_err(|e| format!("Not signed in: {e}"))?;
        let refresh = tok
            .get("refresh_token")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        if refresh.is_empty() {
            return Err("Not signed in to Google Drive".to_string());
        }
        let exp = tok.get("expires_at").and_then(|x| x.as_u64()).unwrap_or(0);
        if exp > now() + 120 {
            let access = tok
                .get("access_token")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            if !access.is_empty() {
                return Ok(access);
            }
        }
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| e.to_string())?;
        let resp = client
            .post(GDRIVE_TOKEN_URL)
            .form(&[
                ("client_id", GDRIVE_CLIENT_ID),
                ("client_secret", GDRIVE_CLIENT_SECRET),
                ("refresh_token", refresh.as_str()),
                ("grant_type", "refresh_token"),
            ])
            .send()
            .map_err(|e| format!("Token refresh failed: {e}"))?;
        let status = resp.status();
        let text = resp.text().unwrap_or_default();
        if !status.is_success() {
            return Err(format!(
                "Token refresh failed ({status}): {}",
                truncate(&text, 300)
            ));
        }
        let v: Value =
            serde_json::from_str(&text).map_err(|e| format!("Invalid refresh response: {e}"))?;
        let access = v
            .get("access_token")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        if access.is_empty() {
            return Err("No access token in refresh response".to_string());
        }
        tok["access_token"] = json!(access);
        if let Some(rt) = v.get("refresh_token").and_then(|x| x.as_str()) {
            tok["refresh_token"] = json!(rt);
        }
        tok["expires_at"] = json!(now() + v.get("expires_in").and_then(|x| x.as_u64()).unwrap_or(3600));
        write_token_file(&path, &tok)?;
        Ok(access)
    }

    fn gd_find_folder(
        client: &reqwest::blocking::Client,
        token: &str,
        name: &str,
        parent: Option<&str>,
    ) -> Result<Option<String>, String> {
        let mut q = format!(
            "name = '{}' and mimeType = '{}' and trashed = false",
            name.replace('\'', "\\'"),
            DRIVE_FOLDER_MIME
        );
        match parent {
            Some(p) => q.push_str(&format!(" and '{p}' in parents")),
            None => q.push_str(" and 'root' in parents"),
        }
        let qs = enc([
            ("q", q.as_str()),
            ("fields", "files(id,createdTime)"),
            ("orderBy", "createdTime"),
            ("pageSize", "10"),
        ]);
        let (s, b) = gd_http(client, "GET", &format!("{DRIVE_API}?{qs}"), token)?;
        if s != 200 {
            return Err(format!("Drive folder lookup failed ({s}): {}", truncate(&b, 300)));
        }
        let v: Value = serde_json::from_str(&b).map_err(|e| e.to_string())?;
        Ok(v
            .get("files")
            .and_then(|f| f.as_array())
            .and_then(|a| a.first())
            .and_then(|f| f.get("id"))
            .and_then(|i| i.as_str())
            .map(|s| s.to_string()))
    }

    fn gd_list_children(
        client: &reqwest::blocking::Client,
        token: &str,
        folder: &str,
    ) -> Result<Vec<(String, String, bool)>, String> {
        let mut out = Vec::new();
        let mut page: Option<String> = None;
        loop {
            let q = format!("'{folder}' in parents and trashed = false");
            let mut pairs: Vec<(&str, &str)> = vec![
                ("q", q.as_str()),
                ("fields", "nextPageToken,files(id,name,mimeType)"),
                ("pageSize", "1000"),
            ];
            if let Some(t) = &page {
                pairs.push(("pageToken", t));
            }
            let qs = enc(pairs);
            let (s, b) = gd_http(client, "GET", &format!("{DRIVE_API}?{qs}"), token)?;
            if s != 200 {
                return Err(format!("Drive list failed ({s}): {}", truncate(&b, 300)));
            }
            let v: Value = serde_json::from_str(&b).map_err(|e| e.to_string())?;
            if let Some(files) = v.get("files").and_then(|f| f.as_array()) {
                for f in files {
                    let id = f.get("id").and_then(|x| x.as_str()).unwrap_or("");
                    let name = f.get("name").and_then(|x| x.as_str()).unwrap_or("");
                    let is_folder = f.get("mimeType").and_then(|x| x.as_str()) == Some(DRIVE_FOLDER_MIME);
                    if !id.is_empty() {
                        out.push((id.to_string(), name.to_string(), is_folder));
                    }
                }
            }
            match v.get("nextPageToken").and_then(|x| x.as_str()) {
                Some(t) => page = Some(t.to_string()),
                None => break,
            }
        }
        Ok(out)
    }

    fn gd_delete(client: &reqwest::blocking::Client, token: &str, id: &str) -> Result<(), String> {
        let (s, b) = gd_http(client, "DELETE", &format!("{DRIVE_API}/{id}"), token)?;
        if s == 200 || s == 204 || s == 404 {
            Ok(())
        } else {
            Err(format!("Delete failed ({s}): {}", truncate(&b, 300)))
        }
    }

    fn gd_delete_tree(
        client: &reqwest::blocking::Client,
        token: &str,
        folder: &str,
    ) -> Result<(u64, u64), String> {
        let children = gd_list_children(client, token, folder)?;
        let mut deleted = 0u64;
        let mut failed = 0u64;
        for (id, _name, is_folder) in children {
            if is_folder {
                match gd_delete_tree(client, token, &id) {
                    Ok((d, f)) => {
                        deleted += d;
                        failed += f;
                    }
                    Err(_) => failed += 1,
                }
            }
            match gd_delete(client, token, &id) {
                Ok(()) => deleted += 1,
                Err(_) => failed += 1,
            }
        }
        Ok((deleted, failed))
    }

    fn gdrive_delete_app(account: &str, app: &str) -> Result<u64, String> {
        let token = gd_ensure_access()?;
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| e.to_string())?;
        let root = gd_find_folder(&client, &token, "CloudRedirect", None)?
            .ok_or_else(|| "CloudRedirect folder not found in Google Drive".to_string())?;
        let acct = gd_find_folder(&client, &token, account, Some(&root))?
            .ok_or_else(|| "Account folder not found in Google Drive".to_string())?;
        let appf = gd_find_folder(&client, &token, app, Some(&acct))?
            .ok_or_else(|| "Game folder not found in Google Drive".to_string())?;
        let (mut deleted, mut failed) = gd_delete_tree(&client, &token, &appf)?;
        match gd_delete(&client, &token, &appf) {
            Ok(()) => deleted += 1,
            Err(_) => failed += 1,
        }
        if failed > 0 {
            return Err(format!("Deleted {deleted} items, {failed} failed"));
        }
        Ok(deleted)
    }

    // -------------------------------------------------------------------------
    // Route: delete a synced game (local + cloud)
    // -------------------------------------------------------------------------

    fn delete_app(id_raw: &str) -> (u16, String) {
        let id = id_raw.trim().to_string();
        if id.is_empty() || !id.chars().all(|c| c.is_ascii_digit()) {
            return err_resp("Invalid app id");
        }
        let mut local_deleted = false;
        let mut local_message: Option<String> = None;
        let mut account: Option<String> = None;
        match storage_root() {
            Some(root) => {
                let found = account_ids().into_iter().find(|acct| {
                    root.join(acct).join(&id).is_dir()
                });
                match found {
                    Some(acct) => {
                        account = Some(acct.clone());
                        match std::fs::remove_dir_all(root.join(&acct).join(&id)) {
                            Ok(()) => local_deleted = true,
                            Err(e) => local_message = Some(format!("Local delete failed: {e}")),
                        }
                    }
                    None => {
                        local_message = Some("Game not found in local storage".to_string());
                        let ids = account_ids();
                        if ids.len() == 1 {
                            account = Some(ids[0].clone());
                        }
                    }
                }
            }
            None => local_message = Some("Steam cloud_redirect folder not found".to_string()),
        }

        let provider = provider_name();
        let mut cloud_deleted = false;
        let mut cloud_count = 0u64;
        let mut cloud_message: Option<String> = None;
        if !installed() {
            let dll_name = if cfg!(target_os = "linux") {
                "cloud_redirect.so"
            } else {
                "cloud_redirect.dll"
            };
            cloud_message = Some(format!("{dll_name} is not installed"));
        } else if provider == "gdrive" {
            if let Some(acct) = &account {
                match gdrive_delete_app(acct, &id) {
                    Ok(n) => {
                        cloud_deleted = true;
                        cloud_count = n;
                    }
                    Err(e) => cloud_message = Some(e),
                }
            } else {
                cloud_message = Some("Cannot determine Steam account for cloud delete".to_string());
            }
        } else if provider.is_empty() {
            cloud_message = Some("No cloud provider configured".to_string());
        } else {
            cloud_message = Some(format!(
                "Cloud delete is not supported for provider '{provider}' yet (Google Drive only)"
            ));
        }

        crate::log_to_temp(&format!(
            "[cloudsave] delete {id}: local={local_deleted} cloud={cloud_deleted} items={cloud_count} provider={provider}"
        ));
        let message = local_message.or(cloud_message).unwrap_or_default();
        let ok = local_deleted || cloud_deleted;
        (
            200,
            json!({
                "ok": ok,
                "localDeleted": local_deleted,
                "cloudDeleted": cloud_deleted,
                "cloudDeletedCount": cloud_count,
                "message": message,
            })
            .to_string(),
        )
    }

    // -------------------------------------------------------------------------
    // Dispatcher
    // -------------------------------------------------------------------------

    pub(crate) fn handle(method: &str, path: &str, body: &str) -> Option<(u16, String)> {
        if path == "/api/cloudsave/status" && method == "GET" {
            return Some(status_json());
        }
        if path == "/api/cloudsave/apps" && method == "GET" {
            return Some(apps_json());
        }
        if path == "/api/cloudsave/login" && method == "POST" {
            return Some(start_login(body));
        }
        if path == "/api/cloudsave/save-provider" && method == "POST" {
            return Some(save_provider(body));
        }
        if path == "/api/cloudsave/logout" && method == "POST" {
            return Some(logout());
        }
        if path == "/api/cloudsave/stats-sync" && method == "POST" {
            return Some(stats_sync(body));
        }
        if path.starts_with("/api/cloudsave/apps/") && method == "DELETE" {
            return Some(delete_app(path.trim_start_matches("/api/cloudsave/apps/")));
        }
        None
    }

    use std::io::{Read, Write};
}

#[cfg(not(windows))]
mod w {
    pub(crate) fn handle(method: &str, path: &str, _body: &str) -> Option<(u16, String)> {
        if path.starts_with("/api/cloudsave") {
            let _ = method;
            return Some((
                200,
                serde_json::json!({
                    "ok": false,
                    "message": "Cloud Saves is only available on Windows"
                })
                .to_string(),
            ));
        }
        None
    }

    pub(crate) fn catalog_rows() -> Vec<serde_json::Value> {
        Vec::new()
    }
}
