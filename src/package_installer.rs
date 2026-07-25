use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock};

// ---------------------------------------------------------------------------
// Download job state
// ---------------------------------------------------------------------------
struct DownloadJob {
    status: String,
    progress: u8,
    message: String,
    error: Option<String>,
    app_id: String,
    source_id: String,
    lua_count: u32,
    manifest_count: u32,
    files: Vec<InstalledFile>,
}

#[derive(serde::Serialize)]
struct InstalledFile {
    filename: String,
    #[serde(rename = "type")]
    file_type: String,
    size: usize,
}

static DOWNLOADS: OnceLock<RwLock<HashMap<String, DownloadJob>>> = OnceLock::new();

fn get_downloads() -> &'static RwLock<HashMap<String, DownloadJob>> {
    DOWNLOADS.get_or_init(|| RwLock::new(HashMap::new()))
}

fn update_job<F: FnOnce(&mut DownloadJob)>(request_id: &str, f: F) {
    if let Ok(mut map) = get_downloads().write() {
        if let Some(job) = map.get_mut(request_id) {
            f(job);
        }
    }
}

// ---------------------------------------------------------------------------
// Provider resolution (mirrors Lua load_providers)
// ---------------------------------------------------------------------------
struct Provider {
    id: String,
    name: String,
    base_url: String,
    api_key: Option<String>,
}

fn detect_steam_root() -> String {
    let candidates = [
        "C:\\Program Files (x86)\\Steam",
        "C:\\Program Files (x86)\\Steam Luma",
    ];
    for c in &candidates {
        if Path::new(c).join("steam.exe").exists() {
            return c.to_string();
        }
    }
    if let Ok(local_appdata) = std::env::var("LOCALAPPDATA") {
        let p = PathBuf::from(&local_appdata).join("Steam");
        if p.join("steam.exe").exists() {
            return p.to_string_lossy().to_string();
        }
    }
    "C:\\Program Files (x86)\\Steam".to_string()
}

fn config_path() -> PathBuf {
    if let Ok(lad) = std::env::var("LOCALAPPDATA") {
        PathBuf::from(lad).join("LumaForge").join("config.json")
    } else {
        PathBuf::from("C:\\Windows\\Temp\\lumaforge_config.json")
    }
}

fn load_providers_from_config() -> Vec<Provider> {
    let path = config_path();
    let Ok(raw) = std::fs::read_to_string(&path) else {
        crate::log_to_temp(&format!("[package] No config at {}", path.display()));
        return Vec::new();
    };
    let Ok(config): Result<Value, _> = serde_json::from_str(&raw) else {
        crate::log_to_temp("[package] Failed to parse config.json");
        return Vec::new();
    };

    let dl = config.get("downloads").unwrap_or(&config);
    let raw_providers = dl.get("providers").and_then(|v| v.as_array()).cloned().unwrap_or_default();

    raw_providers
        .into_iter()
        .map(|p| {
            let id = p.get("id").or(p.get("name"))
                .and_then(|v| v.as_str()).unwrap_or("unknown").to_string();
            Provider {
                id: id.clone(),
                name: p.get("name").and_then(|v| v.as_str()).unwrap_or(&id).to_string(),
                base_url: p.get("baseUrl").or(p.get("base_url"))
                    .and_then(|v| v.as_str()).unwrap_or("").to_string(),
                api_key: p.get("apiKey").or(p.get("api_key"))
                    .and_then(|v| v.as_str()).map(|s| s.to_string()),
            }
        })
        .collect()
}

fn resolve_provider(source_id: &str) -> Option<Provider> {
    let providers = load_providers_from_config();
    let lower = source_id.to_lowercase();
    providers.into_iter().find(|p| {
        p.enabled() && (p.id.to_lowercase() == lower || p.name.to_lowercase() == lower)
    })
}

impl Provider {
    fn enabled(&self) -> bool {
        !self.base_url.is_empty()
    }

    fn download_url(&self, app_id: &str) -> String {
        if self.id == "hubcapdb" {
            format!("{}/api/v1/manifest/{}", self.base_url, app_id)
        } else if self.id == "ryuu" {
            format!("{}/api/download/{}", self.base_url, app_id)
        } else {
            format!("{}/{}", self.base_url, app_id)
        }
    }

    fn build_headers(&self) -> HashMap<String, String> {
        let mut headers = HashMap::new();
        if let Some(ref key) = self.api_key {
            if !key.is_empty() {
                if self.id == "hubcapdb" {
                    headers.insert("Authorization".to_string(), format!("Bearer {}", key));
                } else if self.id == "ryuu" {
                    headers.insert("X-Auth-Key".to_string(), key.clone());
                }
            }
        }
        headers
    }
}

// ---------------------------------------------------------------------------
// Route handler — called from bridge.rs before Lua
// ---------------------------------------------------------------------------
pub fn try_handle_route(method: &str, path: &str, body: &str) -> Option<(u16, String)> {
    if method == "POST" && path == "/api/download" {
        return Some(handle_download_request(body));
    }
    if method == "GET" && path.starts_with("/api/download-status/") {
        let request_id = &path["/api/download-status/".len()..];
        return Some(handle_download_status(request_id));
    }
    None
}

// ---------------------------------------------------------------------------
// POST /api/download — accept request, spawn background thread
// ---------------------------------------------------------------------------
fn handle_download_request(body: &str) -> (u16, String) {
    let parsed: Value = serde_json::from_str(body).unwrap_or_default();

    let app_id = parsed.get("appId").or(parsed.get("app_id"))
        .and_then(|v| v.as_str()).unwrap_or("");
    let source_id = parsed.get("sourceId").or(parsed.get("source_id"))
        .and_then(|v| v.as_str()).unwrap_or("");

    if app_id.is_empty() {
        return (400, json!({"ok": false, "message": "Missing appId"}).to_string());
    }
    if source_id.is_empty() {
        return (400, json!({"ok": false, "message": "Missing sourceId"}).to_string());
    }

    let provider = match resolve_provider(source_id) {
        Some(p) => p,
        None => {
            return (400, json!({"ok": false, "message": format!("Provider not found: {}", source_id)}).to_string());
        }
    };

    let request_id = format!(
        "{}-{}-{}",
        app_id,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        rand::random::<u16>()
    );

    let job = DownloadJob {
        status: "queued".to_string(),
        progress: 0,
        message: "Queued".to_string(),
        error: None,
        app_id: app_id.to_string(),
        source_id: source_id.to_string(),
        lua_count: 0,
        manifest_count: 0,
        files: Vec::new(),
    };
    get_downloads().write().unwrap().insert(request_id.clone(), job);

    let rid = request_id.clone();
    std::thread::spawn(move || download_and_install(rid, provider));

    crate::log_to_temp(&format!("[package] Download accepted for {} via {}", app_id, source_id));
    (200, json!({"ok": true, "requestId": request_id}).to_string())
}

// ---------------------------------------------------------------------------
// GET /api/download-status/:id — return current job state
// ---------------------------------------------------------------------------
fn handle_download_status(request_id: &str) -> (u16, String) {
    let map = get_downloads().read().unwrap();
    match map.get(request_id) {
        Some(job) => {
            let resp = json!({
                "ok": true,
                "status": job.status,
                "progress": job.progress,
                "message": job.message,
                "errorCode": job.error,
                "appId": job.app_id,
                "luaCount": job.lua_count,
                "manifestCount": job.manifest_count,
            });
            (200, resp.to_string())
        }
        None => {
            (200, json!({
                "ok": false,
                "status": "failed",
                "progress": 0,
                "message": "Download not found",
                "errorCode": "NOT_FOUND"
            }).to_string())
        }
    }
}

// ---------------------------------------------------------------------------
// Background download thread — binary-safe ZIP extraction
// ---------------------------------------------------------------------------
fn download_and_install(request_id: String, provider: Provider) {
    let app_id = {
        let map = get_downloads().read().unwrap();
        match map.get(&request_id) {
            Some(j) => j.app_id.clone(),
            None => return,
        }
    };

    update_job(&request_id, |j| {
        j.status = "downloading".to_string();
        j.progress = 10;
        j.message = format!("Downloading from {}", provider.name);
    });

    let url = provider.download_url(&app_id);
    let headers = provider.build_headers();

    // Download as raw bytes — this is the critical fix over Lua's resp.text()
    let client = match reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .danger_accept_invalid_certs(true)
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            update_job(&request_id, |j| {
                j.status = "failed".to_string();
                j.message = format!("HTTP client error: {}", e);
                j.error = Some("CLIENT_ERROR".to_string());
            });
            return;
        }
    };

    let mut req = client.get(&url);
    for (k, v) in &headers {
        req = req.header(k.as_str(), v.as_str());
    }

    let response = match req.send() {
        Ok(r) => r,
        Err(e) => {
            update_job(&request_id, |j| {
                j.status = "failed".to_string();
                j.message = format!("Download failed: {}", e);
                j.error = Some("NETWORK_ERROR".to_string());
            });
            return;
        }
    };

    let status = response.status().as_u16();
    if !response.status().is_success() {
        update_job(&request_id, |j| {
            j.status = "failed".to_string();
            j.message = format!("Download failed: HTTP {}", status);
            j.error = Some(format!("HTTP_{}", status));
        });
        return;
    }

    let bytes = match response.bytes() {
        Ok(b) => b,
        Err(e) => {
            update_job(&request_id, |j| {
                j.status = "failed".to_string();
                j.message = format!("Failed to read response: {}", e);
                j.error = Some("READ_ERROR".to_string());
            });
            return;
        }
    };

    crate::log_to_temp(&format!("[package] Downloaded {} bytes for {}", bytes.len(), app_id));

    update_job(&request_id, |j| {
        j.progress = 50;
        j.message = "Extracting package".to_string();
        j.status = "extracting".to_string();
    });

    // Validate ZIP signature
    if bytes.len() < 4 || bytes[0] != 0x50 || bytes[1] != 0x4b {
        update_job(&request_id, |j| {
            j.status = "failed".to_string();
            j.message = "Response is not a valid ZIP package".to_string();
            j.error = Some("NOT_ZIP".to_string());
        });
        return;
    }

    // Extract ZIP
    let cursor = Cursor::new(bytes.as_ref());
    let mut archive = match zip::ZipArchive::new(cursor) {
        Ok(a) => a,
        Err(e) => {
            update_job(&request_id, |j| {
                j.status = "failed".to_string();
                j.message = format!("ZIP extraction failed: {}", e);
                j.error = Some("ZIP_ERROR".to_string());
            });
            return;
        }
    };

    let steam = detect_steam_root();
    let lua_dir = format!("{}\\config\\lua", steam);
    let manifest_dir = format!("{}\\depotcache", steam);

    // Ensure directories exist
    let _ = std::fs::create_dir_all(&lua_dir);
    let _ = std::fs::create_dir_all(&manifest_dir);

    let entry_count = archive.len();
    let mut lua_count: u32 = 0;
    let mut manifest_count: u32 = 0;
    let mut installed_files: Vec<InstalledFile> = Vec::new();
    let mut any_lua_installed = false;

    for i in 0..entry_count {
        let mut entry = match archive.by_index(i) {
            Ok(e) => e,
            Err(_) => continue,
        };

        let entry_name = entry.name().to_string();
        let basename = Path::new(&entry_name)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();

        if basename.is_empty() || basename.contains('/') || basename.contains('\\') {
            continue;
        }

        let ext = Path::new(&basename)
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default();

        if ext != "lua" && ext != "manifest" {
            crate::log_to_temp(&format!("[package] Skipping non-installable: {}", entry_name));
            continue;
        }

        let mut file_bytes = Vec::new();
        if std::io::Read::read_to_end(&mut entry, &mut file_bytes).is_err() {
            crate::log_to_temp(&format!("[package] Failed to read entry: {}", entry_name));
            continue;
        }

        let dest = if ext == "lua" {
            format!("{}\\{}", lua_dir, basename)
        } else {
            format!("{}\\{}", manifest_dir, basename)
        };

        match std::fs::write(&dest, &file_bytes) {
            Ok(()) => {
                let ftype = if ext == "lua" { "lua" } else { "manifest" };
                installed_files.push(InstalledFile {
                    filename: basename.clone(),
                    file_type: ftype.to_string(),
                    size: file_bytes.len(),
                });
                if ext == "lua" {
                    lua_count += 1;
                    any_lua_installed = true;
                } else {
                    manifest_count += 1;
                }
                crate::log_to_temp(&format!("[package] Installed {} ({} bytes) -> {}", basename, file_bytes.len(), dest));
            }
            Err(e) => {
                crate::log_to_temp(&format!("[package] Failed to write {}: {}", dest, e));
            }
        }

        // Update progress based on entry index
        let pct = 50 + ((i as u8 + 1) * 50 / (entry_count as u8).max(1));
        update_job(&request_id, |j| {
            j.progress = pct.min(99);
            j.message = format!("Installing {} of {}", i + 1, entry_count);
        });
    }

    if !any_lua_installed {
        update_job(&request_id, |j| {
            j.status = "failed".to_string();
            j.message = "No .lua files found in package".to_string();
            j.error = Some("NO_LUA_FILES".to_string());
        });
        return;
    }

    update_job(&request_id, |j| {
        j.status = "completed".to_string();
        j.progress = 100;
        j.message = format!("Installed {} Lua files and {} manifest files", lua_count, manifest_count);
        j.lua_count = lua_count;
        j.manifest_count = manifest_count;
        j.files = installed_files;
    });

    crate::log_to_temp(&format!(
        "[package] Completed {}: {} lua, {} manifests",
        app_id, lua_count, manifest_count
    ));
}

// ---------------------------------------------------------------------------
// Cleanup: remove old completed/failed jobs (called periodically or on-demand)
// ---------------------------------------------------------------------------
pub fn cleanup_old_jobs() {
    if let Ok(mut map) = get_downloads().write() {
        map.retain(|_, job| {
            matches!(job.status.as_str(), "queued" | "downloading" | "extracting" | "processing")
        });
    }
}
