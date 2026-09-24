mod cache;
mod fetcher;
mod lua_writer;
mod steamcmd;

use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{OnceLock, RwLock};

use cache::read_config;
use steamcmd::fetch_app_depot_info;

// ---------------------------------------------------------------------------
// Steam Keys provider — native .lua generation, manifest fetch and pin/unpin.
// Ported from LumaForge to the blocking-reqwest bridge.
//
// Routes:
//   POST /api/download               (only when sourceId == "steamkeys")
//   GET  /api/steam-keys/status/{id}
//   GET  /api/steam-keys/pins
//   POST /api/steam-keys/pin         {appId, mode: "current"|"latest"}
//   POST /api/steam-keys/unpin       {appId}   (0 = all games)
//   GET  /api/steam-keys/settings
//   POST /api/steam-keys/settings    {autoFetch}
// ---------------------------------------------------------------------------

const DEFAULT_MANIFEST_REPO: &str = "steamtools-games/ManifestHub3";

// ---------------------------------------------------------------------------
// Paths & settings
// ---------------------------------------------------------------------------

fn steam_root() -> Result<PathBuf, String> {
    crate::depot_downloader::steam_root()
        .ok_or_else(|| "Steam installation not found".to_string())
}

fn lua_dir() -> Result<PathBuf, String> {
    let dir = steam_root()?.join("config").join("lua");
    fs::create_dir_all(&dir).map_err(|e| format!("Failed to create lua directory: {e}"))?;
    Ok(dir)
}

fn lua_path_for(app_id: u64) -> Result<PathBuf, String> {
    Ok(lua_dir()?.join(format!("{}.lua", app_id)))
}

fn save_steam_keys_settings(fields: &Value) -> Result<(), String> {
    let path = crate::platform::config_json_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let mut root = read_config();
    if !root.is_object() {
        root = json!({});
    }
    let obj = root.as_object_mut().unwrap();
    let section = obj.entry("steamKeys").or_insert_with(|| json!({}));
    if !section.is_object() {
        *section = json!({});
    }
    let sec = section.as_object_mut().unwrap();
    if let Some(v) = fields.get("autoFetch") {
        sec.insert("autoFetch".to_string(), json!(v.as_bool().unwrap_or(true)));
    }
    if let Some(v) = fields.get("manifestRepo") {
        if let Some(s) = v.as_str() {
            if !s.is_empty() {
                sec.insert("manifestRepo".to_string(), json!(s));
            }
        }
    }
    let out = serde_json::to_string_pretty(&root).map_err(|e| e.to_string())?;
    fs::write(&path, out).map_err(|e| format!("write config.json: {e}"))?;
    Ok(())
}

fn auto_fetch_enabled() -> bool {
    read_config()
        .get("steamKeys")
        .and_then(|s| s.get("autoFetch"))
        .and_then(|v| v.as_bool())
        .unwrap_or(true)
}

fn manifest_repo() -> (String, String) {
    let raw = read_config()
        .get("steamKeys")
        .and_then(|s| s.get("manifestRepo"))
        .and_then(|v| v.as_str())
        .unwrap_or(DEFAULT_MANIFEST_REPO)
        .to_string();
    match raw.split_once('/') {
        Some((o, r)) if !o.is_empty() && !r.is_empty() => (o.to_string(), r.to_string()),
        _ => {
            let (o, r) = DEFAULT_MANIFEST_REPO.split_once('/').unwrap();
            (o.to_string(), r.to_string())
        }
    }
}

// ---------------------------------------------------------------------------
// ACF helpers (pin to current)
// ---------------------------------------------------------------------------

fn find_acf_file(steam_root: &std::path::Path, app_id: u64) -> Option<PathBuf> {
    let manifest_name = format!("appmanifest_{}.acf", app_id);
    let primary = steam_root.join("steamapps").join(&manifest_name);
    if primary.exists() {
        return Some(primary);
    }
    let vdf_path = steam_root.join("steamapps").join("libraryfolders.vdf");
    if let Ok(content) = fs::read_to_string(&vdf_path) {
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('"') && trimmed.contains("path") {
                let parts: Vec<&str> = trimmed.split('"').collect();
                if parts.len() >= 4 {
                    let acf = std::path::Path::new(parts[3])
                        .join("steamapps")
                        .join(&manifest_name);
                    if acf.exists() {
                        return Some(acf);
                    }
                }
            }
        }
    }
    None
}

fn parse_mounted_depots(acf_path: &std::path::Path) -> Result<HashMap<u64, String>, String> {
    let content =
        fs::read_to_string(acf_path).map_err(|e| format!("Failed to read ACF: {e}"))?;
    let mut mounted = HashMap::new();

    // Strategy 1: nested "depotId" { "manifest" "gid" } (InstalledDepots/MountedDepots)
    let re_nested = regex::Regex::new(r#""(\d+)"\s*\{\s*"manifest"\s+"(\d+)""#).unwrap();
    for cap in re_nested.captures_iter(&content) {
        if let Ok(depot_id) = cap[1].parse::<u64>() {
            mounted.insert(depot_id, cap[2].to_string());
        }
    }

    // Strategy 2: flat format inside MountedDepots section
    if mounted.is_empty() {
        let re_section = regex::Regex::new(r#""MountedDepots"\s*\{([^}]*)\}"#).unwrap();
        if let Some(section) = re_section.captures(&content) {
            let re_flat = regex::Regex::new(r#""(\d+)"\s+"(\d+)""#).unwrap();
            for cap in re_flat.captures_iter(&section[1]) {
                if let Ok(depot_id) = cap[1].parse::<u64>() {
                    mounted.insert(depot_id, cap[2].to_string());
                }
            }
        }
    }
    Ok(mounted)
}

// ---------------------------------------------------------------------------
// High-level operations
// ---------------------------------------------------------------------------

fn generate_lua_for_app(app_id: u64) -> Result<String, String> {
    let app_info = fetch_app_depot_info(app_id)?;
    let game_name = app_info
        .app_name
        .clone()
        .unwrap_or_else(|| format!("Game {}", app_id));
    let depot_keys = cache::load_depot_keys()?;
    let app_tokens = cache::load_app_tokens()?;
    let lua_dir = lua_dir()?;

    let mut depots: Vec<lua_writer::LuaDepotEntry> = Vec::new();
    if let Some(app_key) = depot_keys.get(&app_id).cloned() {
        depots.push(lua_writer::LuaDepotEntry::new(app_id).with_key(app_key));
    }
    for d in &app_info.depots {
        if d.depot_id == app_id {
            continue;
        }
        if d.dlc_app_id.is_none() || d.dlc_app_id == Some(app_id) {
            let mut entry = lua_writer::LuaDepotEntry::new(d.depot_id);
            if let Some(k) = depot_keys.get(&d.depot_id).cloned() {
                entry = entry.with_key(k);
            }
            if d.is_shared {
                if let Some(from) = d.from_app_id {
                    entry = entry.with_shared(from);
                }
            }
            depots.push(entry);
            continue;
        }
        if let Some(key) = depot_keys.get(&d.depot_id).cloned() {
            let mut entry = lua_writer::LuaDepotEntry::new(d.depot_id).with_key(key);
            if let Some(dlc_id) = d.dlc_app_id {
                entry = entry.with_dlc(dlc_id);
            }
            depots.push(entry);
        }
    }

    let mut relevant_ids = std::collections::HashSet::new();
    relevant_ids.insert(app_id);
    for &dlc_id in &app_info.dlc_ids {
        relevant_ids.insert(dlc_id);
    }
    let tokens: Vec<(u64, String)> = app_tokens
        .into_iter()
        .filter(|(id, _)| relevant_ids.contains(id))
        .collect();

    let manifest_pins: Vec<(u64, String, Option<u64>)> = app_info
        .depots
        .iter()
        .filter_map(|d| {
            d.public_manifest_id.as_ref().map(|mid| {
                let size = if d.size > 0 { Some(d.size) } else { None };
                (d.depot_id, mid.clone(), size)
            })
        })
        .collect();

    let path = lua_writer::generate_lua_file(
        &lua_dir,
        app_id,
        &game_name,
        &depots,
        &app_info.dlc_ids,
        &tokens,
        &manifest_pins,
    )?;
    Ok(path.to_string_lossy().to_string())
}

fn add_all_dlcs(app_id: u64) -> Result<(u32, u32), String> {
    let app_info = fetch_app_depot_info(app_id)?;
    if app_info.dlc_ids.is_empty() {
        return Ok((0, 0));
    }
    let lua_path = lua_path_for(app_id)?;
    if !lua_path.exists() {
        return Err(format!(
            "No Lua file found for app {}. Generate it first.",
            app_id
        ));
    }
    let existing = lua_writer::active_addappids(&lua_path);
    let depot_keys = cache::load_depot_keys().unwrap_or_default();

    let mut added = 0u32;
    let mut skipped = 0u32;
    for &dlc_id in &app_info.dlc_ids {
        if dlc_id == app_id {
            continue;
        }
        if existing.contains(&dlc_id) {
            skipped += 1;
            continue;
        }
        let key = depot_keys.get(&dlc_id).map(|k| k.as_str());
        lua_writer::add_dlc_to_lua(&lua_path, dlc_id, key, None)?;
        added += 1;
    }
    Ok((added, skipped))
}

fn fetch_manifests_op(app_id: u64) -> Result<(usize, usize), String> {
    let app_info = fetch_app_depot_info(app_id)?;
    let depot_ids: Vec<u64> = app_info.depots.iter().map(|d| d.depot_id).collect();
    if depot_ids.is_empty() {
        return Err(format!("No depots found for app {}", app_id));
    }
    let (owner, repo) = manifest_repo();
    let manifests = fetcher::fetch_manifests_for_game(app_id, &depot_ids, &owner, &repo)?;
    let count = manifests.len();
    let saved = manifests.iter().filter(|m| m.placed_path.is_some()).count();
    Ok((count, saved))
}

fn pin_op(app_id: u64, mode: &str) -> Result<String, String> {
    let lua_path = lua_path_for(app_id)?;
    if !lua_path.exists() {
        return Err(format!(
            "No Lua file found for app {}. Generate Lua first.",
            app_id
        ));
    }
    match mode {
        "current" => {
            let root = steam_root()?;
            let acf_path = find_acf_file(&root, app_id).ok_or_else(|| {
                format!("No ACF file found for app {}. Is the game installed?", app_id)
            })?;
            let mounted = parse_mounted_depots(&acf_path)?;
            if mounted.is_empty() {
                return Err(format!(
                    "No mounted depots found in ACF for app {}",
                    app_id
                ));
            }
            let mut count = 0u32;
            for (depot_id, manifest_id) in &mounted {
                lua_writer::set_manifest_pin(&lua_path, *depot_id, manifest_id, true, None)?;
                count += 1;
            }
            Ok(format!(
                "Pinned {} depot(s) to current version for app {}",
                count, app_id
            ))
        }
        "latest" => {
            let app_info = fetch_app_depot_info(app_id)?;
            let mut count = 0u32;
            for depot in &app_info.depots {
                if let Some(ref manifest_id) = depot.public_manifest_id {
                    lua_writer::set_manifest_pin(
                        &lua_path,
                        depot.depot_id,
                        manifest_id,
                        true,
                        Some(depot.size),
                    )?;
                    count += 1;
                }
            }
            if count == 0 {
                return Err(format!("No latest manifests available for app {}", app_id));
            }
            Ok(format!(
                "Pinned {} depot(s) to latest version for app {}",
                count, app_id
            ))
        }
        other => Err(format!("Unknown pin mode: {}", other)),
    }
}

fn unpin_op(app_id: u64) -> Result<String, String> {
    let lua_dir = lua_dir()?;
    if app_id == 0 {
        let mut total_count = 0u32;
        let mut total_games = 0u32;
        let entries =
            fs::read_dir(&lua_dir).map_err(|e| format!("Failed to read lua dir: {e}"))?;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("lua") {
                if let Ok(count) = lua_writer::unpin_all_manifests(&path) {
                    if count > 0 {
                        total_count += count;
                        total_games += 1;
                    }
                }
            }
        }
        return Ok(format!(
            "Unpinned {} manifest(s) across {} game(s)",
            total_count, total_games
        ));
    }
    let lua_path = lua_dir.join(format!("{}.lua", app_id));
    if !lua_path.exists() {
        return Err(format!("No Lua file found for app {}", app_id));
    }
    let count = lua_writer::unpin_all_manifests(&lua_path)?;
    Ok(format!(
        "Unpinned {} manifest(s) for app {}",
        count, app_id
    ))
}

fn pins_aggregate() -> Value {
    let mut map = serde_json::Map::new();
    let Ok(lua_dir) = lua_dir() else {
        return json!({"ok": true, "pins": {}});
    };
    let root = steam_root().ok();
    if let Ok(entries) = fs::read_dir(&lua_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("lua") {
                continue;
            }
            let app_id = match path.file_stem().and_then(|s| s.to_str()) {
                Some(s) if !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()) => s.to_string(),
                _ => continue,
            };
            let has_pins = lua_writer::has_active_pins(&path);
            let installed = match (&root, app_id.parse::<u64>()) {
                (Some(root), Ok(id)) => find_acf_file(root, id).is_some(),
                _ => false,
            };
            map.insert(app_id, json!({"hasPins": has_pins, "installed": installed}));
        }
    }
    json!({"ok": true, "pins": map})
}

// ---------------------------------------------------------------------------
// Download job — same shape as package_installer so the frontend poll works
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize)]
struct SteamKeysJob {
    status: String,
    progress: u8,
    message: String,
    error: Option<String>,
    app_id: String,
    lua_count: u32,
    manifest_count: u32,
}

fn jobs() -> &'static RwLock<HashMap<String, SteamKeysJob>> {
    static INSTANCE: OnceLock<RwLock<HashMap<String, SteamKeysJob>>> = OnceLock::new();
    INSTANCE.get_or_init(|| RwLock::new(HashMap::new()))
}

fn set_job(request_id: &str, job: SteamKeysJob) {
    if let Ok(mut map) = jobs().write() {
        map.insert(request_id.to_string(), job);
    }
}

fn update_job<F: FnOnce(&mut SteamKeysJob)>(request_id: &str, f: F) {
    if let Ok(mut map) = jobs().write() {
        if let Some(job) = map.get_mut(request_id) {
            f(job);
        }
    }
}

fn status_response(request_id: &str) -> (u16, String) {
    let map = jobs().read().unwrap();
    match map.get(request_id) {
        Some(job) => (
            200,
            json!({
                "ok": true,
                "status": job.status,
                "progress": job.progress,
                "message": job.message,
                "errorCode": job.error,
                "appId": job.app_id,
                "luaCount": job.lua_count,
                "manifestCount": job.manifest_count
            })
            .to_string(),
        ),
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

fn fail_job(request_id: &str, code: &str, message: &str) {
    update_job(request_id, |job| {
        job.status = "failed".to_string();
        job.progress = 100;
        job.message = message.to_string();
        job.error = Some(code.to_string());
    });
    crate::log_to_temp(&format!(
        "[steam-keys] job {} failed ({}): {}",
        request_id, code, message
    ));
}

fn start_steamkeys_job(app_id: u64) -> String {
    let request_id = format!(
        "{}-{}-{}",
        app_id,
        cache::now_secs(),
        rand::random::<u16>()
    );
    let auto_fetch = auto_fetch_enabled();

    set_job(
        &request_id,
        SteamKeysJob {
            status: "queued".to_string(),
            progress: 0,
            message: "Queued".to_string(),
            error: None,
            app_id: app_id.to_string(),
            lua_count: 0,
            manifest_count: 0,
        },
    );

    let rid = request_id.clone();
    std::thread::spawn(move || {
        // 1. Key cache
        update_job(&rid, |job| {
            job.status = "downloading".to_string();
            job.progress = 10;
            job.message = "Updating key cache...".to_string();
        });
        if let Err(e) = cache::ensure_key_files() {
            // Cache failure is non-fatal only if we already have files to load
            if cache::load_depot_keys().is_err() {
                fail_job(&rid, "CACHE_ERROR", &e);
                return;
            }
        }

        // 2. Generate .lua
        update_job(&rid, |job| {
            job.status = "processing".to_string();
            job.progress = 30;
            job.message = "Generating .lua file...".to_string();
        });
        if let Err(e) = generate_lua_for_app(app_id) {
            let code = if e.contains("depot info") || e.contains("API error") {
                "STEAMCMD_ERROR"
            } else {
                "LUA_ERROR"
            };
            fail_job(&rid, code, &e);
            return;
        }
        update_job(&rid, |job| {
            job.lua_count = 1;
            job.progress = 45;
            job.message = "Adding DLCs...".to_string();
        });

        // 3. Add all DLCs (warn only)
        let mut warnings: Vec<String> = Vec::new();
        match add_all_dlcs(app_id) {
            Ok((added, skipped)) => {
                if added > 0 {
                    crate::log_to_temp(&format!(
                        "[steam-keys] app {}: added {} DLCs ({} already present)",
                        app_id, added, skipped
                    ));
                }
            }
            Err(e) => warnings.push(format!("DLCs: {}", e)),
        }

        // 4. Auto-fetch manifests (warn only)
        if auto_fetch {
            update_job(&rid, |job| {
                job.progress = 60;
                job.message = "Fetching manifests...".to_string();
            });
            match fetch_manifests_op(app_id) {
                Ok((count, saved)) => {
                    update_job(&rid, |job| {
                        job.manifest_count = saved as u32;
                    });
                    crate::log_to_temp(&format!(
                        "[steam-keys] app {}: fetched {} manifests, {} saved to depotcache",
                        app_id, count, saved
                    ));
                    if count == 0 {
                        warnings.push("No manifests found in repository".to_string());
                    }
                }
                Err(e) => {
                    warnings.push(format!("Fetch: {}", e));
                    crate::log_to_temp(&format!(
                        "[steam-keys] app {}: manifest fetch failed: {}",
                        app_id, e
                    ));
                }
            }
        }

        // 5. Complete
        let mut message = format!("Steam Keys setup complete for app {}", app_id);
        if !warnings.is_empty() {
            message = format!("{} ({})", message, warnings.join("; "));
        }
        update_job(&rid, |job| {
            job.status = "completed".to_string();
            job.progress = 100;
            job.message = message.clone();
        });
        crate::log_to_temp(&format!("[steam-keys] {}", message));
    });

    request_id
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

fn body_app_id(body: &str) -> Option<(u64, Option<String>)> {
    let v: Value = serde_json::from_str(body).ok()?;
    let app = v.get("appId").or_else(|| v.get("app_id"))?;
    let app_id = app
        .as_u64()
        .or_else(|| app.as_str().and_then(|s| s.parse::<u64>().ok()))?;
    let source = v
        .get("sourceId")
        .or_else(|| v.get("source_id"))
        .and_then(|s| s.as_str())
        .map(|s| s.to_string());
    Some((app_id, source))
}

fn parse_app_id_from_value(v: &Value) -> Option<u64> {
    v.get("appId")
        .or_else(|| v.get("app_id"))
        .and_then(|a| {
            a.as_u64()
                .or_else(|| a.as_str().and_then(|s| s.parse::<u64>().ok()))
        })
}

pub fn try_handle_route(method: &str, path: &str, body: &str) -> Option<(u16, String)> {
    // Intercept POST /api/download for the steamkeys source only
    if method == "POST" && path == "/api/download" {
        if let Some((app_id, source)) = body_app_id(body) {
            if source.as_deref() == Some("steamkeys") {
                let request_id = start_steamkeys_job(app_id);
                return Some((200, json!({"ok": true, "requestId": request_id}).to_string()));
            }
        }
        return None;
    }

    // Serve download-status for our own jobs (so the standard frontend poll
    // works unchanged); fall through to package_installer/Lua otherwise.
    if method == "GET" && path.starts_with("/api/download-status/") {
        let id = path.trim_start_matches("/api/download-status/");
        let known = jobs().read().map(|m| m.contains_key(id)).unwrap_or(false);
        if known {
            return Some(status_response(id));
        }
        return None;
    }

    if path.starts_with("/api/steam-keys/status/") && method == "GET" {
        let id = path.trim_start_matches("/api/steam-keys/status/");
        return Some(status_response(id));
    }

    if path == "/api/steam-keys/pins" && method == "GET" {
        return Some((200, pins_aggregate().to_string()));
    }

    if path == "/api/steam-keys/pin" && method == "POST" {
        let Ok(v) = serde_json::from_str::<Value>(body) else {
            return Some((
                400,
                json!({"ok": false, "message": "Invalid JSON"}).to_string(),
            ));
        };
        let Some(app_id) = parse_app_id_from_value(&v) else {
            return Some((
                400,
                json!({"ok": false, "message": "Missing appId"}).to_string(),
            ));
        };
        let mode = v
            .get("mode")
            .and_then(|m| m.as_str())
            .unwrap_or("latest")
            .to_string();
        return Some(match pin_op(app_id, &mode) {
            Ok(msg) => (200, json!({"ok": true, "message": msg}).to_string()),
            Err(e) => (200, json!({"ok": false, "message": e}).to_string()),
        });
    }

    if path == "/api/steam-keys/unpin" && method == "POST" {
        let Ok(v) = serde_json::from_str::<Value>(body) else {
            return Some((
                400,
                json!({"ok": false, "message": "Invalid JSON"}).to_string(),
            ));
        };
        let app_id = parse_app_id_from_value(&v).unwrap_or(0);
        return Some(match unpin_op(app_id) {
            Ok(msg) => (200, json!({"ok": true, "message": msg}).to_string()),
            Err(e) => (200, json!({"ok": false, "message": e}).to_string()),
        });
    }

    if path == "/api/steam-keys/settings" {
        if method == "GET" {
            let cfg = read_config();
            let sec = cfg.get("steamKeys");
            return Some((
                200,
                json!({
                    "ok": true,
                    "autoFetch": sec
                        .and_then(|s| s.get("autoFetch"))
                        .and_then(|v| v.as_bool())
                        .unwrap_or(true),
                    "manifestRepo": sec
                        .and_then(|s| s.get("manifestRepo"))
                        .and_then(|v| v.as_str())
                        .unwrap_or(DEFAULT_MANIFEST_REPO)
                })
                .to_string(),
            ));
        }
        if method == "POST" {
            let Ok(v) = serde_json::from_str::<Value>(body) else {
                return Some((
                    400,
                    json!({"ok": false, "message": "Invalid JSON"}).to_string(),
                ));
            };
            return Some(match save_steam_keys_settings(&v) {
                Ok(()) => (200, json!({"ok": true}).to_string()),
                Err(e) => (200, json!({"ok": false, "message": e}).to_string()),
            });
        }
    }

    None
}
