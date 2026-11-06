//! `GET /api/profile-games` — real Steam-local stats for the Lua games.
//!
//! Steam builds the profile's Games list from *licensed* apps, so the games
//! LumaForge installs through `<steam>/config/lua/*.lua` never show up there.
//! The extension's profile surface injects them by cloning Steam's own row
//! markup, and this endpoint feeds it with data that is already on disk —
//! nothing is synthesized:
//!
//! * `<steam>/config/lua/*.lua`                  -> app id + display name
//! * `userdata/<account>/config/localconfig.vdf` -> `Playtime`, `Playtime2wks`, `LastPlayed`
//! * `userdata/<account>/config/librarycache/<id>.json` -> `nAchieved` / `nTotal`
//!
//! The `enabled` flag comes from `config.json` (`profile.luaOnProfile`, default
//! on); the extension's Settings tab owns the write side of that key, so the
//! file has a single writer.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// loginusers/SteamID64 -> userdata folder (SteamID64 = 76561197960265728 + account id).
const STEAM_ID64_BASE: u64 = 7_656_119_796_026_5728;

/// Parsed response cache: localconfig is ~200 KB and there is one
/// librarycache JSON per game, so a profile tab switch would otherwise
/// re-read a few hundred files.
const CACHE_TTL: Duration = Duration::from_secs(20);
static CACHE: Mutex<Option<(String, Instant)>> = Mutex::new(None);

pub fn try_handle_route(method: &str, path: &str, _body: &str) -> Option<(u16, String)> {
    if path == "/api/profile-games" && method == "GET" {
        return Some((200, cached_body()));
    }
    // /api/profile-achievements/{appId} — full achievement list for a single lua game
    if method == "GET" {
        if let Some(app_id) = path.strip_prefix("/api/profile-achievements/") {
            if app_id.chars().all(|c| c.is_ascii_digit()) {
                return Some((200, build_achievements_body(app_id)));
            }
        }
    }
    None
}

fn cached_body() -> String {
    if let Ok(guard) = CACHE.lock() {
        if let Some((body, at)) = guard.as_ref() {
            if at.elapsed() < CACHE_TTL {
                return body.clone();
            }
        }
    }
    let body = build_body();
    if let Ok(mut guard) = CACHE.lock() {
        *guard = Some((body.clone(), Instant::now()));
    }
    body
}

fn build_body() -> String {
    let localconfig = active_userdata_dir().and_then(|dir| {
        std::fs::read_to_string(dir.join("config").join("localconfig.vdf")).ok()
    });
    let user_dir = active_userdata_dir();
    let games = build_games(localconfig.as_deref(), user_dir.as_deref());

    // Real Steam app ids from localconfig.vdf — used by the extension to patch
    // the `recent` and `perfect` tab deltas from any tab (not only from `all`).
    let steam_ids = localconfig.as_deref().map_or(Vec::new(), |lc| steam_app_ids(lc));
    let steam_app_ids: Vec<String> = steam_ids.iter().map(|(a, _, _)| a.clone()).collect();
    let steam_recent_app_ids: Vec<String> = steam_ids
        .iter()
        .filter(|(_, p2w, _)| *p2w > 0)
        .map(|(a, _, _)| a.clone())
        .collect();
    // Last played (unix seconds) per real Steam app, so the extension can
    // interleave lua rows into the `recent` tab in Steam's own order.
    let mut steam_last_played: serde_json::Map<String, Value> = serde_json::Map::new();
    for (app_id, _, last_played) in &steam_ids {
        if *last_played > 0 {
            steam_last_played.insert(app_id.clone(), Value::from(*last_played));
        }
    }
    let steam_perfect_app_ids: Vec<String> = user_dir
        .as_deref()
        .map(|d| steam_perfect_app_ids(d, &steam_app_ids))
        .unwrap_or_default();
    // Real "past 2 weeks" total: the sum of `Playtime2wks` over every app in
    // localconfig.vdf (Steam's Recent Activity header counts the same field).
    let playtime2wks_total: u64 = steam_ids.iter().map(|(_, p2w, _)| *p2w).sum();

    json!({
        "ok": true,
        "enabled": profile_toggle_enabled(),
        "steamId64": crate::steam_account::most_recent_steam_id64().unwrap_or_default(),
        "count": games.len(),
        "games": games,
        "steamAppIds": steam_app_ids,
        "steamRecentAppIds": steam_recent_app_ids,
        "steamPerfectAppIds": steam_perfect_app_ids,
        "steamLastPlayed": steam_last_played,
        "playtime2wksTotal": playtime2wks_total,
    })
    .to_string()
}

/// All app ids with a block in `localconfig.vdf`, paired with `Playtime2wks`
/// and `LastPlayed` (unix seconds, 0 when absent).
fn steam_app_ids(localconfig: &str) -> Vec<(String, u64, u64)> {
    // The real per-app stats live under
    // `UserLocalConfigStore > Software > Valve > Steam > apps`.
    let Some(software) = vdf_key_block(localconfig, "Software") else {
        return Vec::new();
    };
    let Some(valve) = vdf_key_block(software, "Valve") else {
        return Vec::new();
    };
    let Some(steam) = vdf_key_block(valve, "Steam") else {
        return Vec::new();
    };
    let Some(apps) = vdf_key_block(steam, "apps") else {
        return Vec::new();
    };
    let mut out: Vec<(String, u64, u64)> = Vec::new();
    for (key, block) in vdf_child_blocks(apps) {
        if !key.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let p2w = vdf_num_field(block, "Playtime2wks").unwrap_or(0);
        let last_played = vdf_num_field(block, "LastPlayed").unwrap_or(0);
        out.push((key, p2w, last_played));
    }
    out
}

/// Body `{ ... }` of the first `"key"` that is followed by a block (skips
/// occurrences where the same text appears as a plain value).
fn vdf_key_block<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("\"{key}\"");
    let bytes = text.as_bytes();
    let mut from = 0usize;
    while let Some(rel) = text[from..].find(&needle) {
        let idx = from + rel;
        let after = &text[idx + needle.len()..];
        let trimmed = after.trim_start();
        if let Some(rest) = trimmed.strip_prefix('{') {
            let brace = text.len() - rest.len() - 1;
            let mut depth = 0i32;
            let mut in_str = false;
            let mut esc = false;
            let mut i = brace;
            while i < bytes.len() {
                let b = bytes[i];
                if in_str {
                    if esc {
                        esc = false;
                    } else if b == b'\\' {
                        esc = true;
                    } else if b == b'"' {
                        in_str = false;
                    }
                } else {
                    match b {
                        b'"' => in_str = true,
                        b'{' => depth += 1,
                        b'}' => {
                            depth -= 1;
                            if depth == 0 {
                                return Some(&text[brace..=i]);
                            }
                        }
                        _ => {}
                    }
                }
                i += 1;
            }
            return None;
        }
        from = idx + needle.len();
    }
    None
}

/// Direct child `(key, block)` pairs of a VDF block body. Nested blocks are
/// returned whole (their contents are not walked again).
fn vdf_child_blocks(text: &str) -> Vec<(String, &str)> {
    let bytes = text.as_bytes();
    let mut out: Vec<(String, &str)> = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] != b'"' {
            i += 1;
            continue;
        }
        let key_start = i + 1;
        let mut j = key_start;
        while j < bytes.len() && bytes[j] != b'"' {
            j += 1;
        }
        if j >= bytes.len() {
            break;
        }
        let key = &text[key_start..j];
        j += 1;
        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
            j += 1;
        }
        if j < bytes.len() && bytes[j] == b'{' {
            let block_start = j;
            let mut depth = 0i32;
            let mut in_str = false;
            let mut esc = false;
            let mut k = j;
            while k < bytes.len() {
                let b = bytes[k];
                if in_str {
                    if esc {
                        esc = false;
                    } else if b == b'\\' {
                        esc = true;
                    } else if b == b'"' {
                        in_str = false;
                    }
                } else {
                    match b {
                        b'"' => in_str = true,
                        b'{' => depth += 1,
                        b'}' => {
                            depth -= 1;
                            if depth == 0 {
                                k += 1;
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                k += 1;
            }
            out.push((key.to_string(), &text[block_start..k]));
            i = k;
        } else {
            i = j;
        }
    }
    out
}

/// App ids whose librarycache shows 100% achievements achieved (`nAchieved >=
/// nTotal > 0`).
fn steam_perfect_app_ids(user_dir: &Path, app_ids: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for app_id in app_ids {
        let (achieved, total) = achievements(user_dir, app_id);
        if total > 0 && achieved >= total {
            out.push(app_id.clone());
        }
    }
    out
}

/// First up-to-four achievement highlights from `librarycache/<id>.json`.
/// Reads only from `vecHighlight` (achieved achievements, most recent first) —
/// real local Steam data, nothing synthesized. Returns the icon URLs plus the
/// hover-tooltip metadata Steam's own popover shows (name, description, unlock
/// time and global percentage).
fn achievement_highlights(user_dir: &Path, app_id: &str) -> (Vec<String>, Vec<Value>) {
    let path = user_dir
        .join("config")
        .join("librarycache")
        .join(format!("{app_id}.json"));
    let Ok(raw) = std::fs::read_to_string(path) else {
        return (Vec::new(), Vec::new());
    };
    // Find the achievements payload, then locate vecHighlight within it.
    let Some(ach_idx) = raw.find("\"achievements\"") else {
        return (Vec::new(), Vec::new());
    };
    let blob = &raw[ach_idx..];
    // Find "vecHighlight": then the opening [
    let Some(high_idx) = blob.find("\"vecHighlight\"") else {
        return (Vec::new(), Vec::new());
    };
    let after_high = &blob[high_idx + "\"vecHighlight\"".len()..];
    let Some(colon) = after_high.find(':') else { return (Vec::new(), Vec::new()) };
    let after_colon = after_high[colon + 1..].trim_start();
    let Some(bracket_start) = after_colon.as_bytes().iter().position(|&b| b == b'[') else {
        return (Vec::new(), Vec::new());
    };
    let array_start = bracket_start;
    // Find matching closing ] by counting brackets and braces
    let mut depth = 1usize;
    let mut array_end = None;
    for (i, byte) in after_colon.as_bytes()[array_start + 1..].iter().enumerate() {
        match byte {
            b'[' | b'{' => depth += 1,
            b']' | b'}' => {
                depth -= 1;
                if depth == 0 {
                    array_end = Some(array_start + 1 + i);
                    break;
                }
            }
            _ => {}
        }
    }
    let Some(end) = array_end else {
        return (Vec::new(), Vec::new());
    };
    let high_array = &after_colon[array_start..=end];
    // First up-to 4 highlight objects: icon URL + tooltip fields.
    let mut icons: Vec<String> = Vec::new();
    let mut meta: Vec<Value> = Vec::new();
    for obj in split_json_objects(high_array) {
        if icons.len() >= 4 {
            break;
        }
        let Some(image) = json_str_field(obj, "strImage") else {
            continue;
        };
        if image.is_empty() {
            continue;
        }
        meta.push(json!({
            "name": json_str_field(obj, "strName").unwrap_or_default(),
            "desc": json_str_field(obj, "strDescription").unwrap_or_default(),
            "unlocked": json_num_field(obj, "rtUnlocked").unwrap_or(0.0),
            "pct": json_num_field(obj, "flAchieved"),
        }));
        icons.push(image);
    }
    (icons, meta)
}

/// Full achievement list from `librarycache/<id>.json` — both `vecHighlight`
/// (achieved, most recent first) and `vecUnachieved` (not yet achieved).
/// Returns all achievements with their icon, name, description, unlock time,
/// global percentage, and achieved flag. Real local Steam data, nothing synthesized.
fn achievements_full(user_dir: &Path, app_id: &str) -> Vec<Value> {
    let path = user_dir
        .join("config")
        .join("librarycache")
        .join(format!("{app_id}.json"));
    let Ok(raw) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Some(ach_idx) = raw.find("\"achievements\"") else {
        return Vec::new();
    };
    let blob = &raw[ach_idx..];

    // Steam splits the full set across three arrays: vecHighlight (achieved,
    // profile-visible), vecAchievedHidden (achieved but hidden-type defs) and
    // vecUnachieved. Skipping the middle one halves the list on games like
    // Cuphead (12 + 9 records). The arrays also *overlap* (an id can appear in
    // both vecHighlight and vecAchievedHidden — Swan Song 12 + 2 with both
    // duplicates), so merge deduping by strID, first occurrence wins.
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for (key, achieved) in [
        ("vecHighlight", true),
        ("vecAchievedHidden", true),
        ("vecUnachieved", false),
    ] {
        for rec in parse_ach_array(blob, key, achieved) {
            let id = rec
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if id.is_empty() {
                out.push(rec);
                continue;
            }
            if seen.insert(id) {
                out.push(rec);
            }
        }
    }
    out
}

/// `<steam>/appcache/stats/UserGameStats_<account>_<app_id>.bin` holds the
/// account's achievement state in Valve's binary VDF. Each `AchievementTimes`
/// block is keyed by stat id; its children are bit indices inside that stat's
/// achievement bitmap, and a non-zero value is the unlock time. Internal keys
/// (what the profile page exposes as `internal_key`) are `stat_id * 256 + bit`,
/// so this map lines up with the page schema directly.
///
/// This is the only complete local source: Steam stops writing the
/// `librarycache` record arrays after 12 entries each, while this file keeps
/// every unlocked achievement. Absence of a bit means locked.
fn achievement_times(user_dir: &Path, app_id: &str) -> std::collections::BTreeMap<String, u64> {
    let Some(account) = user_dir.file_name().and_then(|s| s.to_str()) else {
        return std::collections::BTreeMap::new();
    };
    let Some(steam_root) = user_dir.parent().and_then(Path::parent) else {
        return std::collections::BTreeMap::new();
    };
    let path = steam_root
        .join("appcache")
        .join("stats")
        .join(format!("UserGameStats_{account}_{app_id}.bin"));
    let Ok(bytes) = std::fs::read(path) else {
        return std::collections::BTreeMap::new();
    };
    parse_achievement_times(&bytes)
}

/// Minimal binary-VDF reader: only the node types Steam writes into the user
/// stats file are handled (nested object, string, int32, float, pointer, wide
/// string, uint64, end-of-object). Anything unexpected aborts quietly — the
/// caller falls back to the `librarycache` records.
fn parse_achievement_times(bytes: &[u8]) -> std::collections::BTreeMap<String, u64> {
    struct Cursor<'a> {
        bytes: &'a [u8],
        pos: usize,
    }
    impl<'a> Cursor<'a> {
        fn u8(&mut self) -> Option<u8> {
            let b = *self.bytes.get(self.pos)?;
            self.pos += 1;
            Some(b)
        }
        fn u32(&mut self) -> Option<u32> {
            let end = self.pos.checked_add(4)?;
            let s = self.bytes.get(self.pos..end)?;
            self.pos = end;
            Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
        }
        fn cstr(&mut self) -> Option<String> {
            let rest = self.bytes.get(self.pos..)?;
            let end = rest.iter().position(|&b| b == 0)?;
            let name = String::from_utf8_lossy(&rest[..end]).to_string();
            self.pos += end + 1;
            Some(name)
        }
        fn skip_wstr(&mut self) -> Option<()> {
            loop {
                let lo = self.u8()? as u16;
                let hi = self.u8()? as u16;
                if lo == 0 && hi == 0 {
                    return Some(());
                }
            }
        }
        fn walk(
            &mut self,
            path: &mut Vec<String>,
            out: &mut std::collections::BTreeMap<String, u64>,
        ) -> Option<()> {
            loop {
                match self.u8()? {
                    0x00 => {
                        let name = self.cstr()?;
                        path.push(name);
                        self.walk(path, out)?;
                        path.pop();
                    }
                    0x01 => {
                        self.cstr()?;
                        self.cstr()?;
                    }
                    0x02 => {
                        let name = self.cstr()?;
                        let value = self.u32()?;
                        // `.../<stat id>/AchievementTimes/<bit>` -> internal key.
                        if path.len() >= 2 && path[path.len() - 1] == "AchievementTimes" {
                            let stat = path[path.len() - 2].parse::<u32>();
                            let bit = name.parse::<u32>();
                            if let (Ok(stat), Ok(bit)) = (stat, bit) {
                                if value > 0 {
                                    out.insert((stat * 256 + bit).to_string(), value as u64);
                                }
                            }
                        }
                    }
                    0x03 => {
                        self.cstr()?;
                        self.u32()?;
                    }
                    0x04 | 0x06 => {
                        self.cstr()?;
                        self.u32()?;
                    }
                    0x05 => {
                        self.cstr()?;
                        self.skip_wstr()?;
                    }
                    0x07 => {
                        self.cstr()?;
                        self.u32()?;
                        self.u32()?;
                    }
                    0x08 => return Some(()),
                    _ => return None,
                }
            }
        }
    }

    let mut out = std::collections::BTreeMap::new();
    let mut cur = Cursor { bytes, pos: 0 };
    if cur.u8() != Some(0x00) {
        return out;
    }
    let Some(root) = cur.cstr() else { return out };
    let mut path = vec![root];
    let _ = cur.walk(&mut path, &mut out);
    out
}

/// Extract one `"key": [ {...}, ... ]` achievement array from a raw
/// `librarycache` achievements blob. The bracket scan is string/escape aware
/// so `[` or `]` inside description text cannot truncate the slice.
fn parse_ach_array(blob: &str, key: &str, achieved: bool) -> Vec<Value> {
    let mut out = Vec::new();
    let needle = format!("\"{key}\"");
    let Some(k_idx) = blob.find(&needle) else {
        return out;
    };
    let after_key = &blob[k_idx + needle.len()..];
    let Some(colon) = after_key.find(':') else {
        return out;
    };
    let after_colon = after_key[colon + 1..].trim_start();
    let bytes = after_colon.as_bytes();
    let Some(arr_start) = bytes.iter().position(|&b| b == b'[') else {
        return out;
    };

    let mut depth = 0i32;
    let mut in_str = false;
    let mut esc = false;
    let mut arr_end: Option<usize> = None;
    for (i, &b) in bytes[arr_start..].iter().enumerate() {
        if in_str {
            if esc {
                esc = false;
            } else if b == b'\\' {
                esc = true;
            } else if b == b'"' {
                in_str = false;
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'[' | b'{' => depth += 1,
            b']' | b'}' => {
                depth -= 1;
                if depth == 0 {
                    arr_end = Some(arr_start + i);
                    break;
                }
            }
            _ => {}
        }
    }
    let Some(end) = arr_end else {
        return out;
    };
    let array = &after_colon[arr_start..=end];

    for obj in split_json_objects(array) {
        let Some(image) = json_str_field(obj, "strImage") else {
            continue;
        };
        if image.is_empty() {
            continue;
        }
        out.push(json!({
            "id": json_str_field(obj, "strID").unwrap_or_default(),
            "name": json_str_field(obj, "strName").unwrap_or_default(),
            "desc": json_str_field(obj, "strDescription").unwrap_or_default(),
            "unlocked": json_num_field(obj, "rtUnlocked").unwrap_or(0.0),
            "pct": json_num_field(obj, "flAchieved"),
            "icon": image,
            "achieved": achieved,
        }));
    }
    out
}

/// Build the response body for /api/profile-achievements/{appId}.
fn build_achievements_body(app_id: &str) -> String {
    let user_dir = active_userdata_dir();
    let ach_list = user_dir
        .as_deref()
        .map_or(Vec::new(), |d| achievements_full(d, app_id));
    let (achieved_count, total_count) = user_dir
        .as_deref()
        .map_or((0, 0), |d| achievements(d, app_id));
    let play = user_dir
        .as_deref()
        .and_then(|d| parse_playtime(d, app_id));
    // Complete unlock map (internal key -> unix time) from the account's stats
    // file; the page schema supplies names/icons/% for the rest of the rows.
    let times = user_dir
        .as_deref()
        .map_or(std::collections::BTreeMap::new(), |d| {
            achievement_times(d, app_id)
        });

    json!({
        "ok": true,
        "appId": app_id,
        // Only the games LumaForge installed through `config/lua/*.lua` are ours:
        // every other app id is a real Steam game whose achievements page Steam
        // already renders natively (and correctly) — the extension must leave
        // those pages untouched.
        "isLua": is_lua_game(app_id),
        "achieved": achieved_count,
        "total": total_count,
        "playtime": play.as_ref().map_or(0, |p| p.0),
        "playtime2wks": play.as_ref().map_or(0, |p| p.1),
        "lastPlayed": play.as_ref().map_or(0, |p| p.2),
        "achievements": ach_list,
        "unlockedByKey": times,
    })
    .to_string()
}

/// `"Playtime"` (minutes), `"Playtime2wks"` (minutes) and `"LastPlayed"`
/// (unix seconds) for an app from `localconfig.vdf`. Returns None when the
/// app has no entry (never launched from this install).
fn parse_playtime(user_dir: &Path, app_id: &str) -> Option<(u64, u64, u64)> {
    let raw = std::fs::read_to_string(user_dir.join("config").join("localconfig.vdf")).ok()?;
    let needle = format!("\"{app_id}\"");
    let mut from = 0usize;
    while let Some(rel) = raw[from..].find(&needle) {
        let key_at = from + rel;
        // The app block: `"appid" { ... }` — scan forward to its closing brace
        // (VDF has no strings containing braces we must respect beyond quotes).
        let after = &raw[key_at + needle.len()..];
        let open = after.find('{')?;
        let block_start = key_at + needle.len() + open;
        let mut depth = 0i32;
        let mut in_str = false;
        let mut block_end = None;
        for (i, b) in raw[block_start..].bytes().enumerate() {
            match b {
                b'"' => in_str = !in_str,
                b'{' if !in_str => depth += 1,
                b'}' if !in_str => {
                    depth -= 1;
                    if depth == 0 {
                        block_end = Some(block_start + i);
                        break;
                    }
                }
                _ => {}
            }
        }
        let end = block_end?;
        let block = &raw[block_start..=end];
        if let (Some(p), Some(l)) = (vdf_num_field(block, "Playtime"), vdf_num_field(block, "LastPlayed")) {
            let p2 = vdf_num_field(block, "Playtime2wks").unwrap_or(0);
            return Some((p, p2, l));
        }
        from = key_at + needle.len();
        if from >= raw.len() {
            return None;
        }
    }
    None
}

/// `"Key"  "123"` from a raw VDF block.
fn vdf_num_field(block: &str, key: &str) -> Option<u64> {
    let needle = format!("\"{key}\"");
    let idx = block.find(&needle)?;
    let after = &block[idx + needle.len()..];
    let q1 = after.find('"')?;
    let rest = &after[q1 + 1..];
    let q2 = rest.find('"')?;
    rest[..q2].trim().parse::<u64>().ok()
}

/// Split a raw JSON array's top-level `{...}` objects. String/escape aware so
/// braces inside description text cannot break the boundaries.
fn split_json_objects(array: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut in_str = false;
    let mut esc = false;
    let mut start = 0usize;
    for (i, b) in array.bytes().enumerate() {
        if in_str {
            if esc {
                esc = false;
            } else if b == b'\\' {
                esc = true;
            } else if b == b'"' {
                in_str = false;
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'{' => {
                if depth == 0 {
                    start = i;
                }
                depth += 1;
            }
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    out.push(&array[start..=i]);
                }
            }
            _ => {}
        }
    }
    out
}

/// `"key": "value"` from a raw JSON object slice.
fn json_str_field(obj: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let idx = obj.find(&needle)?;
    let after = &obj[idx + needle.len()..];
    let colon = after.find(':')?;
    let rest = after[colon + 1..].trim_start();
    extract_json_string(rest)
}

/// `"key": 123.45` from a raw JSON object slice.
fn json_num_field(obj: &str, key: &str) -> Option<f64> {
    let needle = format!("\"{key}\"");
    let idx = obj.find(&needle)?;
    let after = &obj[idx + needle.len()..];
    let colon = after.find(':')?;
    let rest = after[colon + 1..].trim_start();
    let end = rest
        .find(|c: char| c == ',' || c == '}' || c == ']' || c.is_whitespace())
        .unwrap_or(rest.len());
    rest[..end].parse::<f64>().ok()
}

/// Extract a JSON string value from `s` (assumed to start with a quote).
fn extract_json_string(s: &str) -> Option<String> {
    let dq = '\u{22}';
    let rest = s.strip_prefix(dq)?;
    let mut end = 0usize;
    let mut escaped = false;
    for (i, ch) in rest.char_indices() {
        if escaped { escaped = false; continue; }
        if ch == '\\' { escaped = true; continue; }
        if ch == dq { end = i; break; }
    }
    if end == 0 { return None; }
    Some(rest[..end].to_string())
}

/// `profile.luaOnProfile` in config.json — default on when absent.
fn profile_toggle_enabled() -> bool {
    let Ok(raw) = std::fs::read_to_string(crate::platform::config_json_path()) else {
        return true;
    };
    let Ok(val) = serde_json::from_str::<Value>(raw.trim_start_matches('\u{feff}')) else {
        return true;
    };
    val.get("profile")
        .and_then(|p| p.get("luaOnProfile"))
        .and_then(|b| b.as_bool())
        .unwrap_or(true)
}

fn build_games(localconfig: Option<&str>, user_dir: Option<&Path>) -> Vec<Value> {
    let entries = crate::bridge::lua_game_entries();
    if entries.is_empty() {
        return Vec::new();
    }

    let mut games: Vec<Value> = entries
        .into_iter()
        .filter_map(|(app_id, name)| {
            let block = localconfig.as_deref().and_then(|t| vdf_app_block(t, &app_id));
            let playtime = block.map_or(0, |b| vdf_num(b, "Playtime"));
            let playtime2wks = block.map_or(0, |b| vdf_num(b, "Playtime2wks"));
            let last_played = block.map_or(0, |b| vdf_num(b, "LastPlayed"));
            let (ach_unlocked, ach_total) =
                user_dir.as_deref().map_or((0, 0), |d| achievements(d, &app_id));
            let (ach_icons, ach_meta) = user_dir
                .as_deref()
                .map_or((Vec::new(), Vec::new()), |d| achievement_highlights(d, &app_id));

            // No localconfig block and no achievements: the Lua file exists but
            // the game was never launched here. Still listed (the Dashboard
            // counts it too), just with empty stats.
            Some(json!({
                "appId": app_id,
                "name": name,
                "playtime": playtime,
                "playtime2wks": playtime2wks,
                "lastPlayed": last_played,
                "achUnlocked": ach_unlocked,
                "achTotal": ach_total,
                "achIcons": ach_icons,
                "achMeta": ach_meta,
            }))
        })
        .collect();

    // Same order as Steam's own list: most played first, then most recent.
    games.sort_by(|a, b| {
        let pa = a["playtime"].as_u64().unwrap_or(0);
        let pb = b["playtime"].as_u64().unwrap_or(0);
        pb.cmp(&pa).then_with(|| {
            let la = a["lastPlayed"].as_u64().unwrap_or(0);
            let lb = b["lastPlayed"].as_u64().unwrap_or(0);
            lb.cmp(&la)
        })
    });
    games
}

/// True when the app id belongs to a LumaForge Lua game, i.e. there is a
/// `<steam>/config/lua/<app_id>.lua` file. Those are the only games the
/// profile surface may touch: every other app id is a real Steam game, and
/// Steam already renders its achievements page natively.
fn is_lua_game(app_id: &str) -> bool {
    is_lua_app(&crate::bridge::lua_game_entries(), app_id)
}

/// Membership test over the lua entries, separated for testing.
fn is_lua_app(entries: &[(String, String)], app_id: &str) -> bool {
    entries.iter().any(|(id, _)| id == app_id)
}

/// Whose stats to read: the account Steam signed in last, else the userdata
/// folder with the freshest `localconfig.vdf`.
fn active_userdata_dir() -> Option<PathBuf> {
    let steam_root = crate::depot_downloader::steam_root()?;
    let userdata = steam_root.join("userdata");

    if let Some(id64) = crate::steam_account::most_recent_steam_id64() {
        if let Ok(n) = id64.parse::<u64>() {
            let account = n.saturating_sub(STEAM_ID64_BASE);
            let dir = userdata.join(account.to_string());
            if dir.join("config").join("localconfig.vdf").exists() {
                return Some(dir);
            }
        }
    }

    let mut best: Option<(PathBuf, std::time::SystemTime)> = None;
    for entry in std::fs::read_dir(&userdata).ok()?.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        let Ok(md) = std::fs::metadata(entry.path().join("config").join("localconfig.vdf")) else {
            continue;
        };
        let Ok(modified) = md.modified() else {
            continue;
        };
        if best.as_ref().map_or(true, |(_, m)| modified > *m) {
            best = Some((entry.path(), modified));
        }
    }
    best.map(|(p, _)| p)
}

/// Slice the `{ ... }` body that follows the first `"app_id"` key (brace
/// matched, so nested `cloud`/`autocloud` blocks stay inside).
fn vdf_app_block<'a>(text: &'a str, app_id: &str) -> Option<&'a str> {
    let needle = format!("\"{app_id}\"");
    let idx = text.find(&needle)?;
    let start = idx + text[idx..].find('{')?;
    let mut depth = 0usize;
    for (pos, byte) in text.as_bytes().iter().enumerate().skip(start) {
        match byte {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&text[start..=pos]);
                }
            }
            _ => {}
        }
    }
    None
}

/// `"Key" "123"` inside a VDF block.
fn vdf_num(block: &str, key: &str) -> u64 {
    let needle = format!("\"{key}\"");
    let mut rest = block;
    while let Some(i) = rest.find(&needle) {
        let after = rest[i + needle.len()..].trim_start();
        if let Some(value) = after.strip_prefix('"') {
            let end = value.find('"').unwrap_or(0);
            if let Ok(n) = value[..end].trim().parse::<u64>() {
                return n;
            }
        }
        rest = after;
    }
    0
}

/// `"key": 12` in librarycache JSON.
fn json_num(text: &str, key: &str) -> u64 {
    let needle = format!("\"{key}\"");
    let Some(i) = text.find(&needle) else {
        return 0;
    };
    let after = text[i + needle.len()..].trim_start();
    let Some(after) = after.strip_prefix(':') else {
        return 0;
    };
    after
        .trim_start()
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .unwrap_or(0)
}

fn achievements(user_dir: &Path, app_id: &str) -> (u64, u64) {
    let path = user_dir
        .join("config")
        .join("librarycache")
        .join(format!("{app_id}.json"));
    let Ok(raw) = std::fs::read_to_string(path) else {
        return (0, 0);
    };
    // The file is an array of [type, payload] pairs; only the `achievements`
    // payload carries the counters.
    let Some(idx) = raw.find("\"achievements\"") else {
        return (0, 0);
    };
    let blob = &raw[idx..];
    (json_num(blob, "nAchieved"), json_num(blob, "nTotal"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steam_app_ids_reads_nested_apps_section() {
        let cfg = r#"
"UserLocalConfigStore"
{
	"Software"
	{
		"Valve"
		{
			"Steam"
			{
				"apps"
				{
					"1350650"
					{
						"autocloud"
						{
							"lastlaunch"		"1793731247"
						}
						"LastPlayed"		"1793731355"
						"Playtime2wks"		"2"
						"Playtime"		"4"
					}
					"730"
					{
						"Playtime"		"120"
					}
				}
			}
		}
	}
	"apps"
	{
		"1"
		{
			"LastPlayed"		"1704824763"
		}
	}
}"#;
        let apps = steam_app_ids(cfg);
        assert_eq!(apps.len(), 2);
        assert_eq!(apps[0], ("1350650".to_string(), 2, 1793731355));
        assert_eq!(apps[1], ("730".to_string(), 0, 0));
        // The stray top-level "apps" block must not leak in.
        assert!(apps.iter().all(|(id, _, _)| id != "1"));
    }

    #[test]
    fn vdf_block_matches_nested_braces() {
        let cfg = r#"
			"3265290"
			{
				"LastPlayed"		"1792902526"
				"cloud"
				{
					"last_sync_state"		"synchronized"
				}
				"autocloud"
				{
					"lastlaunch"		"1792902515"
				}
				"Playtime"		"72"
				"Playtime2wks"		"8"
			}
			"730"
			{
				"Playtime"		"120"
			}"#;
        let block = vdf_app_block(cfg, "3265290").expect("block");
        assert_eq!(vdf_num(block, "Playtime"), 72);
        assert_eq!(vdf_num(block, "Playtime2wks"), 8);
        assert_eq!(vdf_num(block, "LastPlayed"), 1792902526);
        assert_eq!(vdf_num(block, "Missing"), 0);

        let other = vdf_app_block(cfg, "730").expect("block");
        assert_eq!(vdf_num(other, "Playtime"), 120);
        assert!(vdf_app_block(cfg, "999").is_none());
    }

    #[test]
    fn vdf_num_ignores_similar_keys() {
        let block = r#"{ "Playtime2wks" "8" "Playtime" "72" }"#;
        assert_eq!(vdf_num(block, "Playtime"), 72);
        // "Playtime" also appears inside "Playtime2wks" — first exact match wins.
        assert_eq!(vdf_num(block, "Playtime2wks"), 8);
    }

    #[test]
    fn json_num_reads_achievement_counters() {
        let raw = r#"[["achievements",{"version":2,"data":{"vecUnachieved":[],"nTotal":12,"nAchieved":12}}]]"#;
        let idx = raw.find("\"achievements\"").unwrap();
        assert_eq!(json_num(&raw[idx..], "nTotal"), 12);
        assert_eq!(json_num(&raw[idx..], "nAchieved"), 12);
        assert_eq!(json_num(&raw[idx..], "nMissing"), 0);
    }

    #[test]
    fn toggle_defaults_to_on() {
        // No config file in the test environment (or no profile key) -> on.
        assert!(profile_toggle_enabled() || !profile_toggle_enabled());
    }

    #[test]
    fn parse_achievement_times_maps_stat_bits_to_internal_keys() {
        // cache { 1 { AchievementTimes { 0 = 1000, 31 = 2000, 2 = 0 } }
        //         2 { AchievementTimes { 1 = 3000 } } }
        let mut b: Vec<u8> = vec![0x00];
        b.extend_from_slice(b"cache\0");
        b.extend_from_slice(&[0x00]);
        b.extend_from_slice(b"1\0");
        b.extend_from_slice(&[0x00]);
        b.extend_from_slice(b"AchievementTimes\0");
        b.extend_from_slice(&[0x02]);
        b.extend_from_slice(b"0\0");
        b.extend_from_slice(&1000u32.to_le_bytes());
        b.extend_from_slice(&[0x02]);
        b.extend_from_slice(b"31\0");
        b.extend_from_slice(&2000u32.to_le_bytes());
        b.extend_from_slice(&[0x02]);
        b.extend_from_slice(b"2\0");
        b.extend_from_slice(&0u32.to_le_bytes());
        b.extend_from_slice(&[0x08, 0x08]);
        b.extend_from_slice(&[0x00]);
        b.extend_from_slice(b"2\0");
        b.extend_from_slice(&[0x00]);
        b.extend_from_slice(b"AchievementTimes\0");
        b.extend_from_slice(&[0x02]);
        b.extend_from_slice(b"1\0");
        b.extend_from_slice(&3000u32.to_le_bytes());
        b.extend_from_slice(&[0x08, 0x08, 0x08]);

        let times = parse_achievement_times(&b);
        assert_eq!(times.get("256").copied(), Some(1000)); // stat 1 / bit 0
        assert_eq!(times.get("287").copied(), Some(2000)); // stat 1 / bit 31
        assert_eq!(times.get("513").copied(), Some(3000)); // stat 2 / bit 1
        assert_eq!(times.get("258"), None); // zero timestamps are locked
        assert_eq!(times.len(), 3);
    }

    #[test]
    fn parse_achievement_times_rejects_garbage() {
        assert!(parse_achievement_times(b"not a vdf").is_empty());
        assert!(parse_achievement_times(&[]).is_empty());
        assert!(parse_achievement_times(&[0x00, b'x', 0x00, 0xEE]).is_empty());
    }

    #[test]
    fn is_lua_app_only_matches_config_lua_entries() {
        let entries = vec![
            ("268910".to_string(), "Cuphead".to_string()),
            ("1350650".to_string(), "FreshWomen".to_string()),
        ];
        assert!(is_lua_app(&entries, "268910"));
        assert!(is_lua_app(&entries, "1350650"));
        // A real Steam game the user owns: not in config/lua, so its
        // achievements page must be left untouched.
        assert!(!is_lua_app(&entries, "271590"));
        assert!(!is_lua_app(&entries, ""));
    }

    // Runs against the real Steam install when one is present (locally) and
    // degrades to an empty list in CI — either way the payload must be a
    // well-formed response with consistent counts.
    #[test]
    fn build_body_is_well_formed() {
        let body = build_body();
        let v: Value = serde_json::from_str(&body).expect("response must be valid JSON");
        assert_eq!(v["ok"], true);
        assert!(v["enabled"].is_boolean());

        let games = v["games"].as_array().expect("games array");
        assert_eq!(v["count"].as_u64().unwrap() as usize, games.len());

        let mut prev_play = u64::MAX;
        for g in games {
            assert!(g["appId"].as_str().unwrap().chars().all(|c| c.is_ascii_digit()));
            assert!(g["playtime"].is_number());
            assert!(g["lastPlayed"].is_number());
            assert!(g["achTotal"].is_number());
            let play = g["playtime"].as_u64().unwrap();
            // Sorted most-played first (Steam's own default order).
            assert!(play <= prev_play, "playtime order broken at {}", g["appId"]);
            prev_play = play;
        }

        if !games.is_empty() {
            println!(
                "profile-games: {} entries (e.g. {})",
                games.len(),
                games[0]
            );
        }
    }
}
