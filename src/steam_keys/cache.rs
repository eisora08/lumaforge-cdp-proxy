use serde_json::json;
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

pub(super) const DEPOT_KEYS_URL: &str = "https://api.993499094.xyz/depotkeys.json";
pub(super) const TOKEN_KEYS_URL: &str = "https://api.993499094.xyz/appaccesstokens.json";
pub(super) const KEY_INDEX_URL: &str = "https://pan.qzyun.net/f/d/MlArs0/key.txt";
const UPDATE_INTERVAL_SECS: u64 = 86400;

pub(super) fn strip_bom(s: &str) -> &str {
    s.trim_start_matches('\u{feff}')
}

pub(super) fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub(super) fn read_config() -> serde_json::Value {
    match fs::read_to_string(crate::platform::config_json_path()) {
        Ok(raw) => serde_json::from_str(strip_bom(&raw)).unwrap_or_else(|_| json!({})),
        Err(_) => json!({}),
    }
}

fn cache_dir() -> Result<std::path::PathBuf, String> {
    let dir = crate::platform::config_dir().join("cache").join("steam_keys");
    fs::create_dir_all(&dir).map_err(|e| format!("Failed to create cache dir: {e}"))?;
    Ok(dir)
}

fn build_client(timeout_secs: u64) -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .user_agent(format!(
            "{}/{}",
            env!("CARGO_PKG_NAME"),
            env!("CARGO_PKG_VERSION")
        ))
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .connect_timeout(std::time::Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {e}"))
}

pub(super) fn build_http_client(timeout_secs: u64) -> Result<reqwest::blocking::Client, String> {
    build_client(timeout_secs)
}

fn needs_update() -> Result<bool, String> {
    let dir = cache_dir()?;
    if !dir.join("depotkeys.json").exists() || !dir.join("appaccesstokens.json").exists() {
        return Ok(true);
    }
    let last = fs::read_to_string(dir.join(".lastupdate"))
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0);
    Ok(now_secs().saturating_sub(last) >= UPDATE_INTERVAL_SECS)
}

fn write_key_files(dir: &Path, depot_bytes: &[u8], token_bytes: &[u8]) -> Result<(), String> {
    fs::write(dir.join("depotkeys.json"), depot_bytes)
        .map_err(|e| format!("Failed to write depotkeys.json: {e}"))?;
    fs::write(dir.join("appaccesstokens.json"), token_bytes)
        .map_err(|e| format!("Failed to write appaccesstokens.json: {e}"))?;
    let _ = fs::write(dir.join(".lastupdate"), now_secs().to_string());
    Ok(())
}

pub(super) fn update_key_files() -> Result<(), String> {
    let dir = cache_dir()?;
    let client = build_client(60)?;

    let primary = (|| -> Result<(), String> {
        let depot = client
            .get(DEPOT_KEYS_URL)
            .send()
            .and_then(|r| r.error_for_status())
            .and_then(|r| r.bytes())
            .map_err(|e| format!("Failed to download depotkeys.json: {e}"))?;
        let token = client
            .get(TOKEN_KEYS_URL)
            .send()
            .and_then(|r| r.error_for_status())
            .and_then(|r| r.bytes())
            .map_err(|e| format!("Failed to download appaccesstokens.json: {e}"))?;
        if depot.len() < 100 {
            return Err("depotkeys.json too small, likely invalid".to_string());
        }
        if token.len() < 50 {
            return Err("appaccesstokens.json too small, likely invalid".to_string());
        }
        write_key_files(&dir, &depot, &token)
    })();
    if primary.is_ok() {
        return primary;
    }

    // Fallback: resolve URLs from the key index
    let index = client
        .get(KEY_INDEX_URL)
        .send()
        .and_then(|r| r.error_for_status())
        .and_then(|r| r.text())
        .map_err(|e| format!("Failed to download key index: {e}"))?;
    let mut depot_url = String::new();
    let mut token_url = String::new();
    for line in index.lines() {
        let url = line.trim();
        if url.ends_with("depotkeys.json") {
            depot_url = url.to_string();
        } else if url.ends_with("appaccesstokens.json") {
            token_url = url.to_string();
        }
    }
    if depot_url.is_empty() || token_url.is_empty() {
        return Err("Could not resolve depot/token URLs from key index".to_string());
    }
    let depot = client
        .get(&depot_url)
        .send()
        .and_then(|r| r.error_for_status())
        .and_then(|r| r.bytes())
        .map_err(|e| format!("Failed to download depotkeys.json: {e}"))?;
    let token = client
        .get(&token_url)
        .send()
        .and_then(|r| r.error_for_status())
        .and_then(|r| r.bytes())
        .map_err(|e| format!("Failed to download appaccesstokens.json: {e}"))?;
    write_key_files(&dir, &depot, &token)
}

pub(super) fn ensure_key_files() -> Result<(), String> {
    if !needs_update()? {
        return Ok(());
    }
    update_key_files()
}

pub(super) fn load_depot_keys() -> Result<HashMap<u64, String>, String> {
    ensure_key_files()?;
    let path = cache_dir()?.join("depotkeys.json");
    let raw =
        fs::read_to_string(&path).map_err(|e| format!("Failed to read depotkeys.json: {e}"))?;
    let json: HashMap<String, String> =
        serde_json::from_str(&raw).map_err(|e| format!("Failed to parse depotkeys.json: {e}"))?;
    let mut result = HashMap::new();
    for (key, value) in json {
        if let Ok(id) = key.parse::<u64>() {
            if value.len() >= 40 {
                result.insert(id, value);
            }
        }
    }
    Ok(result)
}

pub(super) fn load_app_tokens() -> Result<HashMap<u64, String>, String> {
    ensure_key_files()?;
    let path = cache_dir()?.join("appaccesstokens.json");
    let raw = fs::read_to_string(&path)
        .map_err(|e| format!("Failed to read appaccesstokens.json: {e}"))?;
    let json: HashMap<String, String> = serde_json::from_str(&raw)
        .map_err(|e| format!("Failed to parse appaccesstokens.json: {e}"))?;
    let mut result = HashMap::new();
    for (key, value) in json {
        if let Ok(id) = key.parse::<u64>() {
            if !value.is_empty() {
                result.insert(id, value);
            }
        }
    }
    Ok(result)
}
