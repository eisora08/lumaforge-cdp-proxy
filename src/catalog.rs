//! Game catalog + artwork endpoints.
//!
//! `GET /api/catalog` — unified rows for every app we know about:
//! librarycache dirs ∪ installed appmanifests ∪ cloudsave apps ∪ lua files.
//! `GET /api/art/{id}` — artwork as base64 JSON. The Steam store page runs on
//! HTTPS, so plain `<img src="http://127.0.0.1:21775/...">` would be blocked
//! as mixed content; the JSON body travels through the existing bridge fetch
//! interceptor instead.

use base64::Engine;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const CATALOG_TTL: Duration = Duration::from_secs(15);

static CATALOG_CACHE: Mutex<Option<(String, Instant)>> = Mutex::new(None);

pub fn try_handle_route(method: &str, path: &str, _body: &str) -> Option<(u16, String)> {
    if method != "GET" {
        return None;
    }
    if let Some(rest) = path.strip_prefix("/api/art/") {
        let id = rest.trim_end_matches('/');
        if !id.is_empty() {
            return Some(handle_art(id));
        }
        return None;
    }
    if path == "/api/catalog" {
        return Some(handle_catalog());
    }
    None
}

// ---------------------------------------------------------------------------
// /api/art/{id}
// ---------------------------------------------------------------------------

fn handle_art(app_id: &str) -> (u16, String) {
    if app_id.parse::<u64>().is_err() {
        return (400, json!({"ok": false, "error": "invalid app id"}).to_string());
    }

    // 1. Local Steam librarycache (covers apps with dead CDN entries)
    if let Some(bytes) = read_local_art(app_id) {
        crate::log_to_temp(&format!("[art] {app_id} served from librarycache"));
        return (200, art_ok(&bytes, "local"));
    }

    // 2. Disk cache from a previous CDN fetch
    let cache_path = art_cache_path(app_id);
    if let Ok(bytes) = std::fs::read(&cache_path) {
        if bytes.len() >= 512 {
            return (200, art_ok(&bytes, "disk"));
        }
    }

    // 3. Steam CDN (server-side, avoids mixed-content)
    if let Some(bytes) = fetch_cdn_art(app_id) {
        if let Some(parent) = cache_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&cache_path, &bytes);
        crate::log_to_temp(&format!("[art] {app_id} served from CDN"));
        return (200, art_ok(&bytes, "cdn"));
    }

    crate::log_to_temp(&format!("[art] {app_id} not found (local+CDN)"));
    (404, json!({"ok": false, "error": "no artwork"}).to_string())
}

fn art_ok(bytes: &[u8], src: &str) -> String {
    json!({
        "ok": true,
        "ct": "image/jpeg",
        "src": src,
        "b64": base64::engine::general_purpose::STANDARD.encode(bytes)
    })
    .to_string()
}

fn art_cache_path(app_id: &str) -> PathBuf {
    crate::platform::local_data_dir()
        .join("cache")
        .join("art")
        .join(format!("{app_id}.jpg"))
}

fn librarycache_root() -> Option<PathBuf> {
    let steam_root = crate::depot_downloader::steam_root()?;
    let root = steam_root.join("appcache").join("librarycache");
    root.is_dir().then_some(root)
}

fn read_local_art(app_id: &str) -> Option<Vec<u8>> {
    let root = librarycache_root()?;
    let app_dir = root.join(app_id);

    let direct = app_dir.join("library_header.jpg");
    if direct.is_file() {
        if let Ok(bytes) = std::fs::read(&direct) {
            if !bytes.is_empty() {
                return Some(bytes);
            }
        }
    }

    // Hashed subdirs: {sha1}/library_header.jpg — newest mtime wins
    let mut best: Option<(SystemTime, PathBuf)> = None;
    let entries = std::fs::read_dir(&app_dir).ok()?;
    for entry in entries.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        let candidate = entry.path().join("library_header.jpg");
        if !candidate.is_file() {
            continue;
        }
        let mt = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .unwrap_or(UNIX_EPOCH);
        if best.as_ref().map(|(t, _)| mt > *t).unwrap_or(true) {
            best = Some((mt, candidate));
        }
    }
    let (_, path) = best?;
    std::fs::read(path).ok().filter(|b| !b.is_empty())
}

fn fetch_cdn_art(app_id: &str) -> Option<Vec<u8>> {
    let url = format!(
        "https://cdn.cloudflare.steamstatic.com/steam/apps/{app_id}/header.jpg"
    );
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(6))
        .build()
        .ok()?;
    let resp = client.get(&url).send().ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let bytes = resp.bytes().ok()?;
    // Reject tiny bodies (error pages / placeholders)
    if bytes.len() < 512 {
        return None;
    }
    Some(bytes.to_vec())
}

// ---------------------------------------------------------------------------
// /api/catalog
// ---------------------------------------------------------------------------

fn handle_catalog() -> (u16, String) {
    if let Ok(guard) = CATALOG_CACHE.lock() {
        if let Some((body, at)) = guard.as_ref() {
            if at.elapsed() < CATALOG_TTL {
                return (200, body.clone());
            }
        }
    }

    let body = build_catalog();

    if let Ok(mut guard) = CATALOG_CACHE.lock() {
        *guard = Some((body.clone(), Instant::now()));
    }
    (200, body)
}

fn base_row(app_id: &str) -> Value {
    json!({
        "appId": app_id,
        "name": "",
        "installed": false,
        "cloudsave": false,
        "lua": false,
        "art": format!("/api/art/{app_id}")
    })
}

fn build_catalog() -> String {
    let started = Instant::now();
    let mut apps: HashMap<String, Value> = HashMap::new();

    // 1. Librarycache universe (browsed/cached apps, installed or not)
    if let Some(root) = librarycache_root() {
        if let Ok(rd) = std::fs::read_dir(root) {
            for entry in rd.flatten() {
                if !entry.path().is_dir() {
                    continue;
                }
                let id = entry.file_name().to_string_lossy().into_owned();
                if id.parse::<u64>().is_err() {
                    continue;
                }
                apps.entry(id.clone()).or_insert_with(|| base_row(&id));
            }
        }
    }

    // 2. Installed apps (appmanifest_*.acf across library roots)
    for lib in crate::game_fix::library_roots() {
        let Ok(rd) = std::fs::read_dir(lib.join("steamapps")) else {
            continue;
        };
        for entry in rd.flatten() {
            let fname = entry.file_name().to_string_lossy().into_owned();
            let Some(id) = fname
                .strip_prefix("appmanifest_")
                .and_then(|s| s.strip_suffix(".acf"))
            else {
                continue;
            };
            if id.parse::<u64>().is_err() {
                continue;
            }
            let row = apps.entry(id.to_string()).or_insert_with(|| base_row(id));
            row["installed"] = json!(true);
            if let Ok(content) = std::fs::read_to_string(entry.path()) {
                if let Some(name) = crate::game_fix::parse_acf_field(&content, "name") {
                    if !name.is_empty() {
                        row["name"] = json!(name);
                    }
                }
            }
        }
    }

    // 3. Cloud Saves rows (local storage; no network)
    for row in crate::cloudsave::catalog_rows() {
        let Some(id) = row["appId"].as_str().map(|s| s.to_string()) else {
            continue;
        };
        if id.is_empty() {
            continue;
        }
        let name = row["name"].as_str().filter(|n| !n.is_empty()).map(|n| n.to_string());
        let entry = apps.entry(id.clone()).or_insert_with(|| base_row(&id));
        entry["cloudsave"] = json!({
            "fileCount": row["fileCount"],
            "sizeBytes": row["sizeBytes"],
            "modified": row["modified"],
        });
        if let Some(n) = name {
            if entry["name"].as_str().unwrap_or("").is_empty() {
                entry["name"] = json!(n);
            }
        }
    }

    // 4. Lua files
    for row in lua_rows() {
        let Some(id) = row["appId"].as_str().map(|s| s.to_string()) else {
            continue;
        };
        let name = row["name"].as_str().filter(|n| !n.is_empty()).map(|n| n.to_string());
        let entry = apps.entry(id.clone()).or_insert_with(|| base_row(&id));
        entry["lua"] = json!({ "file": row["filename"] });
        if let Some(n) = name {
            if entry["name"].as_str().unwrap_or("").is_empty() {
                entry["name"] = json!(n);
            }
        }
    }

    // 5. Name fallback: appnames.json cache → "App {id}"
    let appnames = read_appnames_cache();
    let mut list: Vec<Value> = apps.into_values().collect();
    for row in &mut list {
        if row["name"].as_str().unwrap_or("").is_empty() {
            let id = row["appId"].as_str().unwrap_or("");
            let name = appnames
                .get(id)
                .cloned()
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| format!("App {id}"));
            row["name"] = json!(name);
        }
    }
    list.sort_by_key(|r| {
        r["appId"]
            .as_str()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(u64::MAX)
    });

    crate::log_to_temp(&format!(
        "[catalog] built {} apps in {}ms",
        list.len(),
        started.elapsed().as_millis()
    ));

    json!({
        "ok": true,
        "count": list.len(),
        "generatedAt": std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
        "apps": list,
    })
    .to_string()
}

fn read_appnames_cache() -> HashMap<String, String> {
    let path = crate::platform::local_data_dir().join("appnames.json");
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| {
            v.as_object().map(|o| {
                o.iter()
                    .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                    .collect()
            })
        })
        .unwrap_or_default()
}

fn lua_rows() -> Vec<Value> {
    let steam_root = crate::depot_downloader::steam_root();
    let lua_dir = steam_root.map(|r| r.join("config").join("lua"));
    let lua_dir = match lua_dir {
        Some(d) if d.exists() => d,
        _ => return Vec::new(),
    };

    let mut files: Vec<Value> = Vec::new();
    let Ok(rd) = std::fs::read_dir(lua_dir) else {
        return files;
    };
    for entry in rd.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if !name_str.ends_with(".lua") || name_str.contains(".backup") {
            continue;
        }
        let app_id = name_str.trim_end_matches(".lua").to_string();

        // Game name from the comment on line 2 (same heuristic as /api/lua-files)
        let mut game_name = String::new();
        if let Ok(content) = std::fs::read_to_string(entry.path()) {
            for line in content.lines().take(3) {
                let trimmed = line.trim();
                if trimmed.starts_with("--") && trimmed.len() > 3 {
                    let candidate = trimmed[2..].trim();
                    if !candidate.is_empty()
                        && !candidate.contains("Lua")
                        && !candidate.contains("LumaForge")
                        && !candidate.contains("Manifest")
                        && !candidate.contains("Created")
                        && !candidate.contains("Website")
                        && !candidate.contains("Total")
                        && !candidate.contains("MAIN APPLICATION")
                    {
                        game_name = candidate.to_string();
                        break;
                    }
                }
            }
        }

        files.push(json!({
            "appId": app_id,
            "filename": name_str,
            "name": game_name,
        }));
    }
    files
}
