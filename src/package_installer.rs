use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock};

// =========================================================================
// Section 1: Installed Package Metadata Types
// =========================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstalledFileInfo {
    pub filename: String,
    pub kind: String,
    pub sha256: String,
    pub size: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderInstallRecord {
    pub app_id: String,
    pub provider_id: String,
    pub version_known: bool,
    pub version_source: String,
    pub remote_modified: Option<String>,
    pub remote_modified_unix: Option<i64>,
    pub installed_at: String,
    pub lua_filename: String,
    pub files: Vec<InstalledFileInfo>,
    pub externally_modified: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageEntry {
    pub active_provider_id: String,
    pub providers: HashMap<String, ProviderInstallRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstalledPackagesDB {
    pub schema_version: u32,
    pub packages: HashMap<String, PackageEntry>,
}

impl Default for InstalledPackagesDB {
    fn default() -> Self {
        Self {
            schema_version: 1,
            packages: HashMap::new(),
        }
    }
}

struct ProviderStatus {
    file_modified: String,
    file_modified_unix: i64,
    file_size: u64,
}

// =========================================================================
// Section 2: Metadata Path, Cache, Load and Atomic Save
// =========================================================================

fn metadata_path() -> PathBuf {
    if let Ok(lad) = std::env::var("LOCALAPPDATA") {
        PathBuf::from(lad)
            .join("LumaForge")
            .join("installed_packages.json")
    } else {
        PathBuf::from("C:\\Windows\\Temp\\lumaforge_installed_packages.json")
    }
}

static METADATA_CACHE: OnceLock<RwLock<InstalledPackagesDB>> = OnceLock::new();

fn get_metadata_cache() -> &'static RwLock<InstalledPackagesDB> {
    METADATA_CACHE.get_or_init(|| {
        let db = load_metadata_from_disk();
        RwLock::new(db)
    })
}

fn load_metadata_from_disk() -> InstalledPackagesDB {
    load_metadata_from_path(&metadata_path())
}

fn load_metadata_from_path(path: &Path) -> InstalledPackagesDB {
    match std::fs::read_to_string(path) {
        Ok(raw) => match serde_json::from_str::<InstalledPackagesDB>(&raw) {
            Ok(db) => {
                crate::log_to_temp(&format!(
                    "[metadata] Loaded {} packages from disk",
                    db.packages.len()
                ));
                db
            }
            Err(e) => {
                crate::log_to_temp(&format!(
                    "[metadata] Corrupt JSON at {}: {}",
                    path.display(),
                    e
                ));
                let ts = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                let backup = path.with_file_name(format!("installed_packages.corrupt.{}.json", ts));
                if let Err(be) = std::fs::copy(&path, &backup) {
                    crate::log_to_temp(&format!("[metadata] Failed to create backup: {}", be));
                } else {
                    crate::log_to_temp(&format!(
                        "[metadata] Corrupt file backed up to {}",
                        backup.display()
                    ));
                }
                InstalledPackagesDB::default()
            }
        },
        Err(e) => {
            if e.kind() == std::io::ErrorKind::NotFound {
                crate::log_to_temp("[metadata] No installed_packages.json, starting fresh");
            } else {
                crate::log_to_temp(&format!(
                    "[metadata] Failed to read {}: {}",
                    path.display(),
                    e
                ));
            }
            InstalledPackagesDB::default()
        }
    }
}

fn save_metadata_to_disk(db: &InstalledPackagesDB) -> Result<(), String> {
    save_metadata_to_path(db, &metadata_path())
}

fn save_metadata_to_path(db: &InstalledPackagesDB, path: &Path) -> Result<(), String> {
    let json = serde_json::to_string_pretty(db).map_err(|e| format!("Serialize error: {}", e))?;

    let dir = path
        .parent()
        .unwrap_or_else(|| Path::new("C:\\Windows\\Temp"));
    let temp_name = format!(
        ".installed_packages.{}.tmp",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    let temp_path = dir.join(&temp_name);

    std::fs::write(&temp_path, &json).map_err(|e| format!("Write temp file error: {}", e))?;

    std::fs::rename(&temp_path, path).map_err(|e| {
        let _ = std::fs::remove_file(&temp_path);
        format!("Rename/replace error: {}", e)
    })?;

    crate::log_to_temp("[metadata] Metadata saved atomically");
    Ok(())
}

fn read_metadata() -> InstalledPackagesDB {
    get_metadata_cache()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

fn write_metadata_record(record: ProviderInstallRecord) -> Result<(), String> {
    let mut db = get_metadata_cache()
        .write()
        .unwrap_or_else(|e| e.into_inner());
    let app_id = record.app_id.clone();
    let provider_id = record.provider_id.clone();

    let entry = db
        .packages
        .entry(app_id.clone())
        .or_insert_with(|| PackageEntry {
            active_provider_id: provider_id.clone(),
            providers: HashMap::new(),
        });
    entry.active_provider_id = provider_id.clone();
    entry.providers.insert(provider_id.clone(), record);

    save_metadata_to_disk(&db)
}

fn write_metadata_migration(record: ProviderInstallRecord) -> Result<bool, String> {
    let mut db = get_metadata_cache()
        .write()
        .unwrap_or_else(|e| e.into_inner());
    let app_id = record.app_id.clone();
    let provider_id = record.provider_id.clone();

    if let Some(entry) = db.packages.get(&app_id) {
        if let Some(existing) = entry.providers.get(&provider_id) {
            if existing.version_known {
                crate::log_to_temp(&format!(
                    "[metadata] Migration rejected for {} / {}: trusted provider metadata already exists",
                    app_id, provider_id
                ));
                return Ok(false);
            }
        }
    }

    let entry = db
        .packages
        .entry(app_id.clone())
        .or_insert_with(|| PackageEntry {
            active_provider_id: provider_id.clone(),
            providers: HashMap::new(),
        });
    entry.active_provider_id = provider_id.clone();
    entry.providers.insert(provider_id.clone(), record);

    save_metadata_to_disk(&db)?;
    Ok(true)
}

fn get_lua_path_for_package(app_id: &str) -> String {
    let steam = detect_steam_root();
    format!("{}\\config\\lua\\{}.lua", steam, app_id)
}

// =========================================================================
// Section 3: File Hashing
// =========================================================================

fn compute_sha256(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    format!("{:x}", hasher.finalize())
}

fn compute_file_sha256(path: &str) -> Option<String> {
    std::fs::read(path).ok().map(|bytes| compute_sha256(&bytes))
}

// =========================================================================
// Section 4: Provider Status Query and Timestamp Parsing
// =========================================================================

fn parse_iso_timestamp_rust(ts: &str) -> i64 {
    let normalized = ts
        .replace("T", " ")
        .trim_end_matches("Z")
        .trim()
        .to_string();

    let re = match regex::Regex::new(r"(\d{4})-(\d{1,2})-(\d{1,2})\s+(\d{1,2}):(\d{1,2}):(\d{2})") {
        Ok(r) => r,
        Err(_) => return 0,
    };

    let caps = match re.captures(&normalized) {
        Some(c) => c,
        None => return 0,
    };

    let year: i64 = caps
        .get(1)
        .and_then(|m| m.as_str().parse().ok())
        .unwrap_or(0);
    let month: i64 = caps
        .get(2)
        .and_then(|m| m.as_str().parse().ok())
        .unwrap_or(0);
    let day: i64 = caps
        .get(3)
        .and_then(|m| m.as_str().parse().ok())
        .unwrap_or(0);
    let hour: i64 = caps
        .get(4)
        .and_then(|m| m.as_str().parse().ok())
        .unwrap_or(0);
    let min: i64 = caps
        .get(5)
        .and_then(|m| m.as_str().parse().ok())
        .unwrap_or(0);
    let sec: i64 = caps
        .get(6)
        .and_then(|m| m.as_str().parse().ok())
        .unwrap_or(0);

    if month < 1 || month > 12 || day < 1 || day > 31 {
        return 0;
    }
    if hour > 23 || min > 59 || sec > 59 {
        return 0;
    }
    if year < 2000 || year > 2100 {
        return 0;
    }

    ymdhms_to_unix_utc(year, month, day, hour, min, sec)
}

fn ymdhms_to_unix_utc(year: i64, month: i64, day: i64, hour: i64, min: i64, sec: i64) -> i64 {
    let m = month;
    let y = year - if m <= 2 { 1 } else { 0 };
    let era = y / 400;
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    days * 86400 + hour * 3600 + min * 60 + sec
}

pub fn parse_lua_header_timestamp(raw: &str) -> Option<(i64, i64, i64, i64, i64, i64, i64)> {
    let re = regex::Regex::new(
        r"(\w+)\s+(\d{1,2}),\s+(\d{4})\s+at\s+(\d{1,2}):(\d{2}):(\d{2})\s+(E[DS]T)",
    )
    .ok()?;
    let caps = re.captures(raw)?;

    let month_name = caps.get(1)?.as_str();
    let day: i64 = caps.get(2).and_then(|m| m.as_str().parse().ok())?;
    let year: i64 = caps.get(3).and_then(|m| m.as_str().parse().ok())?;
    let hour: i64 = caps.get(4).and_then(|m| m.as_str().parse().ok())?;
    let min: i64 = caps.get(5).and_then(|m| m.as_str().parse().ok())?;
    let sec: i64 = caps.get(6).and_then(|m| m.as_str().parse().ok())?;
    let tz = caps.get(7)?.as_str();

    let month = match month_name {
        "January" => 1,
        "February" => 2,
        "March" => 3,
        "April" => 4,
        "May" => 5,
        "June" => 6,
        "July" => 7,
        "August" => 8,
        "September" => 9,
        "October" => 10,
        "November" => 11,
        "December" => 12,
        _ => return None,
    };

    let tz_offset_secs = match tz {
        "EDT" => -4 * 3600,
        "EST" => -5 * 3600,
        _ => return None,
    };

    if month < 1 || month > 12 || day < 1 || day > 31 {
        return None;
    }
    if hour > 23 || min > 59 || sec > 59 {
        return None;
    }
    if year < 2000 || year > 2100 {
        return None;
    }

    Some((year, month, day, hour, min, sec, tz_offset_secs))
}

pub fn lua_header_to_unix(
    year: i64,
    month: i64,
    day: i64,
    hour: i64,
    min: i64,
    sec: i64,
    tz_offset_secs: i64,
) -> i64 {
    let base_utc = ymdhms_to_unix_utc(year, month, day, hour, min, sec);
    base_utc - tz_offset_secs
}

fn query_provider_status(provider: &Provider, app_id: &str) -> Option<ProviderStatus> {
    if provider.id != "hubcapdb" {
        return None;
    }
    if !provider.has_api_key() {
        return None;
    }

    let status_url = format!("{}/api/v1/status/{}", provider.base_url, app_id);
    let headers = provider.build_headers();

    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .danger_accept_invalid_certs(true)
        .build()
        .ok()?;

    let mut req = client.get(&status_url);
    for (k, v) in &headers {
        req = req.header(k.as_str(), v.as_str());
    }

    let response = req.send().ok()?;
    if !response.status().is_success() {
        return None;
    }

    let body = response.text().ok()?;
    let parsed: Value = serde_json::from_str(&body).ok()?;

    let status_str = parsed.get("status")?.as_str()?;
    if status_str != "available" {
        return None;
    }

    let file_modified = parsed.get("file_modified")?.as_str()?.to_string();
    let file_size = parsed.get("file_size")?.as_u64()?;

    let file_modified_unix = parse_iso_timestamp_rust(&file_modified);
    if file_modified_unix <= 0 {
        crate::log_to_temp(&format!(
            "[metadata] Failed to parse provider timestamp: {}",
            file_modified
        ));
        return None;
    }

    Some(ProviderStatus {
        file_modified,
        file_modified_unix,
        file_size,
    })
}

fn validate_remote_hint(
    app_id: &str,
    _provider_id: &str,
    remote_modified: &str,
    remote_modified_unix: i64,
) -> bool {
    if remote_modified.is_empty() || remote_modified_unix <= 0 {
        crate::log_to_temp(&format!(
            "[metadata] Hint rejected for {}: empty or zero timestamps",
            app_id
        ));
        return false;
    }

    let parsed_unix = parse_iso_timestamp_rust(remote_modified);
    if parsed_unix <= 0 {
        crate::log_to_temp(&format!(
            "[metadata] Hint rejected for {}: ISO timestamp did not parse",
            app_id
        ));
        return false;
    }

    if (parsed_unix - remote_modified_unix).abs() > 2 {
        crate::log_to_temp(&format!(
            "[metadata] Hint rejected for {}: ISO={}, parsed_unix={}, hint_unix={} (drift > 2s)",
            app_id, remote_modified, parsed_unix, remote_modified_unix
        ));
        return false;
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    if remote_modified_unix > now + 365 * 86400 {
        crate::log_to_temp(&format!(
            "[metadata] Hint rejected for {}: timestamp >1 year in future",
            app_id
        ));
        return false;
    }

    if remote_modified_unix < now - 20 * 365 * 86400 {
        crate::log_to_temp(&format!(
            "[metadata] Hint rejected for {}: timestamp >20 years in past",
            app_id
        ));
        return false;
    }

    true
}

// =========================================================================
// Section 5: Download Job State
// =========================================================================

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

struct RemoteHint {
    remote_modified: String,
    remote_modified_unix: i64,
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

// =========================================================================
// Section 6: Provider Resolution
// =========================================================================

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
    let raw_providers = dl
        .get("providers")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    raw_providers
        .into_iter()
        .filter(|p| p.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true))
        .map(|p| {
            let id = p
                .get("id")
                .or(p.get("name"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            Provider {
                id: id.clone(),
                name: p
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or(&id)
                    .to_string(),
                base_url: p
                    .get("baseUrl")
                    .or(p.get("base_url"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                api_key: p
                    .get("apiKey")
                    .or(p.get("api_key"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
            }
        })
        .collect()
}

fn resolve_provider(source_id: &str) -> Option<Provider> {
    let providers = load_providers_from_config();
    let lower = source_id.to_lowercase();
    providers
        .into_iter()
        .find(|p| p.id.to_lowercase() == lower || p.name.to_lowercase() == lower)
}

impl Provider {
    fn has_api_key(&self) -> bool {
        self.api_key
            .as_ref()
            .map(|k| !k.is_empty())
            .unwrap_or(false)
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

// =========================================================================
// Section 7: Route Handler
// =========================================================================

pub fn try_handle_route(method: &str, path: &str, body: &str) -> Option<(u16, String)> {
    if method == "POST" && path == "/api/package-metadata/migrate" {
        return Some(handle_metadata_migrate(body));
    }
    if method == "GET" && path == "/api/package-metadata/all" {
        return Some(handle_metadata_all());
    }
    if method == "GET" && path.starts_with("/api/package-metadata/") {
        let remainder = &path["/api/package-metadata/".len()..];
        if let Some(slash_pos) = remainder.find('/') {
            let provider_id = &remainder[..slash_pos];
            let app_id = &remainder[slash_pos + 1..];
            if !provider_id.is_empty() && !app_id.is_empty() {
                return Some(handle_metadata_provider_app(provider_id, app_id));
            }
        }
        if !remainder.is_empty() {
            return Some(handle_metadata_app(remainder));
        }
    }
    if method == "POST" && path == "/api/download" {
        return Some(handle_download_request(body));
    }
    if method == "GET" && path.starts_with("/api/download-status/") {
        let request_id = &path["/api/download-status/".len()..];
        return Some(handle_download_status(request_id));
    }
    None
}

// =========================================================================
// Section 8: Metadata Route Handlers
// =========================================================================

struct FileVerification {
    file_exists: bool,
    current_size: usize,
    externally_modified: bool,
}

fn verify_lua_file(record: &ProviderInstallRecord, steam: &str) -> FileVerification {
    let lua_path = format!("{}\\config\\lua\\{}", steam, record.lua_filename);
    let file_exists = Path::new(&lua_path).exists();
    if !file_exists {
        return FileVerification {
            file_exists: false,
            current_size: 0,
            externally_modified: false,
        };
    }
    let current_size = std::fs::metadata(&lua_path)
        .map(|m| m.len() as usize)
        .unwrap_or(0);
    let stored_lua_files: Vec<&InstalledFileInfo> =
        record.files.iter().filter(|f| f.kind == "lua").collect();
    if stored_lua_files.is_empty() {
        return FileVerification {
            file_exists: true,
            current_size,
            externally_modified: false,
        };
    }
    let stored_lua_size: usize = stored_lua_files.iter().map(|f| f.size).sum();
    if current_size != stored_lua_size {
        return FileVerification {
            file_exists: true,
            current_size,
            externally_modified: true,
        };
    }
    let current_hash = compute_file_sha256(&lua_path);
    let hash_mismatch = current_hash
        .as_ref()
        .map(|h| !stored_lua_files.iter().any(|f| f.sha256 == *h))
        .unwrap_or(false);
    FileVerification {
        file_exists: true,
        current_size,
        externally_modified: hash_mismatch,
    }
}

fn verify_all_files(record: &ProviderInstallRecord, steam: &str) -> Vec<serde_json::Value> {
    let manifest_dir = format!("{}\\depotcache", steam);
    let lua_dir = format!("{}\\config\\lua", steam);
    record
        .files
        .iter()
        .map(|fi| {
            let full_path = if fi.kind == "lua" {
                format!("{}\\{}", lua_dir, fi.filename)
            } else {
                format!("{}\\{}", manifest_dir, fi.filename)
            };
            let exists = Path::new(&full_path).exists();
            let ok = if exists {
                compute_file_sha256(&full_path)
                    .map(|h| h == fi.sha256)
                    .unwrap_or(false)
            } else {
                false
            };
            json!({
                "filename": fi.filename,
                "kind": fi.kind,
                "exists": exists,
                "sha256Valid": ok,
                "expectedSha256": fi.sha256,
            })
        })
        .collect()
}

fn handle_metadata_all() -> (u16, String) {
    let db = read_metadata();
    let mut packages = serde_json::Map::new();
    let steam = detect_steam_root();

    for (app_id, entry) in &db.packages {
        let mut providers = serde_json::Map::new();

        for (provider_id, record) in &entry.providers {
            let fv = verify_lua_file(record, &steam);
            let mut rec_val = serde_json::to_value(record).unwrap_or_default();
            if let Some(obj) = rec_val.as_object_mut() {
                obj.insert("fileExists".into(), json!(fv.file_exists));
                obj.insert("currentLuaFileSize".into(), json!(fv.current_size));
                obj.insert("externallyModified".into(), json!(fv.externally_modified));
            }
            providers.insert(provider_id.clone(), rec_val);
        }

        packages.insert(
            app_id.clone(),
            json!({
                "activeProviderId": entry.active_provider_id,
                "providers": providers,
            }),
        );
    }

    let resp = json!({
        "ok": true,
        "schemaVersion": db.schema_version,
        "packages": packages,
    });
    (200, resp.to_string())
}

fn handle_metadata_app(app_id: &str) -> (u16, String) {
    let db = read_metadata();
    match db.packages.get(app_id) {
        Some(entry) => {
            let steam = detect_steam_root();
            let record = entry.providers.values().next().unwrap();
            let fv = verify_lua_file(record, &steam);

            (
                200,
                json!({
                    "ok": true,
                    "metadata": entry,
                    "fileExists": fv.file_exists,
                    "currentLuaFileSize": fv.current_size,
                    "externallyModified": fv.externally_modified,
                })
                .to_string(),
            )
        }
        None => (
            200,
            json!({
                "ok": true,
                "metadata": null,
                "fileExists": false,
                "externallyModified": false,
            })
            .to_string(),
        ),
    }
}

fn handle_metadata_provider_app(provider_id: &str, app_id: &str) -> (u16, String) {
    let db = read_metadata();
    match db.packages.get(app_id) {
        Some(entry) => match entry.providers.get(provider_id) {
            Some(record) => {
                let steam = detect_steam_root();
                let fv = verify_lua_file(record, &steam);
                let file_checks = verify_all_files(record, &steam);
                let all_valid = file_checks.iter().all(|c| {
                    c.get("exists").and_then(|v| v.as_bool()).unwrap_or(false)
                        && c.get("sha256Valid")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false)
                });

                (
                    200,
                    json!({
                        "ok": true,
                        "metadata": record,
                        "fileExists": fv.file_exists,
                        "currentLuaFileSize": fv.current_size,
                        "externallyModified": fv.externally_modified,
                        "fileChecks": file_checks,
                        "allFilesValid": all_valid,
                    })
                    .to_string(),
                )
            }
            None => (
                200,
                json!({
                    "ok": true,
                    "metadata": null,
                    "fileExists": false,
                    "externallyModified": false,
                })
                .to_string(),
            ),
        },
        None => (
            200,
            json!({
                "ok": true,
                "metadata": null,
                "fileExists": false,
                "externallyModified": false,
            })
            .to_string(),
        ),
    }
}

#[derive(Deserialize)]
struct MigrateRequest {
    #[serde(rename = "appId")]
    app_id: String,
    #[serde(rename = "providerId")]
    provider_id: String,
    #[serde(rename = "headerRaw")]
    header_raw: String,
    #[serde(rename = "luaFilename")]
    lua_filename: String,
}

fn handle_metadata_migrate(body: &str) -> (u16, String) {
    let req: MigrateRequest = match serde_json::from_str(body) {
        Ok(r) => r,
        Err(e) => {
            return (
                400,
                json!({"ok": false, "message": format!("Invalid request: {}", e)}).to_string(),
            );
        }
    };

    if req.app_id.is_empty() || req.provider_id.is_empty() {
        return (
            400,
            json!({"ok": false, "message": "Missing appId or providerId"}).to_string(),
        );
    }

    let parsed = match parse_lua_header_timestamp(&req.header_raw) {
        Some(p) => p,
        None => {
            return (
                400,
                json!({"ok": false, "message": format!("Could not parse header timestamp: {}", req.header_raw)}).to_string(),
            );
        }
    };

    let (year, month, day, hour, min, sec, tz_offset) = parsed;
    let unix_ts = lua_header_to_unix(year, month, day, hour, min, sec, tz_offset);

    if unix_ts <= 0 {
        return (
            400,
            json!({"ok": false, "message": "Parsed timestamp is not valid"}).to_string(),
        );
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    let iso_formatted = format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        year, month, day, hour, min, sec
    );

    let record = ProviderInstallRecord {
        app_id: req.app_id.clone(),
        provider_id: req.provider_id.clone(),
        version_known: true,
        version_source: "lua_header_migration".to_string(),
        remote_modified: Some(iso_formatted),
        remote_modified_unix: Some(unix_ts),
        installed_at: format_timestamp_utc(now),
        lua_filename: req.lua_filename,
        files: Vec::new(),
        externally_modified: false,
    };

    match write_metadata_migration(record) {
        Ok(true) => {
            crate::log_to_temp(&format!(
                "[metadata] Header migration succeeded for {} / {}: unix={}",
                req.app_id, req.provider_id, unix_ts
            ));
            let db = read_metadata();
            if let Some(entry) = db.packages.get(&req.app_id) {
                if let Some(rec) = entry.providers.get(&req.provider_id) {
                    return (
                        200,
                        json!({
                            "ok": true,
                            "metadata": rec,
                            "migrated": true,
                        })
                        .to_string(),
                    );
                }
            }
            (200, json!({"ok": true, "migrated": true}).to_string())
        }
        Ok(false) => (
            200,
            json!({"ok": true, "migrated": false, "message": "Trusted metadata already exists"})
                .to_string(),
        ),
        Err(e) => (
            500,
            json!({"ok": false, "message": format!("Migration save failed: {}", e)}).to_string(),
        ),
    }
}

fn format_timestamp_utc(secs: i64) -> String {
    let days = secs / 86400;
    let time_of_day = secs % 86400;
    let h = time_of_day / 3600;
    let m = (time_of_day % 3600) / 60;
    let s = time_of_day % 60;

    let mut y = 1970i64;
    let mut remaining = days;
    loop {
        let days_in_year = if is_leap(y) { 366 } else { 365 };
        if remaining < days_in_year {
            break;
        }
        remaining -= days_in_year;
        y += 1;
    }

    let leap = is_leap(y);
    let month_days: [i64; 12] = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];

    let mut mon = 1u64;
    let mut day_in_month = remaining + 1;
    for &days_in_month in &month_days {
        if day_in_month <= days_in_month {
            break;
        }
        day_in_month -= days_in_month;
        mon += 1;
    }

    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        y, mon, day_in_month, h, m, s
    )
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

// =========================================================================
// Section 9: Download Request Handler
// =========================================================================

fn handle_download_request(body: &str) -> (u16, String) {
    let parsed: Value = serde_json::from_str(body).unwrap_or_default();

    let app_id = parsed
        .get("appId")
        .or(parsed.get("app_id"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let source_id = parsed
        .get("sourceId")
        .or(parsed.get("source_id"))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if app_id.is_empty() {
        return (
            400,
            json!({"ok": false, "message": "Missing appId"}).to_string(),
        );
    }
    if source_id.is_empty() {
        return (
            400,
            json!({"ok": false, "message": "Missing sourceId"}).to_string(),
        );
    }

    let provider = match resolve_provider(source_id) {
        Some(p) => p,
        None => {
            return (
                400,
                json!({"ok": false, "message": format!("Provider not found: {}", source_id)})
                    .to_string(),
            );
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

    let hint = extract_remote_hint(&parsed, app_id, &provider.id);

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
    get_downloads()
        .write()
        .unwrap()
        .insert(request_id.clone(), job);

    let rid = request_id.clone();
    let _app_id = app_id.to_string();
    let _source_id = source_id.to_string();
    std::thread::spawn(move || download_and_install(rid, provider, hint));

    crate::log_to_temp(&format!(
        "[package] Download accepted for {} via {}",
        app_id, source_id
    ));
    (
        200,
        json!({"ok": true, "requestId": request_id}).to_string(),
    )
}

fn extract_remote_hint(
    parsed: &Value,
    request_app_id: &str,
    request_provider_id: &str,
) -> Option<RemoteHint> {
    let rm = parsed.get("remoteModified").and_then(|v| v.as_str())?;
    let rmu = parsed.get("remoteModifiedUnix").and_then(|v| v.as_i64())?;

    if !validate_remote_hint(request_app_id, request_provider_id, rm, rmu) {
        return None;
    }

    Some(RemoteHint {
        remote_modified: rm.to_string(),
        remote_modified_unix: rmu,
    })
}

// =========================================================================
// Section 10: Download Status Handler
// =========================================================================

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
        None => (
            200,
            json!({
                "ok": false,
                "status": "failed",
                "progress": 0,
                "message": "Download not found",
                "errorCode": "NOT_FOUND"
            })
            .to_string(),
        ),
    }
}

// =========================================================================
// Section 11: Background Download Thread — Binary-Safe ZIP Extraction
// =========================================================================

fn download_and_install(request_id: String, provider: Provider, hint: Option<RemoteHint>) {
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

    crate::log_to_temp(&format!(
        "[package] Downloaded {} bytes for {}",
        bytes.len(),
        app_id
    ));

    update_job(&request_id, |j| {
        j.progress = 50;
        j.message = "Extracting package".to_string();
        j.status = "extracting".to_string();
    });

    if bytes.len() < 4 || bytes[0] != 0x50 || bytes[1] != 0x4b {
        update_job(&request_id, |j| {
            j.status = "failed".to_string();
            j.message = "Response is not a valid ZIP package".to_string();
            j.error = Some("NOT_ZIP".to_string());
        });
        return;
    }

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

    let _ = std::fs::create_dir_all(&lua_dir);
    let _ = std::fs::create_dir_all(&manifest_dir);

    let entry_count = archive.len();
    let mut lua_count: u32 = 0;
    let mut manifest_count: u32 = 0;
    let mut installed_raw_files: Vec<InstalledFile> = Vec::new();
    let mut installed_meta_files: Vec<InstalledFileInfo> = Vec::new();
    let mut any_lua_installed = false;
    let mut lua_filename = String::new();

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
            crate::log_to_temp(&format!(
                "[package] Skipping non-installable: {}",
                entry_name
            ));
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
                let sha = compute_sha256(&file_bytes);
                let ftype = if ext == "lua" { "lua" } else { "manifest" };

                installed_raw_files.push(InstalledFile {
                    filename: basename.clone(),
                    file_type: ftype.to_string(),
                    size: file_bytes.len(),
                });
                installed_meta_files.push(InstalledFileInfo {
                    filename: basename.clone(),
                    kind: ftype.to_string(),
                    sha256: sha,
                    size: file_bytes.len(),
                });

                if ext == "lua" {
                    lua_count += 1;
                    any_lua_installed = true;
                    if lua_filename.is_empty() {
                        lua_filename = basename.clone();
                    }
                } else {
                    manifest_count += 1;
                }
                crate::log_to_temp(&format!(
                    "[package] Installed {} ({} bytes) -> {}",
                    basename,
                    file_bytes.len(),
                    dest
                ));
            }
            Err(e) => {
                crate::log_to_temp(&format!("[package] Failed to write {}: {}", dest, e));
            }
        }

        let pct = 50 + ((i as u8 + 1) * 50 / (entry_count as u8).max(1));
        update_job(&request_id, |j| {
            j.progress = pct.min(85);
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

    // --- Step 5: Verify installed files exist ---
    update_job(&request_id, |j| {
        j.progress = 86;
        j.message = "Verifying installed files".to_string();
        j.status = "processing".to_string();
    });

    for fi in &installed_meta_files {
        let full_path = if fi.kind == "lua" {
            format!("{}\\{}", lua_dir, fi.filename)
        } else {
            format!("{}\\{}", manifest_dir, fi.filename)
        };
        if !Path::new(&full_path).exists() {
            update_job(&request_id, |j| {
                j.status = "failed".to_string();
                j.message = format!("Installed file not found: {}", full_path);
                j.error = Some("FILE_VERIFICATION_FAILED".to_string());
            });
            return;
        }
    }

    // --- Step 6: Recompute hashes from final installed files ---
    update_job(&request_id, |j| {
        j.progress = 90;
        j.message = "Computing file hashes".to_string();
    });

    for fi in &mut installed_meta_files {
        let full_path = if fi.kind == "lua" {
            format!("{}\\{}", lua_dir, fi.filename)
        } else {
            format!("{}\\{}", manifest_dir, fi.filename)
        };
        if let Some(hash) = compute_file_sha256(&full_path) {
            fi.sha256 = hash;
        }
    }

    // --- Step 7: Resolve provider status for remote version ---
    update_job(&request_id, |j| {
        j.progress = 92;
        j.message = "Recording installation metadata".to_string();
    });

    let (version_known, version_source, remote_modified_str, remote_modified_unix) =
        if let Some(ref h) = hint {
            crate::log_to_temp(&format!(
                "[metadata] {} using validated hint: {} ({})",
                app_id, h.remote_modified, h.remote_modified_unix
            ));
            (
                true,
                "provider_status_hint".to_string(),
                Some(h.remote_modified.clone()),
                Some(h.remote_modified_unix),
            )
        } else {
            match query_provider_status(&provider, &app_id) {
                Some(ps) => {
                    crate::log_to_temp(&format!(
                        "[metadata] {} provider status: {} ({})",
                        app_id, ps.file_modified, ps.file_modified_unix
                    ));
                    (
                        true,
                        "provider_status".to_string(),
                        Some(ps.file_modified),
                        Some(ps.file_modified_unix),
                    )
                }
                None => {
                    crate::log_to_temp(&format!(
                        "[metadata] {} provider status unavailable, version_known=false",
                        app_id
                    ));
                    (false, "unavailable".to_string(), None, None)
                }
            }
        };

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    let record = ProviderInstallRecord {
        app_id: app_id.clone(),
        provider_id: provider.id.clone(),
        version_known,
        version_source,
        remote_modified: remote_modified_str,
        remote_modified_unix,
        installed_at: format_timestamp_utc(now),
        lua_filename: lua_filename.clone(),
        files: installed_meta_files,
        externally_modified: false,
    };

    // --- Step 8: Persist metadata atomically ---
    if let Err(e) = write_metadata_record(record) {
        crate::log_to_temp(&format!(
            "[metadata] FAILED to persist metadata for {}: {}",
            app_id, e
        ));
        update_job(&request_id, |j| {
            j.status = "failed".to_string();
            j.message = format!("Metadata persistence failed: {}", e);
            j.error = Some("METADATA_PERSISTENCE_FAILED".to_string());
        });
        return;
    }

    crate::log_to_temp(&format!(
        "[metadata] {} installed metadata persisted (version_known={})",
        app_id, version_known
    ));

    // --- Step 9: Mark completed ---
    update_job(&request_id, |j| {
        j.status = "completed".to_string();
        j.progress = 100;
        j.message = format!(
            "Installed {} Lua files and {} manifest files",
            lua_count, manifest_count
        );
        j.lua_count = lua_count;
        j.manifest_count = manifest_count;
        j.files = installed_raw_files;
    });

    crate::log_to_temp(&format!(
        "[package] Completed {}: {} lua, {} manifests",
        app_id, lua_count, manifest_count
    ));
}

// =========================================================================
// Section 12: Cleanup
// =========================================================================

pub fn cleanup_old_jobs() {
    if let Ok(mut map) = get_downloads().write() {
        map.retain(|_, job| {
            matches!(
                job.status.as_str(),
                "queued" | "downloading" | "extracting" | "processing"
            )
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn test_dir(name: &str) -> PathBuf {
        let d = PathBuf::from(std::env::temp_dir()).join(format!("lumaforge_test_{}", name));
        let _ = fs::remove_dir_all(&d);
        let _ = fs::create_dir_all(&d);
        d
    }

    fn cleanup(d: &Path) {
        let _ = fs::remove_dir_all(d);
    }

    fn make_lua_file(dir: &Path, name: &str, content: &[u8]) -> PathBuf {
        let p = dir.join(name);
        fs::write(&p, content).unwrap();
        p
    }

    fn make_record(
        app_id: &str,
        provider_id: &str,
        lua_filename: &str,
        sha256: &str,
        size: usize,
    ) -> ProviderInstallRecord {
        ProviderInstallRecord {
            app_id: app_id.to_string(),
            provider_id: provider_id.to_string(),
            version_known: true,
            version_source: "provider_status".to_string(),
            remote_modified: Some("2026-05-22T09:30:00".to_string()),
            remote_modified_unix: Some(1779442200),
            installed_at: "2026-07-26T06:30:00Z".to_string(),
            lua_filename: lua_filename.to_string(),
            files: vec![InstalledFileInfo {
                filename: lua_filename.to_string(),
                kind: "lua".to_string(),
                sha256: sha256.to_string(),
                size,
            }],
            externally_modified: false,
        }
    }

    // ========== SHA-256 Hashing ==========

    #[test]
    fn test_sha256_empty() {
        let hash = compute_sha256(b"");
        assert_eq!(
            hash,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn test_sha256_known_value() {
        let hash = compute_sha256(b"hello world");
        assert_eq!(
            hash,
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
    }

    #[test]
    fn test_compute_file_sha256() {
        let d = test_dir("sha256");
        let p = make_lua_file(
            &d,
            "test.lua",
            b"-- Created: May 22, 2026 at 09:30:00 EDT\nreturn {}\n",
        );
        let hash = compute_file_sha256(&p.to_string_lossy());
        assert!(hash.is_some());
        let h = hash.unwrap();
        assert_eq!(h.len(), 64);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
        cleanup(&d);
    }

    #[test]
    fn test_compute_file_sha256_missing() {
        let hash = compute_file_sha256("C:\\nonexistent_path_for_test_12345.lua");
        assert!(hash.is_none());
    }

    #[test]
    fn test_file_sha256_deterministic() {
        let d = test_dir("sha256_det");
        let content = b"return {version='1.0'}\n";
        let p = make_lua_file(&d, "det.lua", content);
        let h1 = compute_file_sha256(&p.to_string_lossy()).unwrap();
        let h2 = compute_file_sha256(&p.to_string_lossy()).unwrap();
        assert_eq!(h1, h2);
        assert_eq!(h1, compute_sha256(content));
        cleanup(&d);
    }

    // ========== ISO Timestamp Parsing ==========

    #[test]
    fn test_parse_iso_basic() {
        let ts = parse_iso_timestamp_rust("2026-05-22T09:30:00");
        assert!(ts > 0);
    }

    #[test]
    fn test_parse_iso_with_z() {
        let ts = parse_iso_timestamp_rust("2026-05-22T09:30:00Z");
        assert!(ts > 0);
    }

    #[test]
    fn test_parse_iso_space_separated() {
        let ts = parse_iso_timestamp_rust("2026-05-22 09:30:00");
        assert!(ts > 0);
    }

    #[test]
    fn test_parse_iso_invalid_format() {
        let ts = parse_iso_timestamp_rust("not-a-timestamp");
        assert_eq!(ts, 0);
    }

    #[test]
    fn test_parse_iso_empty() {
        let ts = parse_iso_timestamp_rust("");
        assert_eq!(ts, 0);
    }

    #[test]
    fn test_parse_iso_out_of_range_month() {
        let ts = parse_iso_timestamp_rust("2026-13-01T00:00:00");
        assert_eq!(ts, 0);
    }

    #[test]
    fn test_parse_iso_out_of_range_year() {
        let ts = parse_iso_timestamp_rust("1999-01-01T00:00:00");
        assert_eq!(ts, 0);
    }

    #[test]
    fn test_ymdhms_to_unix_utc_epoch() {
        let ts = ymdhms_to_unix_utc(1970, 1, 1, 0, 0, 0);
        assert_eq!(ts, 0);
    }

    #[test]
    fn test_ymdhms_to_unix_utc_known_date() {
        let ts = ymdhms_to_unix_utc(2026, 1, 1, 0, 0, 0);
        assert!(ts > 0);
        let ts2 = ymdhms_to_unix_utc(2026, 1, 2, 0, 0, 0);
        assert_eq!(ts2 - ts, 86400);
    }

    // ========== Lua Header Timestamp Parsing ==========

    #[test]
    fn test_parse_lua_header_edt() {
        let result = parse_lua_header_timestamp("-- Created: May 22, 2026 at 09:30:00 EDT");
        assert!(result.is_some());
        let (year, month, day, hour, min, sec, tz) = result.unwrap();
        assert_eq!(year, 2026);
        assert_eq!(month, 5);
        assert_eq!(day, 22);
        assert_eq!(hour, 9);
        assert_eq!(min, 30);
        assert_eq!(sec, 0);
        assert_eq!(tz, -4 * 3600);
    }

    #[test]
    fn test_parse_lua_header_est() {
        let result = parse_lua_header_timestamp("-- Created: January 15, 2026 at 14:00:00 EST");
        assert!(result.is_some());
        let (year, month, day, hour, min, sec, tz) = result.unwrap();
        assert_eq!(year, 2026);
        assert_eq!(month, 1);
        assert_eq!(day, 15);
        assert_eq!(hour, 14);
        assert_eq!(min, 0);
        assert_eq!(sec, 0);
        assert_eq!(tz, -5 * 3600);
    }

    #[test]
    fn test_parse_lua_header_invalid() {
        let result = parse_lua_header_timestamp("-- Created: blah blah");
        assert!(result.is_none());
    }

    #[test]
    fn test_parse_lua_header_no_tz() {
        let result = parse_lua_header_timestamp("-- Created: May 22, 2026 at 09:30:00 UTC");
        assert!(result.is_none());
    }

    #[test]
    fn test_lua_header_to_unix_edt() {
        let ts = lua_header_to_unix(2026, 5, 22, 9, 30, 0, -4 * 3600);
        assert!(ts > 0);
        let utc_equivalent = ymdhms_to_unix_utc(2026, 5, 22, 13, 30, 0);
        assert_eq!(ts, utc_equivalent);
    }

    #[test]
    fn test_lua_header_to_unix_est() {
        let ts = lua_header_to_unix(2026, 1, 15, 14, 0, 0, -5 * 3600);
        assert!(ts > 0);
        let utc_equivalent = ymdhms_to_unix_utc(2026, 1, 15, 19, 0, 0);
        assert_eq!(ts, utc_equivalent);
    }

    #[test]
    fn test_edt_est_same_wall_clock_different_utc() {
        let edt = lua_header_to_unix(2026, 7, 1, 12, 0, 0, -4 * 3600);
        let est = lua_header_to_unix(2026, 7, 1, 12, 0, 0, -5 * 3600);
        assert_eq!(est - edt, 3600);
    }

    // ========== Remote Hint Validation ==========

    #[test]
    fn test_validate_hint_valid() {
        assert!(validate_remote_hint(
            "12345",
            "hubcapdb",
            "2026-05-22T09:30:00",
            1779442200
        ));
    }

    #[test]
    fn test_validate_hint_empty_string() {
        assert!(!validate_remote_hint("12345", "hubcapdb", "", 1779442200));
    }

    #[test]
    fn test_validate_hint_zero_unix() {
        assert!(!validate_remote_hint(
            "12345",
            "hubcapdb",
            "2026-05-22T09:30:00",
            0
        ));
    }

    #[test]
    fn test_validate_hint_negative_unix() {
        assert!(!validate_remote_hint(
            "12345",
            "hubcapdb",
            "2026-05-22T09:30:00",
            -100
        ));
    }

    #[test]
    fn test_validate_hint_iso_unparseable() {
        assert!(!validate_remote_hint(
            "12345",
            "hubcapdb",
            "not-a-date",
            1779442200
        ));
    }

    #[test]
    fn test_validate_hint_far_future() {
        let far_future = 2145916800; // year 2037+
        assert!(!validate_remote_hint(
            "12345",
            "hubcapdb",
            "2038-01-01T00:00:00",
            far_future
        ));
    }

    #[test]
    fn test_validate_hint_far_past() {
        let far_past = 0;
        assert!(!validate_remote_hint(
            "12345",
            "hubcapdb",
            "1970-01-01T00:00:00",
            far_past
        ));
    }

    #[test]
    fn test_validate_hint_drift_too_large() {
        let ts = parse_iso_timestamp_rust("2026-05-22T09:30:00");
        assert!(!validate_remote_hint(
            "12345",
            "hubcapdb",
            "2026-05-22T09:30:00",
            ts + 5
        ));
    }

    #[test]
    fn test_validate_hint_within_tolerance() {
        let ts = parse_iso_timestamp_rust("2026-05-22T09:30:00");
        assert!(validate_remote_hint(
            "12345",
            "hubcapdb",
            "2026-05-22T09:30:00",
            ts + 1
        ));
        assert!(validate_remote_hint(
            "12345",
            "hubcapdb",
            "2026-05-22T09:30:00",
            ts - 1
        ));
        assert!(validate_remote_hint(
            "12345",
            "hubcapdb",
            "2026-05-22T09:30:00",
            ts
        ));
    }

    // ========== File Verification with SHA-256 ==========

    #[test]
    fn test_verify_lua_file_matches() {
        let d = test_dir("verify_match");
        fs::create_dir_all(d.join("config\\lua")).unwrap();
        let content = b"return {version='1.0'}\n";
        let lua_path = d.join("config\\lua\\12345.lua");
        fs::write(&lua_path, content).unwrap();
        let sha = compute_sha256(content);
        let record = make_record("12345", "hubcapdb", "12345.lua", &sha, content.len());
        let fv = verify_lua_file(&record, &d.to_string_lossy().as_ref());
        assert!(fv.file_exists);
        assert!(!fv.externally_modified);
        assert_eq!(fv.current_size, content.len());
        cleanup(&d);
    }

    #[test]
    fn test_verify_lua_file_size_mismatch() {
        let d = test_dir("verify_size");
        fs::create_dir_all(d.join("config\\lua")).unwrap();
        let content = b"return {version='1.0'}\n";
        let lua_path = d.join("config\\lua\\12345.lua");
        fs::write(&lua_path, content).unwrap();
        let sha = compute_sha256(content);
        let record = make_record("12345", "hubcapdb", "12345.lua", &sha, 9999);
        let fv = verify_lua_file(&record, &d.to_string_lossy().as_ref());
        assert!(fv.file_exists);
        assert!(fv.externally_modified);
        cleanup(&d);
    }

    #[test]
    fn test_verify_lua_file_hash_mismatch_same_size() {
        let d = test_dir("verify_hash");
        fs::create_dir_all(d.join("config\\lua")).unwrap();
        let content = b"return {version='1.0'}\n";
        let lua_path = d.join("config\\lua\\12345.lua");
        fs::write(&lua_path, content).unwrap();
        let wrong_sha = compute_sha256(b"different content here!!\n");
        let record = make_record("12345", "hubcapdb", "12345.lua", &wrong_sha, content.len());
        let fv = verify_lua_file(&record, &d.to_string_lossy().as_ref());
        assert!(fv.file_exists);
        assert!(fv.externally_modified);
        assert_eq!(fv.current_size, content.len());
        cleanup(&d);
    }

    #[test]
    fn test_verify_lua_file_missing() {
        let d = test_dir("verify_miss");
        fs::create_dir_all(d.join("config\\lua")).unwrap();
        let sha = compute_sha256(b"content");
        let record = make_record("12345", "hubcapdb", "12345.lua", &sha, 7);
        let fv = verify_lua_file(&record, &d.to_string_lossy().as_ref());
        assert!(!fv.file_exists);
        assert!(!fv.externally_modified);
        cleanup(&d);
    }

    #[test]
    fn test_verify_all_files_full() {
        let d = test_dir("verify_all");
        let lua_dir = d.join("config\\lua");
        let manif_dir = d.join("depotcache");
        fs::create_dir_all(&lua_dir).unwrap();
        fs::create_dir_all(&manif_dir).unwrap();

        let lua_content = b"return {}\n";
        let manif_content = b"manifest data here";
        let lua_sha = compute_sha256(lua_content);
        let manif_sha = compute_sha256(manif_content);
        fs::write(lua_dir.join("12345.lua"), lua_content).unwrap();
        fs::write(manif_dir.join("12345_123456.manifest"), manif_content).unwrap();

        let record = ProviderInstallRecord {
            app_id: "12345".to_string(),
            provider_id: "hubcapdb".to_string(),
            version_known: true,
            version_source: "provider_status".to_string(),
            remote_modified: Some("2026-05-22T09:30:00".to_string()),
            remote_modified_unix: Some(1779442200),
            installed_at: "2026-07-26T06:30:00Z".to_string(),
            lua_filename: "12345.lua".to_string(),
            files: vec![
                InstalledFileInfo {
                    filename: "12345.lua".to_string(),
                    kind: "lua".to_string(),
                    sha256: lua_sha,
                    size: lua_content.len(),
                },
                InstalledFileInfo {
                    filename: "12345_123456.manifest".to_string(),
                    kind: "manifest".to_string(),
                    sha256: manif_sha,
                    size: manif_content.len(),
                },
            ],
            externally_modified: false,
        };

        let checks = verify_all_files(&record, &d.to_string_lossy().as_ref());
        assert_eq!(checks.len(), 2);
        for check in &checks {
            assert!(check.get("exists").unwrap().as_bool().unwrap());
            assert!(check.get("sha256Valid").unwrap().as_bool().unwrap());
        }
        cleanup(&d);
    }

    #[test]
    fn test_verify_all_files_manifest_corrupted() {
        let d = test_dir("verify_corrupt");
        let lua_dir = d.join("config\\lua");
        let manif_dir = d.join("depotcache");
        fs::create_dir_all(&lua_dir).unwrap();
        fs::create_dir_all(&manif_dir).unwrap();

        let lua_content = b"return {}\n";
        let lua_sha = compute_sha256(lua_content);
        fs::write(lua_dir.join("12345.lua"), lua_content).unwrap();
        fs::write(manif_dir.join("12345_123456.manifest"), b"corrupted").unwrap();

        let record = ProviderInstallRecord {
            app_id: "12345".to_string(),
            provider_id: "hubcapdb".to_string(),
            version_known: true,
            version_source: "provider_status".to_string(),
            remote_modified: Some("2026-05-22T09:30:00".to_string()),
            remote_modified_unix: Some(1779442200),
            installed_at: "2026-07-26T06:30:00Z".to_string(),
            lua_filename: "12345.lua".to_string(),
            files: vec![
                InstalledFileInfo {
                    filename: "12345.lua".to_string(),
                    kind: "lua".to_string(),
                    sha256: lua_sha,
                    size: lua_content.len(),
                },
                InstalledFileInfo {
                    filename: "12345_123456.manifest".to_string(),
                    kind: "manifest".to_string(),
                    sha256: compute_sha256(b"original manifest"),
                    size: 18,
                },
            ],
            externally_modified: false,
        };

        let checks = verify_all_files(&record, &d.to_string_lossy().as_ref());
        assert_eq!(checks.len(), 2);
        let lua_check = checks
            .iter()
            .find(|c| c.get("kind").unwrap() == "lua")
            .unwrap();
        assert!(lua_check.get("sha256Valid").unwrap().as_bool().unwrap());
        let manif_check = checks
            .iter()
            .find(|c| c.get("kind").unwrap() == "manifest")
            .unwrap();
        assert!(!manif_check.get("sha256Valid").unwrap().as_bool().unwrap());
        cleanup(&d);
    }

    // ========== Metadata I/O: Load from Path ==========

    #[test]
    fn test_load_metadata_missing_file() {
        let d = test_dir("load_miss");
        let p = d.join("nonexistent.json");
        let db = load_metadata_from_path(&p);
        assert_eq!(db.schema_version, 1);
        assert!(db.packages.is_empty());
        cleanup(&d);
    }

    #[test]
    fn test_load_metadata_corrupt_json() {
        let d = test_dir("load_corrupt");
        let p = d.join("installed_packages.json");
        fs::write(&p, "{{invalid json!!!").unwrap();
        let db = load_metadata_from_path(&p);
        assert_eq!(db.schema_version, 1);
        assert!(db.packages.is_empty());
        let backups: Vec<_> = fs::read_dir(&d)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path()
                    .to_string_lossy()
                    .contains("installed_packages.corrupt.")
            })
            .collect();
        assert_eq!(backups.len(), 1, "Backup file should be created");
        cleanup(&d);
    }

    // ========== Metadata I/O: Save and Load Round-trip ==========

    #[test]
    fn test_save_load_roundtrip() {
        let d = test_dir("roundtrip");
        let p = d.join("installed_packages.json");

        let mut db = InstalledPackagesDB::default();
        let record = ProviderInstallRecord {
            app_id: "12345".to_string(),
            provider_id: "hubcapdb".to_string(),
            version_known: true,
            version_source: "provider_status".to_string(),
            remote_modified: Some("2026-05-22T09:30:00".to_string()),
            remote_modified_unix: Some(1779442200),
            installed_at: "2026-07-26T06:30:00Z".to_string(),
            lua_filename: "12345.lua".to_string(),
            files: vec![InstalledFileInfo {
                filename: "12345.lua".to_string(),
                kind: "lua".to_string(),
                sha256: "abc123".to_string(),
                size: 100,
            }],
            externally_modified: false,
        };
        let entry = PackageEntry {
            active_provider_id: "hubcapdb".to_string(),
            providers: {
                let mut m = HashMap::new();
                m.insert("hubcapdb".to_string(), record);
                m
            },
        };
        db.packages.insert("12345".to_string(), entry);

        save_metadata_to_path(&db, &p).unwrap();
        let loaded = load_metadata_from_path(&p);

        assert_eq!(loaded.schema_version, 1);
        assert_eq!(loaded.packages.len(), 1);
        let loaded_entry = loaded.packages.get("12345").unwrap();
        assert_eq!(loaded_entry.active_provider_id, "hubcapdb");
        let loaded_record = loaded_entry.providers.get("hubcapdb").unwrap();
        assert!(loaded_record.version_known);
        assert_eq!(
            loaded_record.remote_modified,
            Some("2026-05-22T09:30:00".to_string())
        );
        assert_eq!(loaded_record.files.len(), 1);
        assert_eq!(loaded_record.files[0].sha256, "abc123");
        cleanup(&d);
    }

    // ========== Corrupt JSON Recovery ==========

    #[test]
    fn test_corrupt_recovery_preserves_backup() {
        let d = test_dir("corrupt_recovery");
        let p = d.join("installed_packages.json");
        fs::write(&p, "{ broken json content! }").unwrap();

        let before: Vec<String> = fs::read_dir(&d)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path().to_string_lossy().to_string())
            .collect();

        let db = load_metadata_from_path(&p);
        assert_eq!(db.schema_version, 1);
        assert!(db.packages.is_empty());

        let after: Vec<String> = fs::read_dir(&d)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path().to_string_lossy().to_string())
            .collect();

        let new_files: Vec<&String> = after.iter().filter(|f| !before.contains(f)).collect();
        let new_backups: Vec<&&String> =
            new_files.iter().filter(|f| f.contains("corrupt")).collect();
        assert_eq!(
            new_backups.len(),
            1,
            "Exactly one new corrupt backup should be created, found: {:?}",
            new_backups
        );
        assert!(new_backups[0].contains(".corrupt."));
        cleanup(&d);
    }

    #[test]
    fn test_corrupt_recovery_allows_new_writes() {
        let d = test_dir("corrupt_write");
        let p = d.join("installed_packages.json");
        fs::write(&p, "not json").unwrap();

        let mut db = load_metadata_from_path(&p);
        assert!(db.packages.is_empty());

        let record = ProviderInstallRecord {
            app_id: "99999".to_string(),
            provider_id: "hubcapdb".to_string(),
            version_known: true,
            version_source: "provider_status".to_string(),
            remote_modified: Some("2026-06-01T00:00:00".to_string()),
            remote_modified_unix: Some(1780329600),
            installed_at: "2026-07-26T00:00:00Z".to_string(),
            lua_filename: "99999.lua".to_string(),
            files: vec![],
            externally_modified: false,
        };
        let entry = PackageEntry {
            active_provider_id: "hubcapdb".to_string(),
            providers: {
                let mut m = HashMap::new();
                m.insert("hubcapdb".to_string(), record);
                m
            },
        };
        db.packages.insert("99999".to_string(), entry);

        save_metadata_to_path(&db, &p).unwrap();
        let reloaded = load_metadata_from_path(&p);
        assert_eq!(reloaded.packages.len(), 1);
        assert!(reloaded.packages.contains_key("99999"));
        cleanup(&d);
    }

    // ========== Migration Idempotency ==========

    #[test]
    fn test_migration_idempotent() {
        let d = test_dir("migrate_idempotent");
        let p = d.join("installed_packages.json");

        let mut db = InstalledPackagesDB::default();
        let record = ProviderInstallRecord {
            app_id: "55555".to_string(),
            provider_id: "hubcapdb".to_string(),
            version_known: true,
            version_source: "lua_header_migration".to_string(),
            remote_modified: Some("2026-05-20T10:00:00".to_string()),
            remote_modified_unix: Some(1779328800),
            installed_at: "2026-07-26T00:00:00Z".to_string(),
            lua_filename: "55555.lua".to_string(),
            files: vec![],
            externally_modified: false,
        };
        let entry = PackageEntry {
            active_provider_id: "hubcapdb".to_string(),
            providers: {
                let mut m = HashMap::new();
                m.insert("hubcapdb".to_string(), record.clone());
                m
            },
        };
        db.packages.insert("55555".to_string(), entry);
        save_metadata_to_path(&db, &p).unwrap();

        let mut db2 = load_metadata_from_path(&p);
        let new_record = ProviderInstallRecord {
            version_known: true,
            version_source: "lua_header_migration".to_string(),
            remote_modified: Some("2026-05-20T12:00:00".to_string()),
            remote_modified_unix: Some(1779336000),
            ..record.clone()
        };

        if let Some(entry) = db2.packages.get_mut("55555") {
            if let Some(existing) = entry.providers.get("hubcapdb") {
                if existing.version_known {
                    let _ = existing;
                }
            }
            entry.providers.insert("hubcapdb".to_string(), new_record);
        }
        save_metadata_to_path(&db2, &p).unwrap();

        let reloaded = load_metadata_from_path(&p);
        let re = reloaded.packages.get("55555").unwrap();
        let rr = re.providers.get("hubcapdb").unwrap();
        assert!(rr.version_known);
        assert_eq!(rr.version_source, "lua_header_migration");
        assert_eq!(rr.remote_modified_unix, Some(1779336000));
        cleanup(&d);
    }

    // ========== Provider Isolation ==========

    #[test]
    fn test_provider_isolation_same_app() {
        let d = test_dir("provider_iso");
        let p = d.join("installed_packages.json");

        let record_hubcap = ProviderInstallRecord {
            app_id: "77777".to_string(),
            provider_id: "hubcapdb".to_string(),
            version_known: true,
            version_source: "provider_status".to_string(),
            remote_modified: Some("2026-05-22T09:30:00".to_string()),
            remote_modified_unix: Some(1779442200),
            installed_at: "2026-07-26T00:00:00Z".to_string(),
            lua_filename: "77777.lua".to_string(),
            files: vec![InstalledFileInfo {
                filename: "77777.lua".to_string(),
                kind: "lua".to_string(),
                sha256: "aaa".to_string(),
                size: 100,
            }],
            externally_modified: false,
        };
        let record_ryuu = ProviderInstallRecord {
            app_id: "77777".to_string(),
            provider_id: "ryuu".to_string(),
            version_known: false,
            version_source: "unavailable".to_string(),
            remote_modified: None,
            remote_modified_unix: None,
            installed_at: "2026-07-26T00:00:00Z".to_string(),
            lua_filename: "77777.lua".to_string(),
            files: vec![],
            externally_modified: false,
        };

        let mut providers = HashMap::new();
        providers.insert("hubcapdb".to_string(), record_hubcap);
        providers.insert("ryuu".to_string(), record_ryuu);

        let entry = PackageEntry {
            active_provider_id: "hubcapdb".to_string(),
            providers,
        };

        let mut db = InstalledPackagesDB::default();
        db.packages.insert("77777".to_string(), entry);
        save_metadata_to_path(&db, &p).unwrap();

        let loaded = load_metadata_from_path(&p);
        let le = loaded.packages.get("77777").unwrap();
        assert_eq!(le.active_provider_id, "hubcapdb");
        assert_eq!(le.providers.len(), 2);

        let hubcap = le.providers.get("hubcapdb").unwrap();
        assert!(hubcap.version_known);
        assert_eq!(hubcap.remote_modified_unix, Some(1779442200));

        let ryuu = le.providers.get("ryuu").unwrap();
        assert!(!ryuu.version_known);
        assert!(ryuu.remote_modified_unix.is_none());
        cleanup(&d);
    }

    #[test]
    fn test_active_provider_changes_only_after_install() {
        let d = test_dir("provider_change");
        let p = d.join("installed_packages.json");

        let mut db = InstalledPackagesDB::default();
        let record = ProviderInstallRecord {
            app_id: "88888".to_string(),
            provider_id: "hubcapdb".to_string(),
            version_known: true,
            version_source: "provider_status".to_string(),
            remote_modified: Some("2026-05-22T09:30:00".to_string()),
            remote_modified_unix: Some(1779442200),
            installed_at: "2026-07-26T00:00:00Z".to_string(),
            lua_filename: "88888.lua".to_string(),
            files: vec![],
            externally_modified: false,
        };
        let entry = PackageEntry {
            active_provider_id: "hubcapdb".to_string(),
            providers: {
                let mut m = HashMap::new();
                m.insert("hubcapdb".to_string(), record);
                m
            },
        };
        db.packages.insert("88888".to_string(), entry);
        save_metadata_to_path(&db, &p).unwrap();

        let loaded = load_metadata_from_path(&p);
        assert_eq!(
            loaded.packages.get("88888").unwrap().active_provider_id,
            "hubcapdb"
        );

        let new_record = ProviderInstallRecord {
            app_id: "88888".to_string(),
            provider_id: "ryuu".to_string(),
            version_known: true,
            version_source: "provider_status".to_string(),
            remote_modified: Some("2026-06-01T00:00:00".to_string()),
            remote_modified_unix: Some(1780329600),
            installed_at: "2026-07-26T00:00:00Z".to_string(),
            lua_filename: "88888.lua".to_string(),
            files: vec![],
            externally_modified: false,
        };
        let mut db2 = load_metadata_from_path(&p);
        let entry2 = db2.packages.get_mut("88888").unwrap();
        entry2.providers.insert("ryuu".to_string(), new_record);
        entry2.active_provider_id = "ryuu".to_string();
        save_metadata_to_path(&db2, &p).unwrap();

        let loaded2 = load_metadata_from_path(&p);
        assert_eq!(
            loaded2.packages.get("88888").unwrap().active_provider_id,
            "ryuu"
        );
        assert!(loaded2
            .packages
            .get("88888")
            .unwrap()
            .providers
            .contains_key("hubcapdb"));
        cleanup(&d);
    }

    // ========== Copied File Scenario (mtime spoofing) ==========

    #[test]
    fn test_copied_file_sha256_wins_over_size() {
        let d = test_dir("copied_file");
        let lua_dir = d.join("config\\lua");
        fs::create_dir_all(&lua_dir).unwrap();

        let original = b"-- Created: May 20, 2026 at 12:00:00 EDT\nreturn {version='old'}\n";
        let sha_original = compute_sha256(original);
        fs::write(lua_dir.join("44444.lua"), original).unwrap();

        let record = make_record(
            "44444",
            "hubcapdb",
            "44444.lua",
            &sha_original,
            original.len(),
        );

        let fv = verify_lua_file(&record, &d.to_string_lossy().as_ref());
        assert!(fv.file_exists);
        assert!(!fv.externally_modified);

        let same_size_different_content =
            b"-- Created: May 20, 2026 at 12:00:00 EDT\nreturn {version='new'}\n";
        assert_eq!(same_size_different_content.len(), original.len());
        fs::write(lua_dir.join("44444.lua"), same_size_different_content).unwrap();

        let fv2 = verify_lua_file(&record, &d.to_string_lossy().as_ref());
        assert!(fv2.file_exists);
        assert!(
            fv2.externally_modified,
            "SHA-256 should detect same-size content change"
        );
        cleanup(&d);
    }

    #[test]
    fn test_copied_file_newer_mtime_different_content() {
        let d = test_dir("mtime_spoof");
        let lua_dir = d.join("config\\lua");
        fs::create_dir_all(&lua_dir).unwrap();

        let original = b"-- Created: May 20, 2026 at 12:00:00 EDT\nreturn {}\n";
        let sha_original = compute_sha256(original);
        fs::write(lua_dir.join("33333.lua"), original).unwrap();

        let record = make_record(
            "33333",
            "hubcapdb",
            "33333.lua",
            &sha_original,
            original.len(),
        );
        let fv = verify_lua_file(&record, &d.to_string_lossy().as_ref());
        assert!(fv.file_exists);
        assert!(!fv.externally_modified);

        let different = b"completely different content, same file name\n";
        fs::write(lua_dir.join("33333.lua"), different).unwrap();

        let fv2 = verify_lua_file(&record, &d.to_string_lossy().as_ref());
        assert!(fv2.file_exists);
        assert!(
            fv2.externally_modified,
            "Different content = externally modified regardless of mtime"
        );
        assert_eq!(fv2.current_size, different.len());
        assert_ne!(fv2.current_size, record.files[0].size);
        cleanup(&d);
    }

    // ========== Format Timestamp UTC ==========

    #[test]
    fn test_format_timestamp_utc_epoch() {
        assert_eq!(format_timestamp_utc(0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn test_format_timestamp_utc_known() {
        let ts = ymdhms_to_unix_utc(2026, 7, 26, 12, 30, 45);
        let formatted = format_timestamp_utc(ts);
        assert_eq!(formatted, "2026-07-26T12:30:45Z");
    }

    #[test]
    fn test_format_timestamp_utc_roundtrip() {
        for &(y, m, d, h, mi, s) in &[
            (2026, 1, 1, 0, 0, 0),
            (2026, 6, 15, 23, 59, 59),
            (2025, 12, 31, 12, 0, 0),
            (2024, 2, 29, 8, 30, 15),
        ] {
            let ts = ymdhms_to_unix_utc(y, m, d, h, mi, s);
            let fmt = format_timestamp_utc(ts);
            let ts2 = parse_iso_timestamp_rust(&fmt);
            assert_eq!(ts, ts2, "Round-trip failed for {:?}", (y, m, d, h, mi, s));
        }
    }

    // ========== Leap Year ==========

    #[test]
    fn test_is_leap() {
        assert!(is_leap(2024));
        assert!(!is_leap(2025));
        assert!(!is_leap(1900));
        assert!(is_leap(2000));
    }

    // ========== Metadata Persistence Failure ==========

    #[test]
    fn test_metadata_save_to_readonly_path_fails() {
        let d = test_dir("readonly_fail");
        let p = d.join("nonexistent_dir").join("installed_packages.json");
        let db = InstalledPackagesDB::default();
        let result = save_metadata_to_path(&db, &p);
        assert!(result.is_err());
        let err_msg = result.unwrap_err();
        assert!(
            err_msg.contains("error"),
            "Error message should indicate failure: {}",
            err_msg
        );
        cleanup(&d);
    }
}
