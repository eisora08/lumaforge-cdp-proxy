use regex::Regex;
use std::fs;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// lua_writer — generate Steam .lua files and manage setManifestid pins.
// Ported verbatim from LumaForge lua_writer.rs (line formats preserved).
// ---------------------------------------------------------------------------

fn re_setmanifest_active() -> Regex {
    Regex::new(
        r#"^\s*setManifestid\s*\(\s*(\d+)\s*,\s*"(\d+)"\s*(?:,\s*(\d+))?\s*\)\s*$"#,
    )
    .unwrap()
}

fn re_setmanifest_commented() -> Regex {
    Regex::new(
        r#"^\s*--\s*setManifestid\s*\(\s*(\d+)\s*,\s*"(\d+)"\s*(?:,\s*(\d+))?\s*\)\s*$"#,
    )
    .unwrap()
}

#[derive(Debug, Clone)]
pub struct LuaDepotEntry {
    pub depot_id: u64,
    pub key: Option<String>,
    pub is_shared: bool,
    pub from_app_id: Option<u64>,
    pub dlc_app_id: Option<u64>,
}

impl LuaDepotEntry {
    pub fn new(depot_id: u64) -> Self {
        Self {
            depot_id,
            key: None,
            is_shared: false,
            from_app_id: None,
            dlc_app_id: None,
        }
    }
    pub fn with_key(mut self, key: String) -> Self {
        self.key = Some(key);
        self
    }
    pub fn with_shared(mut self, from_app_id: u64) -> Self {
        self.is_shared = true;
        self.from_app_id = Some(from_app_id);
        self
    }
    pub fn with_dlc(mut self, dlc_app_id: u64) -> Self {
        self.dlc_app_id = Some(dlc_app_id);
        self
    }
}

fn sanitize_lua_comment(name: &str) -> String {
    name.replace('\n', " ")
        .replace('\r', "")
        .replace("--", "\u{2014}")
}

fn find_last_instruction_line(lines: &[String]) -> usize {
    let re =
        Regex::new(r#"(?i)^\s*(?:--\s*)?(?:addappid|addtoken|setManifestid)\s*\("#).unwrap();
    let mut last_idx = 0;
    for (i, line) in lines.iter().enumerate() {
        if re.is_match(line) {
            last_idx = i + 1;
        }
    }
    last_idx
}

fn update_manifest_line(lines: &mut Vec<String>, depot_id: u64, new_line: &str) {
    let re_active = re_setmanifest_active();
    let re_commented = re_setmanifest_commented();

    for line in lines.iter_mut() {
        if let Some(caps) = re_active.captures(line) {
            if caps.get(1).and_then(|m| m.as_str().parse::<u64>().ok()) == Some(depot_id) {
                *line = new_line.to_string();
                return;
            }
        }
    }
    for line in lines.iter_mut() {
        if let Some(caps) = re_commented.captures(line) {
            if caps.get(1).and_then(|m| m.as_str().parse::<u64>().ok()) == Some(depot_id) {
                *line = new_line.to_string();
                return;
            }
        }
    }
    let insert_at = find_last_instruction_line(lines);
    lines.insert(insert_at, new_line.to_string());
}

pub fn generate_lua_file(
    lua_dir: &Path,
    app_id: u64,
    game_name: &str,
    depots: &[LuaDepotEntry],
    dlc_ids: &[u64],
    tokens: &[(u64, String)],
    manifest_pins: &[(u64, String, Option<u64>)],
) -> Result<PathBuf, String> {
    let mut lines: Vec<String> = Vec::new();
    lines.push("-- lua by LumaForge".to_string());
    let name_comment = sanitize_lua_comment(game_name);
    if !name_comment.is_empty() {
        lines.push(format!("-- {}", name_comment));
    }

    let main_app_entry = depots.iter().find(|d| d.depot_id == app_id);
    let main_app_key = main_app_entry.and_then(|d| d.key.as_ref());

    let main_depots: Vec<&LuaDepotEntry> = depots
        .iter()
        .filter(|d| {
            d.depot_id != app_id
                && !d.is_shared
                && (d.dlc_app_id.is_none() || d.dlc_app_id == Some(app_id))
        })
        .collect();
    let shared_depots: Vec<&LuaDepotEntry> = depots.iter().filter(|d| d.is_shared).collect();
    let dlc_depots: Vec<&LuaDepotEntry> = depots
        .iter()
        .filter(|d| {
            d.depot_id != app_id && !d.is_shared && d.dlc_app_id.is_some() && d.dlc_app_id != Some(app_id)
        })
        .collect();
    let dlc_ids_with_depots: std::collections::HashSet<u64> =
        dlc_depots.iter().filter_map(|d| d.dlc_app_id).collect();

    lines.push(String::new());
    lines.push("-- MAIN APPLICATION".to_string());
    if let Some(key) = main_app_key {
        if name_comment.is_empty() {
            lines.push(format!("addappid({}, 1, \"{}\")", app_id, key));
        } else {
            lines.push(format!(
                "addappid({}, 1, \"{}\") -- {}",
                app_id, key, name_comment
            ));
        }
    } else if name_comment.is_empty() {
        lines.push(format!("addappid({})", app_id));
    } else {
        lines.push(format!("addappid({}) -- {}", app_id, name_comment));
    }

    if !main_depots.is_empty() {
        lines.push(String::new());
        lines.push("-- MAIN APP DEPOTS".to_string());
        for d in &main_depots {
            if let Some(k) = &d.key {
                lines.push(format!(
                    "addappid({}, 1, \"{}\") -- Depot {}",
                    d.depot_id, k, d.depot_id
                ));
            }
        }
    }

    if !shared_depots.is_empty() {
        lines.push(String::new());
        lines.push("-- SHARED DEPOTS (from other apps)".to_string());
        for d in &shared_depots {
            if let Some(k) = &d.key {
                let from = d
                    .from_app_id
                    .map(|id| format!(" (Shared from App {})", id))
                    .unwrap_or_default();
                lines.push(format!(
                    "addappid({}, 1, \"{}\") -- Shared Depot {}{}",
                    d.depot_id, k, d.depot_id, from
                ));
            }
        }
    }

    if !dlc_depots.is_empty() {
        lines.push(String::new());
        lines.push("-- DLCS WITH DEDICATED DEPOTS".to_string());
        let mut dlc_groups: Vec<(u64, Vec<&LuaDepotEntry>)> = Vec::new();
        let mut seen: std::collections::HashSet<u64> = std::collections::HashSet::new();
        for d in &dlc_depots {
            if let Some(dlc_id) = d.dlc_app_id {
                if seen.insert(dlc_id) {
                    let group: Vec<&LuaDepotEntry> = dlc_depots
                        .iter()
                        .copied()
                        .filter(|dd| dd.dlc_app_id == Some(dlc_id))
                        .collect();
                    dlc_groups.push((dlc_id, group));
                }
            }
        }
        for (dlc_id, group) in &dlc_groups {
            lines.push(format!("-- DLC {} (AppID: {})", dlc_id, dlc_id));
            lines.push(format!("addappid({})", dlc_id));
            for d in group {
                if let Some(k) = &d.key {
                    lines.push(format!(
                        "addappid({}, 1, \"{}\") -- Depot {}",
                        d.depot_id, k, d.depot_id
                    ));
                }
            }
        }
    }

    let dlc_ids_without_depots: Vec<u64> = dlc_ids
        .iter()
        .copied()
        .filter(|id| *id != app_id && !dlc_ids_with_depots.contains(id))
        .collect();
    if !dlc_ids_without_depots.is_empty() {
        lines.push(String::new());
        lines.push("-- DLCS WITHOUT DEDICATED DEPOTS".to_string());
        for dlc_id in &dlc_ids_without_depots {
            lines.push(format!("addappid({})", dlc_id));
        }
    }

    if !tokens.is_empty() {
        lines.push(String::new());
        for (token_app_id, token_hex) in tokens {
            lines.push(format!("addtoken({}, \"{}\")", token_app_id, token_hex));
        }
    }

    if !manifest_pins.is_empty() {
        lines.push(String::new());
        for (depot_id, manifest_id, size_on_disk) in manifest_pins {
            if let Some(size) = size_on_disk {
                lines.push(format!(
                    "--setManifestid({}, \"{}\", {})",
                    depot_id, manifest_id, size
                ));
            } else {
                lines.push(format!("--setManifestid({}, \"{}\", 0)", depot_id, manifest_id));
            }
        }
    }

    fs::create_dir_all(lua_dir).map_err(|e| format!("Failed to create lua directory: {e}"))?;
    let lua_path = lua_dir.join(format!("{}.lua", app_id));
    fs::write(&lua_path, lines.join("\n"))
        .map_err(|e| format!("Failed to write lua file: {e}"))?;
    Ok(lua_path)
}

pub fn set_manifest_pin(
    lua_path: &Path,
    depot_id: u64,
    manifest_id: &str,
    pin: bool,
    size_override: Option<u64>,
) -> Result<(), String> {
    let content =
        fs::read_to_string(lua_path).map_err(|e| format!("Failed to read lua file: {e}"))?;
    let mut lines: Vec<String> = content.lines().map(|l| l.to_string()).collect();
    let re_any =
        Regex::new(r#"setManifestid\s*\(\s*(\d+)\s*,\s*"(\d+)"\s*(?:,\s*(\d+))?"#).unwrap();

    if pin {
        let existing_size = lines
            .iter()
            .filter_map(|l| re_any.captures(l))
            .find(|caps| {
                caps.get(1).and_then(|m| m.as_str().parse::<u64>().ok()) == Some(depot_id)
            })
            .and_then(|caps| caps.get(3))
            .and_then(|m| m.as_str().parse::<u64>().ok());
        let size = size_override.or(existing_size).unwrap_or(0);
        let new_line = format!("setManifestid({}, \"{}\", {})", depot_id, manifest_id, size);
        update_manifest_line(&mut lines, depot_id, &new_line);
    } else {
        let re_active = re_setmanifest_active();
        for line in lines.iter_mut() {
            if let Some(caps) = re_active.captures(line) {
                if caps.get(1).and_then(|m| m.as_str().parse::<u64>().ok()) == Some(depot_id)
                    && !line.trim_start().starts_with("--")
                {
                    *line = format!("--{}", line.trim_start());
                }
            }
        }
    }

    fs::write(lua_path, lines.join("\n"))
        .map_err(|e| format!("Failed to write lua file: {e}"))?;
    Ok(())
}

pub fn unpin_all_manifests(lua_path: &Path) -> Result<u32, String> {
    let content =
        fs::read_to_string(lua_path).map_err(|e| format!("Failed to read lua file: {e}"))?;
    let mut lines: Vec<String> = content.lines().map(|l| l.to_string()).collect();
    let re_active = re_setmanifest_active();
    let mut count = 0u32;
    for line in lines.iter_mut() {
        if re_active.is_match(line) && !line.trim_start().starts_with("--") {
            *line = format!("--{}", line.trim_start());
            count += 1;
        }
    }
    if count > 0 {
        fs::write(lua_path, lines.join("\n"))
            .map_err(|e| format!("Failed to write lua file: {e}"))?;
    }
    Ok(count)
}

pub fn add_dlc_to_lua(
    lua_path: &Path,
    dlc_app_id: u64,
    key: Option<&str>,
    comment: Option<&str>,
) -> Result<(), String> {
    let content =
        fs::read_to_string(lua_path).map_err(|e| format!("Failed to read lua file: {e}"))?;
    let mut lines: Vec<String> = content.lines().map(|l| l.to_string()).collect();
    let new_line = match (key, comment) {
        (Some(k), Some(c)) => format!("addappid({}, 1, \"{}\") -- {}", dlc_app_id, k, c),
        (Some(k), None) => format!("addappid({}, 1, \"{}\")", dlc_app_id, k),
        (None, Some(c)) => format!("addappid({}) -- {}", dlc_app_id, c),
        (None, None) => format!("addappid({})", dlc_app_id),
    };
    let insert_at = find_last_instruction_line(&lines);
    lines.insert(insert_at, new_line);
    fs::write(lua_path, lines.join("\n"))
        .map_err(|e| format!("Failed to write lua file: {e}"))?;
    Ok(())
}

pub fn has_active_pins(lua_path: &Path) -> bool {
    let Ok(content) = fs::read_to_string(lua_path) else {
        return false;
    };
    let re = re_setmanifest_active();
    for line in content.lines() {
        if re.is_match(line) && !line.trim_start().starts_with("--") {
            return true;
        }
    }
    false
}

pub fn active_addappids(lua_path: &Path) -> std::collections::HashSet<u64> {
    let mut set = std::collections::HashSet::new();
    let Ok(content) = fs::read_to_string(lua_path) else {
        return set;
    };
    let re = Regex::new(r#"^\s*addappid\s*\(\s*(\d+)"#).unwrap();
    for line in content.lines() {
        if let Some(caps) = re.captures(line) {
            if let Ok(id) = caps[1].parse::<u64>() {
                set.insert(id);
            }
        }
    }
    set
}
