use serde_json::{json, Value};
use std::path::PathBuf;

// Steam account settings: Web API Key + SteamID64/32 + accountName.
// Persisted in config.json under the `steam` section (outside the repo).
// Consumers: Goldberg achievements (apiKey), catalog/Voices38 fix (steamId64/accountName).

#[derive(Debug, Clone, Default)]
pub struct SteamAccount {
    pub api_key: Option<String>,
    pub steam_id64: Option<String>,
    pub steam_id32: Option<String>,
    pub account_name: Option<String>,
}

fn config_path() -> PathBuf {
    crate::platform::config_json_path()
}

fn strip_bom(s: &str) -> &str {
    s.trim_start_matches('\u{feff}')
}

fn opt_str(v: &Value) -> Option<String> {
    v.as_str()
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
}

pub fn load_steam_account() -> SteamAccount {
    let mut acct = SteamAccount::default();
    let Ok(raw) = std::fs::read_to_string(config_path()) else {
        return acct;
    };
    let Ok(val) = serde_json::from_str::<Value>(strip_bom(&raw)) else {
        return acct;
    };
    let Some(steam) = val.get("steam") else {
        return acct;
    };
    acct.api_key = steam.get("apiKey").and_then(opt_str);
    acct.steam_id64 = steam.get("steamId64").and_then(opt_str);
    acct.steam_id32 = steam.get("steamId32").and_then(opt_str);
    acct.account_name = steam.get("accountName").and_then(opt_str);
    acct
}

/// Merge the given fields into config.json `steam` section.
/// Null or empty-string values remove the field; absent fields are untouched.
pub fn save_steam_account(fields: &Value) -> Result<(), String> {
    let path = config_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut root: Value = match std::fs::read_to_string(&path) {
        Ok(raw) => serde_json::from_str(strip_bom(&raw)).unwrap_or_else(|_| json!({})),
        Err(_) => json!({}),
    };
    if !root.is_object() {
        root = json!({});
    }
    let obj = root.as_object_mut().unwrap();
    let steam = obj
        .entry("steam")
        .or_insert_with(|| json!({}));
    if !steam.is_object() {
        *steam = json!({});
    }
    let steam_obj = steam.as_object_mut().unwrap();
    for key in ["apiKey", "steamId64", "steamId32", "accountName"] {
        if let Some(v) = fields.get(key) {
            if v.is_null() || v.as_str() == Some("") {
                steam_obj.remove(key);
            } else if let Some(s) = v.as_str() {
                steam_obj.insert(key.to_string(), json!(s));
            }
        }
    }
    let out = serde_json::to_string_pretty(&root).map_err(|e| e.to_string())?;
    std::fs::write(&path, out).map_err(|e| format!("write config.json: {e}"))?;
    Ok(())
}

fn steam_id64_to_account_id(id64: &str) -> Option<String> {
    let v: i64 = id64.parse().ok()?;
    Some((v - 76_561_197_960_265_728).to_string())
}

fn make_account_json(id64: &str, account: &str, persona: &str, remember: bool, timestamp: u64) -> Value {
    json!({
        "steamId64": id64,
        "steamId32": steam_id64_to_account_id(id64).unwrap_or_default(),
        "accountName": account,
        "personaName": persona,
        "rememberPassword": remember,
        "timestamp": timestamp
    })
}

/// Parse `<steam_root>/config/loginusers.vdf`, sorted by timestamp descending.
fn detect_accounts() -> Result<Vec<Value>, String> {
    let steam_root = crate::depot_downloader::steam_root()
        .ok_or_else(|| "Steam root not found".to_string())?;
    let loginusers_path = steam_root.join("config").join("loginusers.vdf");
    if !loginusers_path.exists() {
        return Ok(Vec::new());
    }
    let content = std::fs::read_to_string(&loginusers_path)
        .map_err(|e| format!("Failed to read loginusers.vdf: {e}"))?;

    let mut users: Vec<Value> = Vec::new();
    let mut current_id: Option<String> = None;
    let mut current_account_name: Option<String> = None;
    let mut current_persona_name: Option<String> = None;
    let mut current_remember_password = false;
    let mut current_timestamp: u64 = 0;
    let mut in_user = false;
    let mut brace_depth = 0u32;

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed == "{" {
            brace_depth += 1;
            if brace_depth == 2 && current_id.is_some() {
                in_user = true;
            }
            continue;
        }
        if trimmed == "}" {
            if in_user && brace_depth == 2 {
                if let (Some(id), Some(account)) = (current_id.take(), current_account_name.take()) {
                    users.push(make_account_json(
                        &id,
                        &account,
                        current_persona_name.as_deref().unwrap_or(""),
                        current_remember_password,
                        current_timestamp,
                    ));
                }
                current_persona_name = None;
                current_remember_password = false;
                current_timestamp = 0;
                in_user = false;
            }
            if brace_depth > 0 {
                brace_depth -= 1;
            }
            continue;
        }

        let parts: Vec<&str> = trimmed.split('"').collect();
        if parts.len() < 5 {
            // Could be a steam_id key at depth 1
            if brace_depth == 1 && trimmed.starts_with('"') {
                if let Some(id_val) = parts.get(1) {
                    let val = id_val.trim();
                    if !val.is_empty() && val.chars().all(|c| c.is_ascii_digit()) {
                        current_id = Some(val.to_string());
                    }
                }
            }
            continue;
        }

        let key = parts[1].trim();
        let value = parts[3].trim();
        if in_user {
            match key {
                "AccountName" => current_account_name = Some(value.to_string()),
                "PersonaName" => current_persona_name = Some(value.to_string()),
                "RememberPassword" => current_remember_password = value == "1",
                "Timestamp" => current_timestamp = value.parse::<u64>().unwrap_or(0),
                _ => {}
            }
        }
    }

    // Flush any remaining
    if in_user {
        if let (Some(id), Some(account)) = (current_id.take(), current_account_name.take()) {
            users.push(make_account_json(
                &id,
                &account,
                current_persona_name.as_deref().unwrap_or(""),
                current_remember_password,
                current_timestamp,
            ));
        }
    }

    users.sort_by(|a, b| {
        let ta = a.get("timestamp").and_then(|v| v.as_u64()).unwrap_or(0);
        let tb = b.get("timestamp").and_then(|v| v.as_u64()).unwrap_or(0);
        tb.cmp(&ta)
    });
    Ok(users)
}

pub fn try_handle_route(method: &str, path: &str, body: &str) -> Option<(u16, String)> {
    if path == "/api/steam-account" {
        if method == "GET" {
            let a = load_steam_account();
            return Some((
                200,
                json!({
                    "ok": true,
                    "apiKey": a.api_key.unwrap_or_default(),
                    "steamId64": a.steam_id64.unwrap_or_default(),
                    "steamId32": a.steam_id32.unwrap_or_default(),
                    "accountName": a.account_name.unwrap_or_default()
                })
                .to_string(),
            ));
        }
        if method == "POST" {
            let fields: Value = match serde_json::from_str(body) {
                Ok(v) => v,
                Err(e) => {
                    return Some((
                        400,
                        json!({"ok": false, "message": format!("Invalid JSON: {e}")}).to_string(),
                    ))
                }
            };
            return match save_steam_account(&fields) {
                Ok(()) => Some((200, json!({"ok": true}).to_string())),
                Err(e) => Some((200, json!({"ok": false, "message": e}).to_string())),
            };
        }
    }
    if path == "/api/steam-account/detect" && method == "GET" {
        return Some(match detect_accounts() {
            Ok(accounts) => (200, json!({"ok": true, "accounts": accounts}).to_string()),
            Err(e) => (200, json!({"ok": false, "message": e}).to_string()),
        });
    }
    None
}
