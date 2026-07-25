use std::ffi::c_void;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tungstenite::{connect, Message};
use serde_json::{json, Value};
use serde::Deserialize;
use base64::{Engine as _, engine::general_purpose::STANDARD};

fn log_to_temp(msg: &str) {
    let Ok(local_appdata) = std::env::var("LOCALAPPDATA") else {
        return;
    };
    let log_path = PathBuf::from(local_appdata)
        .join("LumaForge")
        .join("runtime")
        .join("cef_hook.log");
    let _ = std::fs::create_dir_all(log_path.parent().unwrap());
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        let _ = writeln!(f, "{}", msg);
    }
}

fn resolve_debug_port() -> Option<u16> {
    if let Ok(port_str) = std::env::var("STEAMCDP_PORT") {
        if let Ok(port) = port_str.parse::<u16>() {
            log_to_temp(&format!("[cef_hook] Using port from env: {}", port));
            return Some(port);
        }
    }

    let cmd_line = std::env::args().collect::<Vec<String>>().join(" ");
    if let Some(pos) = cmd_line.to_lowercase().find("--remote-debugging-port=") {
        let start = pos + "--remote-debugging-port=".len();
        let remaining = &cmd_line[start..];
        let end = remaining.find(|c: char| !c.is_ascii_digit()).unwrap_or(remaining.len());
        if let Ok(port) = remaining[..end].parse::<u16>() {
            log_to_temp(&format!("[cef_hook] Using port from args: {}", port));
            return Some(port);
        }
    }

    if let Ok(local_appdata) = std::env::var("LOCALAPPDATA") {
        let discovery_path = PathBuf::from(local_appdata)
            .join("LumaForge")
            .join("runtime")
            .join("steam-cdp.json");
        if let Ok(content) = fs::read_to_string(&discovery_path) {
            if let Ok(json) = serde_json::from_str::<Value>(&content) {
                if let Some(port) = json.get("port").and_then(|p| p.as_u64()) {
                    if let Some(updated) = json.get("updatedAt").and_then(|u| u.as_u64()) {
                        let now = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap()
                            .as_secs();
                        if now.saturating_sub(updated) < 120 {
                            log_to_temp(&format!("[cef_hook] Using port from discovery: {}", port));
                            return Some(port as u16);
                        }
                    }
                }
            }
        }
    }

    None
}

// ─── Theme state ────────────────────────────────────────────────────────────

const VFS_HOST: &str = "lumaforge.local";

#[derive(Clone)]
struct LoadedPlugin {
    name: String,
    code: String,
    target_url: Option<String>,
}

#[derive(Clone)]
struct PatchEntry {
    match_regex: String,
    target_css: Option<String>,
    target_js: Option<String>,
}

#[derive(Clone)]
struct ConditionEntry {
    affects: Vec<String>,
    src: String,
}

struct ThemeState {
    theme_name: Option<String>,
    theme_dir: Option<PathBuf>,
    patches: Vec<PatchEntry>,
    webkit_css_path: Option<String>,
    webkit_js_path: Option<String>,
    root_colors_content: Option<String>,
    condition_css: Vec<ConditionEntry>,
    condition_js: Vec<String>,
    slider_css: String,
    last_signal_mtime: Option<u64>,
    active_json_mtime: Option<u64>,
    skin_json_mtime: Option<u64>,
    plugins: Vec<LoadedPlugin>,
    plugins_mtime: Option<u64>,
}

impl ThemeState {
    fn new() -> Self {
        Self {
            theme_name: None,
            theme_dir: None,
            patches: Vec::new(),
            webkit_css_path: None,
            webkit_js_path: None,
            root_colors_content: None,
            condition_css: Vec::new(),
            condition_js: Vec::new(),
            slider_css: String::new(),
            last_signal_mtime: None,
            active_json_mtime: None,
            skin_json_mtime: None,
            plugins: Vec::new(),
            plugins_mtime: None,
        }
    }

    fn theme_dir_str(&self) -> String {
        self.theme_dir.as_ref().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default()
    }
}

// ─── skin.json serde types (PascalCase like Millennium) ─────────────────────

#[derive(Deserialize, Debug, Default)]
#[serde(default)]
struct SkinJson {
    #[serde(alias = "Steam-WebKit", alias = "webkitCSS")]
    steam_webkit: Option<String>,
    #[serde(alias = "webkitJS")]
    webkit_js: Option<String>,
    #[serde(alias = "RootColors")]
    root_colors: Option<String>,
    #[serde(alias = "UseDefaultPatches")]
    use_default_patches: Option<bool>,
    #[serde(alias = "Patches")]
    patches: Vec<SkinPatch>,
    #[serde(alias = "Conditions")]
    conditions: Option<Value>,
}

#[derive(Deserialize, Debug, Clone, Default)]
#[serde(default)]
struct SkinPatch {
    #[serde(alias = "MatchRegexString")]
    match_regex_string: Option<String>,
    #[serde(alias = "TargetCss")]
    target_css: Option<String>,
    #[serde(alias = "TargetJs")]
    target_js: Option<String>,
}

#[derive(Deserialize, Debug, Default)]
#[serde(default)]
struct ConditionTargetCss {
    #[serde(alias = "affects")]
    affects: Option<Vec<String>>,
    #[serde(alias = "src")]
    src: Option<String>,
}

#[derive(Deserialize, Debug, Default)]
#[serde(default)]
struct ConditionValue {
    #[serde(alias = "TargetCss")]
    target_css: Option<ConditionTargetCss>,
}

#[derive(Deserialize, Debug, Default)]
#[serde(default)]
struct SkinSlider {
    #[serde(alias = "cssVariable")]
    css_variable: Option<String>,
    #[serde(alias = "currentValue")]
    current_value: Option<f64>,
    #[serde(alias = "defaultValue")]
    default_value: Option<f64>,
    #[serde(alias = "unit")]
    unit: Option<String>,
    #[serde(alias = "min")]
    min: Option<f64>,
    #[serde(alias = "max")]
    max: Option<f64>,
}

#[derive(Deserialize, Debug, Default)]
#[serde(default)]
struct SkinCondition {
    #[serde(default)]
    values: Option<Value>,
    #[serde(default)]
    slider: Option<SkinSlider>,
    #[serde(default)]
    default: Option<String>,
}

// ─── Default patches (same as Millennium ThemeParser.ts) ────────────────────

struct DefaultPatch {
    match_regex: &'static str,
    target_css: &'static str,
    target_js: Option<&'static str>,
}

fn get_default_patches() -> Vec<DefaultPatch> {
    vec![
        DefaultPatch { match_regex: "^Steam$", target_css: "libraryroot.custom.css", target_js: Some("libraryroot.custom.js") },
        DefaultPatch { match_regex: "^OverlayBrowser_Browser$", target_css: "libraryroot.custom.css", target_js: Some("libraryroot.custom.js") },
        DefaultPatch { match_regex: "^SP Overlay:", target_css: "libraryroot.custom.css", target_js: Some("libraryroot.custom.js") },
        DefaultPatch { match_regex: "Menu$", target_css: "libraryroot.custom.css", target_js: Some("libraryroot.custom.js") },
        DefaultPatch { match_regex: "Supernav$", target_css: "libraryroot.custom.css", target_js: Some("libraryroot.custom.js") },
        DefaultPatch { match_regex: "^notificationtoasts_", target_css: "libraryroot.custom.css", target_js: Some("libraryroot.custom.js") },
        DefaultPatch { match_regex: "^SteamBrowser_Find$", target_css: "libraryroot.custom.css", target_js: Some("libraryroot.custom.js") },
        DefaultPatch { match_regex: "^OverlayTab\\d+_Find$", target_css: "libraryroot.custom.css", target_js: Some("libraryroot.custom.js") },
        DefaultPatch { match_regex: "^Steam Big Picture Mode$", target_css: "bigpicture.custom.css", target_js: Some("bigpicture.custom.js") },
        DefaultPatch { match_regex: "^QuickAccess_", target_css: "bigpicture.custom.css", target_js: Some("bigpicture.custom.js") },
        DefaultPatch { match_regex: "^MainMenu_", target_css: "bigpicture.custom.css", target_js: Some("bigpicture.custom.js") },
        DefaultPatch { match_regex: ".friendsui-container", target_css: "friends.custom.css", target_js: Some("friends.custom.js") },
        DefaultPatch { match_regex: ".ModalDialogPopup", target_css: "libraryroot.custom.css", target_js: Some("libraryroot.custom.js") },
        DefaultPatch { match_regex: ".FullModalOverlay", target_css: "libraryroot.custom.css", target_js: Some("libraryroot.custom.js") },
    ]
}

fn file_mtime_secs(path: &PathBuf) -> Option<u64> {
    fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
}

fn runtime_dir() -> Option<PathBuf> {
    let local_appdata = std::env::var("LOCALAPPDATA").ok()?;
    Some(PathBuf::from(local_appdata).join("LumaForge").join("runtime"))
}

fn themes_base_dir() -> Option<PathBuf> {
    let local_appdata = std::env::var("LOCALAPPDATA").ok()?;
    Some(PathBuf::from(local_appdata).join("LumaForge").join("themes"))
}

fn load_theme_manifest(state: &mut ThemeState) {
    let themes_dir = match themes_base_dir() {
        Some(d) => d,
        None => return,
    };

    // 1. Read active.json to get active theme name + conditions
    let active_json_path = themes_dir.join("active.json");
    let active_json_mtime = file_mtime_secs(&active_json_path);

    if active_json_mtime == state.active_json_mtime && state.theme_dir.is_some() {
        return;
    }
    state.active_json_mtime = active_json_mtime;

    let active_json_content = match fs::read_to_string(&active_json_path) {
        Ok(c) => c,
        Err(_) => {
            log_to_temp("[cef_hook] No active.json found, loading legacy theme");
            load_legacy_theme(state);
            return;
        }
    };

    // Strip UTF-8 BOM if present, then trim whitespace
    let active_json_content = active_json_content.trim_start_matches('\u{FEFF}').trim();

    let active_json: Value = match serde_json::from_str(active_json_content) {
        Ok(v) => v,
        Err(e) => {
            log_to_temp(&format!("[cef_hook] Failed to parse active.json: {} (len={}), retrying...", e, active_json_content.len()));
            let mut last_err = e.to_string();
            let mut parsed_value = None;
            for delay_ms in [50, 150, 300, 500, 1000] {
                std::thread::sleep(Duration::from_millis(delay_ms));
                let retry_content = match fs::read_to_string(&active_json_path) {
                    Ok(c) => c,
                    Err(_) => { last_err = "file not found".to_string(); continue; }
                };
                let retry_content = retry_content.trim_start_matches('\u{FEFF}').trim();
                log_to_temp(&format!("[cef_hook] Retry at {}ms: len={}", delay_ms, retry_content.len()));
                if retry_content.is_empty() {
                    last_err = "empty file".to_string();
                    continue;
                }
                match serde_json::from_str::<Value>(retry_content) {
                    Ok(v) => { parsed_value = Some(v); break; }
                    Err(e2) => { last_err = e2.to_string(); }
                }
            }
            match parsed_value {
                Some(v) => v,
                None => {
                    log_to_temp(&format!("[cef_hook] All retries failed ({}), keeping previous state", last_err));
                    return;
                }
            }
        }
    };

    let theme_name = active_json
        .get("themes").and_then(|t| t.get("activeTheme"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let theme_name = match theme_name {
        Some(n) => n,
        None => {
            log_to_temp("[cef_hook] No activeTheme in active.json");
            load_legacy_theme(state);
            return;
        }
    };

    let theme_dir = themes_dir.join(&theme_name);
    let skin_json_path = theme_dir.join("skin.json");

    let skin_json_mtime = file_mtime_secs(&skin_json_path);
    if skin_json_mtime == state.skin_json_mtime && state.theme_name.as_deref() == Some(&theme_name) {
        return;
    }
    state.skin_json_mtime = skin_json_mtime;

    // 2. Read skin.json directly
    let skin_content = match fs::read_to_string(&skin_json_path) {
        Ok(c) => c,
        Err(_) => {
            log_to_temp(&format!("[cef_hook] No skin.json at {:?}, loading legacy", skin_json_path));
            load_legacy_theme(state);
            return;
        }
    };

    let skin: SkinJson = match serde_json::from_str(&skin_content) {
        Ok(s) => s,
        Err(e) => {
            log_to_temp(&format!("[cef_hook] Failed to parse skin.json: {}", e));
            return;
        }
    };

    state.theme_name = Some(theme_name.clone());
    state.theme_dir = Some(theme_dir.clone());

    // 3. Webkit CSS (global — injected into ALL documents)
    state.webkit_css_path = skin.steam_webkit.as_ref().map(|rel| {
        theme_dir.join(rel).to_string_lossy().into_owned()
    });
    state.webkit_js_path = skin.webkit_js.as_ref().map(|rel| {
        theme_dir.join(rel).to_string_lossy().into_owned()
    });

    // 4. RootColors — read the :root CSS inline
    state.root_colors_content = skin.root_colors.as_ref().and_then(|rel| {
        let path = theme_dir.join(rel);
        fs::read_to_string(&path).ok().filter(|s| !s.is_empty())
    });

    // 5. Build patches — explicit + UseDefaultPatches defaults
    let use_defaults = skin.use_default_patches.unwrap_or(true);
    let mut patches: Vec<PatchEntry> = skin.patches.iter().filter_map(|sp| {
        let regex = sp.match_regex_string.clone().unwrap_or_else(|| ".*".to_string());
        let target_css = sp.target_css.as_ref().map(|rel| {
            theme_dir.join(rel).to_string_lossy().into_owned()
        });
        let target_js = sp.target_js.as_ref().map(|rel| {
            theme_dir.join(rel).to_string_lossy().into_owned()
        });
        if target_css.is_some() || target_js.is_some() {
            Some(PatchEntry { match_regex: regex, target_css, target_js })
        } else {
            None
        }
    }).collect();

    // When UseDefaultPatches is true, merge defaults like Millennium
    if use_defaults && patches.is_empty() {
        for dp in get_default_patches() {
            let css_path = theme_dir.join(dp.target_css);
            let target_css = if css_path.exists() {
                Some(css_path.to_string_lossy().into_owned())
            } else {
                None
            };
            let target_js = dp.target_js.and_then(|js_rel| {
                let js_path = theme_dir.join(js_rel);
                if js_path.exists() {
                    Some(js_path.to_string_lossy().into_owned())
                } else {
                    None
                }
            });
            if target_css.is_some() || target_js.is_some() {
                patches.push(PatchEntry {
                    match_regex: dp.match_regex.to_string(),
                    target_css,
                    target_js,
                });
            }
        }
    }

    state.patches = patches;

    // 6. Parse conditions from skin.json + active.json selections
    state.condition_css.clear();
    state.condition_js.clear();
    state.slider_css.clear();

    let saved_conditions = active_json
        .get("themes").and_then(|t| t.get("conditions"))
        .and_then(|c| c.get(&theme_name));

    let mut slider_vars: Vec<(String, String)> = Vec::new();

    if let Some(skin_conditions) = skin.conditions.as_ref().and_then(|c| c.as_object()) {
        for (cond_name, cond_val) in skin_conditions {
            let cond: SkinCondition = match serde_json::from_value(cond_val.clone()) {
                Ok(c) => c,
                Err(_) => continue,
            };

            // Get selected value from active.json saved conditions
            let selected = saved_conditions
                .and_then(|sc| sc.get(cond_name))
                .and_then(|v| v.as_str())
                .or(cond.default.as_deref())
                .unwrap_or("");

            // Slider condition
            if let Some(ref slider) = cond.slider {
                if let (Some(ref var_name), Some(val)) = (&slider.css_variable, saved_conditions
                    .and_then(|sc| sc.get(cond_name))
                    .and_then(|v| v.as_f64()))
                {
                    let unit = slider.unit.as_deref().unwrap_or("");
                    slider_vars.push((var_name.clone(), format!("{}{}", val, unit)));
                } else if let (Some(ref var_name), Some(default_val)) = (&slider.css_variable, slider.default_value) {
                    let unit = slider.unit.as_deref().unwrap_or("");
                    slider_vars.push((var_name.clone(), format!("{}{}", default_val, unit)));
                }
                continue;
            }

            // Dropdown condition — resolve selected value → TargetCss with affects
            if !selected.is_empty() {
                if let Some(values) = cond.values.as_ref().and_then(|v| v.as_object()) {
                    if let Some(val_obj) = values.get(selected) {
                        let entry: ConditionValue = match serde_json::from_value(val_obj.clone()) {
                            Ok(e) => e,
                            Err(_) => continue,
                        };

                        if let Some(ref target_css) = entry.target_css {
                            let affects = target_css.affects.clone().unwrap_or_default();
                            if let Some(ref src) = target_css.src {
                                if !src.is_empty() && !affects.is_empty() {
                                    let abs_path = theme_dir.join(src).to_string_lossy().into_owned();
                                    state.condition_css.push(ConditionEntry { affects, src: abs_path });
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // Build slider CSS
    if !slider_vars.is_empty() {
        let mut css = String::from(":root {\n");
        for (var, val) in &slider_vars {
            css.push_str(&format!("    {}: {};\n", var, val));
        }
        css.push_str("}\n");
        state.slider_css = css;
    }

    log_to_temp(&format!(
        "[cef_hook] Loaded skin.json: name={}, patches={}, webkit_css={}, root_colors_bytes={}, condition_css={}, condition_js={}, slider_vars={}",
        theme_name, state.patches.len(),
        state.webkit_css_path.is_some(),
        state.root_colors_content.as_ref().map(|s| s.len()).unwrap_or(0),
        state.condition_css.len(), state.condition_js.len(), slider_vars.len(),
    ));
}

/// Legacy fallback: read current.css/current.js from themes dir
fn load_legacy_theme(state: &mut ThemeState) {
    let Ok(local_appdata) = std::env::var("LOCALAPPDATA") else { return; };
    let themes_dir = PathBuf::from(local_appdata).join("LumaForge").join("themes");
    let css_path = themes_dir.join("current.css");
    let js_path = themes_dir.join("current.js");

    let css = fs::read_to_string(&css_path).ok().filter(|s| !s.is_empty());
    let js = fs::read_to_string(&js_path).ok().filter(|s| !s.is_empty());

    if css.is_some() || js.is_some() {
        log_to_temp("[cef_hook] Loaded legacy theme (current.css/current.js)");
    }

    // For legacy mode, create a single patch that matches everything
    if css.is_some() || js.is_some() {
        state.root_colors_content = css;
        state.webkit_css_path = None;
        state.webkit_js_path = None;
        state.patches.clear();
        state.theme_name = Some("legacy".to_string());
        state.theme_dir = Some(themes_dir);
    }
}

fn load_plugins(state: &mut ThemeState) {
    let Ok(local_appdata) = std::env::var("LOCALAPPDATA") else {
        return;
    };
    let plugins_dir = PathBuf::from(local_appdata).join("LumaForge").join("plugins");

    let dir_mtime = file_mtime_secs(&plugins_dir);
    if state.plugins_mtime.is_some() && dir_mtime == state.plugins_mtime {
        return;
    }
    state.plugins_mtime = dir_mtime;

    state.plugins.clear();

    let Ok(entries) = fs::read_dir(&plugins_dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let manifest_path = path.join("manifest.json");
        let config_path = path.join("extension-config.json");

        let manifest_str = match fs::read_to_string(&manifest_path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let manifest: Value = match serde_json::from_str(&manifest_str) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let is_enabled = if config_path.exists() {
            fs::read_to_string(&config_path)
                .ok()
                .and_then(|s| serde_json::from_str::<Value>(&s).ok())
                .and_then(|v| v.get("enabled").and_then(|e| e.as_bool()).or_else(|| v.get("isEnabled").and_then(|e| e.as_bool())))
                .unwrap_or(true)
        } else {
            true
        };

        if !is_enabled {
            continue;
        }

        let plugin_id = manifest.get("id")
            .or_else(|| manifest.get("pluginId"))
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let activation = manifest.get("activation");
        let cef_config = activation;

        let inject_path = if let Some(cef) = cef_config {
            cef.get("injectScript")
                .or_else(|| cef.get("inject_script"))
                .and_then(|v| v.as_str())
                .map(|s| path.join(s))
        } else {
            continue;
        };

        let inject_path = match inject_path {
            Some(p) => p,
            None => continue,
        };

        let target_url = cef_config
            .and_then(|c| {
                c.get("targetUrl")
                    .or_else(|| c.get("target_url"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            });

        let code = match fs::read_to_string(&inject_path) {
            Ok(s) => s,
            Err(_) => continue,
        };

        state.plugins.push(LoadedPlugin {
            name: plugin_id.to_string(),
            code,
            target_url,
        });
    }
}

// ─── VFS ────────────────────────────────────────────────────────────────────

fn guess_mime_type(path: &str) -> &str {
    if path.ends_with(".css") { return "text/css"; }
    if path.ends_with(".js") { return "application/javascript"; }
    if path.ends_with(".json") { return "application/json"; }
    if path.ends_with(".svg") { return "image/svg+xml"; }
    if path.ends_with(".png") { return "image/png"; }
    if path.ends_with(".jpg") || path.ends_with(".jpeg") { return "image/jpeg"; }
    if path.ends_with(".gif") { return "image/gif"; }
    if path.ends_with(".woff") { return "font/woff"; }
    if path.ends_with(".woff2") { return "font/woff2"; }
    if path.ends_with(".ttf") { return "font/ttf"; }
    if path.ends_with(".html") || path.ends_with(".htm") { return "text/html"; }
    "application/octet-stream"
}

/// Handle VFS requests for theme files.
/// Returns Ok(body_bytes) if handled, Err if not a VFS request.
fn handle_vfs_request(url: &str, theme_state: &ThemeState) -> Result<Vec<u8>, ()> {
    // URL format: https://lumaforge.local/themes/<relative_path>
    // or: https://lumaforge.local/<relative_path> (with theme_dir as base)
    let prefix = format!("https://{}/themes/", VFS_HOST);
    let fallback_prefix = format!("https://{}/", VFS_HOST);

    let relative = if let Some(rest) = url.strip_prefix(&prefix) {
        rest.to_string()
    } else if let Some(rest) = url.strip_prefix(&fallback_prefix) {
        rest.to_string()
    } else {
        return Err(());
    };

    // Decode URL encoding
    let decoded = percent_decode(&relative);

    let theme_dir = match &theme_state.theme_dir {
        Some(d) => d.clone(),
        None => return Err(()),
    };

    let file_path = theme_dir.join(&decoded);

    log_to_temp(&format!("[cef_hook] VFS: {} -> {}", url, file_path.display()));

    match fs::read(&file_path) {
        Ok(bytes) => Ok(bytes),
        Err(e) => {
            log_to_temp(&format!("[cef_hook] VFS read error: {} -> {}", file_path.display(), e));
            // Return empty 200 instead of failing — missing CSS/JS files degrade gracefully
            Ok(Vec::new())
        }
    }
}

fn percent_decode(s: &str) -> String {
    let mut result = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i+1..i+3]).unwrap_or("");
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                result.push(byte);
                i += 3;
                continue;
            }
        }
        if bytes[i] == b'+' {
            result.push(b' ');
        } else {
            result.push(bytes[i]);
        }
        i += 1;
    }
    String::from_utf8(result).unwrap_or_default()
}

// ─── Regex matching ─────────────────────────────────────────────────────────

fn regex_matches(pattern: &str, text: &str) -> bool {
    if pattern == ".*" { return true; }
    match regex::Regex::new(pattern) {
        Ok(re) => re.is_match(text),
        Err(_) => text.contains(pattern),
    }
}

// ─── Theme injection ────────────────────────────────────────────────────────

fn build_vfs_css_url(theme_dir: &str, file_path: &str) -> String {
    // Extract the path relative to the theme dir
    let theme_dir_path = PathBuf::from(theme_dir);
    let full = PathBuf::from(file_path);
    let relative = full.strip_prefix(&theme_dir_path).unwrap_or(&full);
    let relative_str = relative.to_string_lossy().replace('\\', "/");
    format!("https://{}/themes/{}", VFS_HOST, relative_str)
}

fn inject_theme_html(
    html: &str,
    theme_state: &ThemeState,
    window_title: &str,
    url: &str,
    plugins: &[LoadedPlugin],
    css_only: bool,
) -> String {
    let mut result = html.to_string();
    let theme_dir = theme_state.theme_dir_str();

    let mut head_inject = String::new();
    let mut body_inject = String::new();

    // 1. Root colors (inline :root variables)
    if let Some(ref root_colors) = theme_state.root_colors_content {
        head_inject.push_str(&format!(
            "<style data-lumaforge=\"root-colors\" id=\"RootColors\">\n{}\n</style>\n",
            root_colors
        ));
    }

    // 2. Webkit CSS (global - injected into ALL documents)
    if let Some(ref webkit_css) = theme_state.webkit_css_path {
        let vfs_url = build_vfs_css_url(&theme_dir, webkit_css);
        head_inject.push_str(&format!(
            "<link rel=\"stylesheet\" data-lumaforge=\"webkit-css\" href=\"{}\">\n",
            vfs_url
        ));
    }

    // 3. Patches matching this window title
    for patch in &theme_state.patches {
        if regex_matches(&patch.match_regex, window_title) {
            if let Some(ref css_path) = patch.target_css {
                let vfs_url = build_vfs_css_url(&theme_dir, css_path);
                head_inject.push_str(&format!(
                    "<link rel=\"stylesheet\" data-lumaforge=\"patch-css\" href=\"{}\">\n",
                    vfs_url
                ));
            }
            if let Some(ref js_path) = patch.target_js {
                if !css_only {
                    let vfs_url = build_vfs_css_url(&theme_dir, js_path);
                    body_inject.push_str(&format!(
                        "<script type=\"module\" data-lumaforge=\"patch-js\" src=\"{}\"></script>\n",
                        vfs_url
                    ));
                }
            }
        }
    }

    // 4. Webkit JS (global)
    if !css_only {
        if let Some(ref webkit_js) = theme_state.webkit_js_path {
            let vfs_url = build_vfs_css_url(&theme_dir, webkit_js);
            body_inject.push_str(&format!(
                "<script type=\"module\" data-lumaforge=\"webkit-js\" src=\"{}\"></script>\n",
                vfs_url
            ));
        }
    }

    // 5. Legacy fallback: if root_colors_content has CSS but no theme_dir for VFS,
    //    inject inline (backward compat)
    if theme_state.patches.is_empty() && theme_state.webkit_css_path.is_none() {
        if let Some(ref legacy_css) = theme_state.root_colors_content {
            if theme_state.theme_name.as_deref() == Some("legacy") {
                head_inject.clear();
                body_inject.clear();
                head_inject.push_str(&format!(
                    "<style data-lumaforge=\"theme\">\n{}\n</style>\n",
                    legacy_css
                ));
            }
        }
    }

    // 5. Condition CSS (dropdown selections) — match affects against window title
    for cond in &theme_state.condition_css {
        let matches = cond.affects.iter().any(|affect| regex_matches(affect, window_title));
        if matches {
            let vfs_url = build_vfs_css_url(&theme_dir, &cond.src);
            head_inject.push_str(&format!(
                "<link rel=\"stylesheet\" data-lumaforge=\"condition-css\" href=\"{}\">\n",
                vfs_url
            ));
        }
    }

    // 6. Slider CSS variables
    if !theme_state.slider_css.is_empty() {
        head_inject.push_str(&format!(
            "<style data-lumaforge=\"slider-css\" id=\"MillenniumSliderConditions\">\n{}\n</style>\n",
            theme_state.slider_css
        ));
    }

    // 7. Condition JS
    if !css_only {
        for js_path in &theme_state.condition_js {
            let vfs_url = build_vfs_css_url(&theme_dir, js_path);
            body_inject.push_str(&format!(
                "<script type=\"module\" data-lumaforge=\"condition-js\" src=\"{}\"></script>\n",
                vfs_url
            ));
        }
    }

    // 8. Plugins
    if !css_only {
        for plugin in plugins {
            let matches = plugin.target_url.as_ref().map_or(true, |pattern| {
                url.contains(pattern.as_str())
            });
            if matches {
                body_inject.push_str(&format!(
                    "<script data-lumaforge-plugin=\"{}\">\n{}\n</script>\n",
                    plugin.name, plugin.code
                ));
            }
        }
    }

    // Inject into HTML
    if !head_inject.is_empty() {
        if let Some(pos) = result.to_lowercase().rfind("</head>") {
            result.insert_str(pos, &head_inject);
        } else if let Some(pos) = result.to_lowercase().rfind("<body") {
            result.insert_str(pos, &head_inject);
        } else {
            result.push_str(&head_inject);
        }
    }

    if !body_inject.is_empty() {
        if let Some(pos) = result.to_lowercase().rfind("</body>") {
            result.insert_str(pos, &body_inject);
        } else if let Some(pos) = result.to_lowercase().rfind("</html>") {
            result.insert_str(pos, &body_inject);
        } else {
            result.push_str(&body_inject);
        }
    }

    result
}

// ─── CDP helpers ────────────────────────────────────────────────────────────

fn get_browser_ws_url(port: u16) -> Option<String> {
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", port)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(3))).ok()?;
    let request = format!("GET /json/version HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n", port);
    stream.write_all(request.as_bytes()).ok()?;

    let mut reader = BufReader::new(stream);
    let mut headers_done = false;
    let mut body = String::new();
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                if headers_done {
                    body.push_str(&line);
                } else if line.trim().is_empty() {
                    headers_done = true;
                }
            }
            Err(_) => break,
        }
    }

    let json: Value = serde_json::from_str(&body).ok()?;
    json.get("webSocketDebuggerUrl")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

fn check_theme_reload_signal(state: &mut ThemeState) -> bool {
    let runtime = match runtime_dir() {
        Some(r) => r,
        None => return false,
    };
    let signal_path = runtime.join("theme-reload");

    let signal_mtime = file_mtime_secs(&signal_path);
    if signal_mtime.is_none() || signal_mtime == state.last_signal_mtime {
        return false;
    }

    state.last_signal_mtime = signal_mtime;
    log_to_temp("[cef_hook] Theme reload signal detected, reloading manifest");
    load_theme_manifest(state);
    true
}

fn send_cdp(socket: &mut tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<TcpStream>>, msg: &Value) -> bool {
    if let Err(e) = socket.send(Message::Text(msg.to_string())) {
        log_to_temp(&format!("[cef_hook] WebSocket send error: {}", e));
        return false;
    }
    true
}

fn recv_cdp_response(socket: &mut tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<TcpStream>>, expected_id: u64) -> Option<Value> {
    let deadline = SystemTime::now() + Duration::from_secs(10);
    loop {
        if SystemTime::now() > deadline {
            log_to_temp(&format!("[cef_hook] Timeout waiting for response id={}", expected_id));
            return None;
        }
        match socket.read() {
            Ok(Message::Text(text)) => {
                if let Ok(msg) = serde_json::from_str::<Value>(&text) {
                    if msg.get("id").and_then(|i| i.as_u64()) == Some(expected_id) {
                        return Some(msg);
                    }
                }
            }
            Ok(_) => {}
            Err(tungstenite::Error::Io(ref e))
                if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
            Err(e) => {
                log_to_temp(&format!("[cef_hook] WebSocket read error: {}", e));
                return None;
            }
        }
    }
}

// ─── Bridge (unchanged) ─────────────────────────────────────────────────────

const BRIDGE_SHIM_JS: &str = r#"(function(){
  var _cid=0,_p={};
  window.__luma_bridge_resolve=function(id,json){
    var p=_p[id];if(p){delete _p[id];
      var resp={ok:true,status:200,statusText:'OK',
        json:function(){return Promise.resolve(typeof json==='string'?JSON.parse(json):json)},
        text:function(){return Promise.resolve(typeof json==='string'?json:JSON.stringify(json))},
        clone:function(){return resp}};
      p(resp);}
  };
  window.__luma_bridge_reject=function(id,err){
    var p=_p[id];if(p){delete _p[id];p.reject(new Error(err))}
  };
  window.__luma_bridge_call=function(path,opts){
    var id=++_cid;
    return new Promise(function(resolve,reject){
      _p[id]={resolve:resolve,reject:reject};
      var body=(opts&&opts.body)?(typeof opts.body==='string'?opts.body:JSON.stringify(opts.body)):null;
      var payload=JSON.stringify({id:id,path:path,method:(opts&&opts.method)||'GET',body:body});
      window.__lumaNativeBridge(payload);
    });
  };
  var _origFetch=window.fetch;
  window.fetch=function(url,opts){
    if(typeof url==='string'&&url.indexOf('http://127.0.0.1:21775')===0){
      var path=url.substring(22);
      return window.__luma_bridge_call(path,opts);
    }
    return _origFetch.apply(this,arguments);
  };
  console.log('[LUMA] Bridge shim installed');
})();"#;

fn proxy_bridge_request(path: &str, method: &str, body: Option<&str>) -> Result<String, String> {
    let mut stream = TcpStream::connect("127.0.0.1:21775").map_err(|e| format!("connect: {}", e))?;
    stream.set_read_timeout(Some(Duration::from_secs(30))).ok();

    let mut req = format!(
        "GET {} HTTP/1.1\r\nHost: 127.0.0.1:21775\r\nConnection: close\r\n",
        path
    );
    let method_upper = method.to_uppercase();
    if method_upper == "POST" {
        if let Some(body_str) = body {
            let body_bytes = body_str.as_bytes();
            req = format!(
                "POST {} HTTP/1.1\r\nHost: 127.0.0.1:21775\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                path, body_bytes.len(), body_str
            );
        } else {
            req = format!(
                "POST {} HTTP/1.1\r\nHost: 127.0.0.1:21775\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
                path
            );
        }
    } else {
        req.push_str("\r\n");
    }

    stream.write_all(req.as_bytes()).map_err(|e| format!("write: {}", e))?;
    let mut resp = Vec::new();
    stream.read_to_end(&mut resp).map_err(|e| format!("read: {}", e))?;

    let resp_str = String::from_utf8_lossy(&resp);
    if let Some(pos) = resp_str.find("\r\n\r\n") {
        let resp_body = &resp_str[pos + 4..];
        let status_ok = resp_str.starts_with("HTTP/1.1 200")
            || resp_str.starts_with("HTTP/1.0 200")
            || resp_str.starts_with("HTTP/1.1 204")
            || resp_str.starts_with("HTTP/1.0 204");
        if status_ok {
            Ok(resp_body.to_string())
        } else {
            Err(format!("HTTP {}", &resp_str[9..12]))
        }
    } else {
        Err("malformed response".to_string())
    }
}

fn handle_bridge_binding(
    socket: &mut tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<TcpStream>>,
    msg_id: &mut u64,
    payload: &str,
) {
    let req: Value = match serde_json::from_str(payload) {
        Ok(v) => v,
        Err(_) => return,
    };
    let call_id = req.get("id").and_then(|i| i.as_u64()).unwrap_or(0);
    let path = req.get("path").and_then(|p| p.as_str()).unwrap_or("/");
    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("GET");
    let body = req.get("body").and_then(|b| b.as_str());

    let result_json = match proxy_bridge_request(path, method, body) {
        Ok(body) => body,
        Err(e) => {
            let reject_fn = json!({
                "id": *msg_id,
                "method": "Runtime.callFunctionOn",
                "params": {
                    "functionDeclaration": "(function(id, err) { window.__luma_bridge_reject(id, err); })",
                    "arguments": [
                        {"type": "number", "value": call_id},
                        {"type": "string", "value": &e}
                    ]
                }
            });
            *msg_id += 1;
            send_cdp(socket, &reject_fn);
            return;
        }
    };

    let resolve_fn = json!({
        "id": *msg_id,
        "method": "Runtime.callFunctionOn",
        "params": {
            "functionDeclaration": "(function(id, json) { window.__luma_bridge_resolve(id, json); })",
            "arguments": [
                {"type": "number", "value": call_id},
                {"type": "string", "value": &result_json}
            ]
        }
    });
    *msg_id += 1;
    send_cdp(socket, &resolve_fn);
}

fn install_bridge_shim(
    socket: &mut tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<TcpStream>>,
    msg_id: &mut u64,
) {
    let add_binding = json!({
        "id": *msg_id,
        "method": "Runtime.addBinding",
        "params": {"name": "__lumaNativeBridge"}
    });
    *msg_id += 1;
    if !send_cdp(socket, &add_binding) {
        return;
    }

    let add_script = json!({
        "id": *msg_id,
        "method": "Page.addScriptToEvaluateOnNewDocument",
        "params": {
            "source": BRIDGE_SHIM_JS,
            "runImmediately": true
        }
    });
    *msg_id += 1;
    send_cdp(socket, &add_script);
}

// ─── Theme injection via addScriptToEvaluateOnNewDocument ───────────────────
//
// Fetch interception only catches HTTP responses (store/community pages).
// The main Steam client window (navbar, library, sidebar, friends) loads HTML
// from internal sources that bypass Fetch. This function registers a persistent
// JS snippet that runs in EVERY new document context, ensuring all pages get
// the theme injected — matching what Millennium does via g_PopupManager hooks.

fn js_escape_str(s: &str) -> String {
    s.replace('\\', "\\\\")
     .replace('\'', "\\'")
     .replace('\n', "\\n")
     .replace('\r', "\\r")
}

fn register_theme_injection_script(
    socket: &mut tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<TcpStream>>,
    msg_id: &mut u64,
    theme_state: &ThemeState,
    inject_existing: bool,
) {
    let theme_dir = theme_state.theme_dir_str();

    let mut js = String::new();
    js.push_str(r#"(function(){
  if(window.__lumaforge_theme_injected) return;
  window.__lumaforge_theme_injected=true;
  var _patched={};
  function waitForHead(cb){
    var tries=0;
    (function poll(){
      if(document.head||document.documentElement||tries++>100) cb();
      else setTimeout(poll,20);
    })();
  }
  function addCSS(href){
    var h=document.head||document.documentElement;
    if(!href||!h||h.querySelector('link[data-lmf="'+href+'"]'))return;
    var l=document.createElement('link');
    l.rel='stylesheet';l.href=href;
    l.setAttribute('data-lmf',href);
    h.appendChild(l);
  }
  function addStyle(css,id){
    var h=document.head||document.documentElement;
    if(!css||!h)return;
    if(id&&h.querySelector('#'+id))return;
    var s=document.createElement('style');
    if(id)s.id=id;
    s.textContent=css;h.appendChild(s);
  }
  function addJS(src){
    var h=document.head||document.documentElement;
    if(!src||!h||h.querySelector('script[data-lmf="'+src+'"]'))return;
    var s=document.createElement('script');
    s.type='module';s.src=src;
    s.setAttribute('data-lmf',src);
    h.appendChild(s);
  }
  function patchByTitle(t){
    if(_patched[t])return;_patched[t]=1;
    var _p=PATCHES_PLACEHOLDER;
    _p.forEach(function(p){
      try{var re=new RegExp(p.r);if(!re.test(t))return;}
      catch(e){if(t.indexOf(p.r)===-1)return;}
      if(p.c)addCSS(p.c);
      if(p.j)addJS(p.j);
    });
  }
  function injectAllPatches(){
    var _p=PATCHES_PLACEHOLDER;
    _p.forEach(function(p){
      if(p.c)addCSS(p.c);
      if(p.j)addJS(p.j);
    });
  }
  function injectAll(){
    addStyle('ROOTCOLORS_PLACEHOLDER','RootColors');
    addCSS('WEBKITCSS_PLACEHOLDER');
    CONDITION_CSS_PLACEHOLDER
    addStyle('SLIDER_PLACEHOLDER','MillenniumSliderConditions');
    CONDITION_JS_PLACEHOLDER
    var t=document.title||'';
    patchByTitle(t);
    injectAllPatches();
  }
  waitForHead(function(){
    injectAll();
    var titleEl=document.querySelector('title');
    if(titleEl){
      var obs=new MutationObserver(function(){
        var t=document.title||'';
        if(!_patched[t])patchByTitle(t);
      });
      obs.observe(titleEl,{childList:true,subtree:true,characterData:true});
      setTimeout(function(){var t=document.title||'';if(!_patched[t])patchByTitle(t);},500);
      setTimeout(function(){var t=document.title||'';if(!_patched[t])patchByTitle(t);},2000);
    }
    var obs2=new MutationObserver(function(){
      if(document.head&&!document.head._lumaObs){
        document.head._lumaObs=1;
        injectAll();
      }
    });
    obs2.observe(document.documentElement||document,{childList:true,subtree:true});
  });
})();"#);

    // Replace placeholders with actual values
    // 1. Root colors
    let root_colors_inline = theme_state.root_colors_content.as_deref().unwrap_or("");
    js = js.replace("ROOTCOLORS_PLACEHOLDER", &js_escape_str(root_colors_inline));

    // 2. Webkit CSS
    let webkit_url = theme_state.webkit_css_path.as_ref()
        .map(|css| build_vfs_css_url(&theme_dir, css))
        .unwrap_or_default();
    js = js.replace("WEBKITCSS_PLACEHOLDER", &webkit_url);

    // 3. Condition CSS — build JSON array with affects for runtime title matching
    let mut cond_css_entries: Vec<Value> = Vec::new();
    for cond in &theme_state.condition_css {
        let vfs_url = build_vfs_css_url(&theme_dir, &cond.src);
        let affects: Vec<Value> = cond.affects.iter().map(|a| Value::String(a.clone())).collect();
        cond_css_entries.push(json!({"url": vfs_url, "affects": affects}));
    }
    let cond_css_json = serde_json::to_string(&cond_css_entries).unwrap_or_else(|_| "[]".to_string());
    let mut cond_css_js = String::new();
    cond_css_js.push_str(&format!("var _condCss={};\n", cond_css_json));
    cond_css_js.push_str("_condCss.forEach(function(c){\n");
    cond_css_js.push_str("  var t=document.title||'';\n");
    cond_css_js.push_str("  var cls=(document.documentElement&&document.documentElement.className)||'';\n");
    cond_css_js.push_str("  var match=c.affects.some(function(a){\n");
    cond_css_js.push_str("    try{return new RegExp(a).test(t)||new RegExp(a).test(cls);}\n");
    cond_css_js.push_str("    catch(e){return t.indexOf(a)>-1||cls.indexOf(a)>-1;}\n");
    cond_css_js.push_str("  });\n");
    cond_css_js.push_str("  if(match)addCSS(c.url);\n");
    cond_css_js.push_str("});\n");
    js = js.replace("CONDITION_CSS_PLACEHOLDER", &cond_css_js);

    // 4. Slider CSS
    let slider_escaped = js_escape_str(&theme_state.slider_css);
    js = js.replace("SLIDER_PLACEHOLDER", &slider_escaped);

    // 5. Condition JS
    let mut cond_js_js = String::new();
    for js_path in &theme_state.condition_js {
        let vfs_url = build_vfs_css_url(&theme_dir, js_path);
        cond_js_js.push_str(&format!("addJS('{}');\n", vfs_url));
    }
    js = js.replace("CONDITION_JS_PLACEHOLDER", &cond_js_js);

    // 6. Build patches JSON array
    let mut patches_json = String::from("[");
    for patch in &theme_state.patches {
        let regex_escaped = js_escape_str(&patch.match_regex);
        let vfs_css = match patch.target_css {
            Some(ref css) => format!("'{}'", js_escape_str(&build_vfs_css_url(&theme_dir, css))),
            None => "''".to_string(),
        };
        let vfs_js = match patch.target_js {
            Some(ref jsf) => format!("'{}'", js_escape_str(&build_vfs_css_url(&theme_dir, jsf))),
            None => "''".to_string(),
        };
        patches_json.push_str(&format!("{{r:'{}',c:{},j:{}}},", regex_escaped, vfs_css, vfs_js));
    }
    patches_json.push(']');
    js = js.replace("PATCHES_PLACEHOLDER", &patches_json);

    // Register via CDP
    let add_script = json!({
        "id": *msg_id,
        "method": "Page.addScriptToEvaluateOnNewDocument",
        "params": {
            "source": js,
            "runImmediately": true
        }
    });
    *msg_id += 1;
    send_cdp(socket, &add_script);

    log_to_temp(&format!(
        "[cef_hook] Registered theme injection script ({} bytes, {} patches)",
        js.len(), theme_state.patches.len()
    ));

    // Now inject into all EXISTING page targets
    if inject_existing {
        inject_into_existing_targets(socket, msg_id, &js);
    }
}

/// Iterate all CDP targets and evaluate the injection script in each page target
fn inject_into_existing_targets(
    socket: &mut tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<TcpStream>>,
    msg_id: &mut u64,
    _js: &str,
) {
    let get_targets = json!({
        "id": *msg_id,
        "method": "Target.getTargets",
        "params": {}
    });
    *msg_id += 1;
    if !send_cdp(socket, &get_targets) {
        return;
    }
    let resp = match recv_cdp_response(socket, *msg_id - 1) {
        Some(r) => r,
        None => {
            log_to_temp("[cef_hook] Failed to get targets");
            return;
        }
    };

    let targets = match resp.get("result").and_then(|r| r.get("targetInfos")).and_then(|t| t.as_array()) {
        Some(arr) => arr,
        None => {
            log_to_temp("[cef_hook] No targets found");
            return;
        }
    };

    let mut injected_count = 0;
    for target in targets {
        let target_type = target.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let target_id = target.get("targetId").and_then(|t| t.as_str()).unwrap_or("");
        let target_url = target.get("url").and_then(|u| u.as_str()).unwrap_or("");

        if target_type != "page" || target_id.is_empty() {
            continue;
        }

        // Skip our own lumaforge.local and about:blank
        if target_url.contains("lumaforge.local") || target_url.starts_with("about:") {
            continue;
        }

        // Attach to the target
        let attach = json!({
            "id": *msg_id,
            "method": "Target.attachToTarget",
            "params": {
                "targetId": target_id,
                "flatten": true
            }
        });
        *msg_id += 1;
        if !send_cdp(socket, &attach) {
            continue;
        }
        let attach_resp = match recv_cdp_response(socket, *msg_id - 1) {
            Some(r) => r,
            None => continue,
        };
        let session_id = attach_resp.get("result")
            .and_then(|r| r.get("sessionId"))
            .and_then(|s| s.as_str())
            .unwrap_or("");

        if session_id.is_empty() {
            log_to_temp(&format!("[cef_hook] Failed to attach to target {}: no session", target_id));
            continue;
        }

        // Navigate to same URL to trigger addScriptToEvaluateOnNewDocument
        let navigate = json!({
            "id": *msg_id,
            "method": "Page.reload",
            "params": {},
            "sessionId": session_id
        });
        *msg_id += 1;
        send_cdp(socket, &navigate);
        injected_count += 1;
        log_to_temp(&format!("[cef_hook] Reloaded target: {} ({})", &target_url[..target_url.len().min(80)], target_id));
    }

    log_to_temp(&format!("[cef_hook] Injected into {} existing targets", injected_count));
}

fn register_webkit_js(
    socket: &mut tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<TcpStream>>,
    msg_id: &mut u64,
    theme_state: &ThemeState,
) {
    // Register webkit JS as persistent script if available
    if let Some(ref webkit_js_path) = theme_state.webkit_js_path {
        let code = match fs::read_to_string(webkit_js_path) {
            Ok(c) => c,
            Err(e) => {
                log_to_temp(&format!("[cef_hook] Failed to read webkit JS: {}", e));
                return;
            }
        };
        let add_script = json!({
            "id": *msg_id,
            "method": "Page.addScriptToEvaluateOnNewDocument",
            "params": {
                "source": code,
                "runImmediately": true
            }
        });
        *msg_id += 1;
        send_cdp(socket, &add_script);
        log_to_temp("[cef_hook] Webkit JS registered for all documents");
    }
}

// ─── Fetch handler ──────────────────────────────────────────────────────────

fn handle_fetch_paused(
    socket: &mut tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<TcpStream>>,
    msg: &Value,
    msg_id: &mut u64,
    theme_state: &mut ThemeState,
) {
    let params = match msg.get("params") {
        Some(p) => p,
        None => return,
    };
    let request_id_str = params.get("requestId").and_then(|r| r.as_str()).unwrap_or("");
    let url = params
        .get("request")
        .and_then(|r| r.get("url"))
        .and_then(|u| u.as_str())
        .unwrap_or("");
    let status = params
        .get("responseStatusCode")
        .and_then(|s| s.as_u64());
    let is_request_stage = status.is_none();
    let status_code = status.unwrap_or(200);
    let response_headers = params.get("responseHeaders").cloned();

    // ── Bridge proxy (request stage) ──
    if url.contains("/luma-bridge/") && is_request_stage {
        let method = params.get("request")
            .and_then(|r| r.get("method"))
            .and_then(|m| m.as_str())
            .unwrap_or("GET");
        let post_data = params.get("request")
            .and_then(|r| r.get("postData"))
            .and_then(|p| p.as_str());
        let idx = url.find("/luma-bridge/").unwrap_or(0) + "/luma-bridge".len();
        let path = if idx < url.len() { &url[idx..] } else { "/" };

        let body = match proxy_bridge_request(path, method, post_data) {
            Ok(b) => b,
            Err(_e) => {
                let fail_msg = json!({
                    "id": *msg_id,
                    "method": "Fetch.failRequest",
                    "params": {"requestId": request_id_str, "errorReason": "Failed"}
                });
                *msg_id += 1;
                send_cdp(socket, &fail_msg);
                return;
            }
        };

        let body_b64 = STANDARD.encode(body.as_bytes());
        let content_length = body.len();
        let resp_headers = json!([
            {"name": "Content-Type", "value": "application/json"},
            {"name": "Access-Control-Allow-Origin", "value": "*"},
            {"name": "Content-Length", "value": content_length.to_string()}
        ]);
        let fulfill = json!({
            "id": *msg_id,
            "method": "Fetch.fulfillRequest",
            "params": {
                "requestId": request_id_str,
                "responseCode": 200,
                "responseHeaders": resp_headers,
                "body": body_b64
            }
        });
        *msg_id += 1;
        send_cdp(socket, &fulfill);
        return;
    }

    // ── VFS requests (request stage) ──
    if url.contains(&format!("{}", VFS_HOST)) && is_request_stage {
        match handle_vfs_request(url, theme_state) {
            Ok(body_bytes) => {
                let mime = guess_mime_type(url);
                let body_b64 = STANDARD.encode(&body_bytes);
                let content_length = body_bytes.len();
                let resp_headers = json!([
                    {"name": "Content-Type", "value": mime},
                    {"name": "Access-Control-Allow-Origin", "value": "*"},
                    {"name": "Cache-Control", "value": "no-cache"},
                    {"name": "Content-Length", "value": content_length.to_string()}
                ]);
                let fulfill = json!({
                    "id": *msg_id,
                    "method": "Fetch.fulfillRequest",
                    "params": {
                        "requestId": request_id_str,
                        "responseCode": 200,
                        "responseHeaders": resp_headers,
                        "body": body_b64
                    }
                });
                *msg_id += 1;
                send_cdp(socket, &fulfill);
                return;
            }
            Err(()) => {
                // Not a VFS request or file not found, fail it
                let fail_msg = json!({
                    "id": *msg_id,
                    "method": "Fetch.failRequest",
                    "params": {"requestId": request_id_str, "errorReason": "NameNotResolved"}
                });
                *msg_id += 1;
                send_cdp(socket, &fail_msg);
                return;
            }
        }
    }

    // ── Non-HTML request stage: continue ──
    if is_request_stage {
        let continue_msg = json!({
            "id": *msg_id,
            "method": "Fetch.continueRequest",
            "params": {"requestId": request_id_str}
        });
        *msg_id += 1;
        send_cdp(socket, &continue_msg);
        return;
    }

    // ── Check if response is HTML ──
    let lower_url = url.to_lowercase();
    let is_html_url = lower_url.ends_with(".html") || lower_url.ends_with(".htm");

    let mut content_type_str = String::new();
    let is_html_content_type = response_headers.as_ref().and_then(|h| {
        if let Value::Array(arr) = h {
            for item in arr {
                if let Value::Object(map) = item {
                    let name = map.get("name").and_then(|v| v.as_str()).unwrap_or("").to_lowercase();
                    let value = map.get("value").and_then(|v| v.as_str()).unwrap_or("").to_lowercase();
                    if name == "content-type" {
                        content_type_str = value.clone();
                        if value.contains("text/html") {
                            return Some(true);
                        }
                    }
                }
            }
        }
        None
    }).unwrap_or(false);

    let is_html = is_html_content_type || is_html_url;

    if !is_html {
        let continue_msg = json!({
            "id": *msg_id,
            "method": "Fetch.continueResponse",
            "params": {"requestId": request_id_str}
        });
        *msg_id += 1;
        send_cdp(socket, &continue_msg);
        return;
    }

    // ── Skip sensitive URLs where HTML injection can break page functionality ──
    let skip_injection = lower_url.contains("/agecheck/")
        || lower_url.contains("/login")
        || lower_url.contains("login.steampowered")
        || lower_url.contains("help.steampowered.com")
        || lower_url.contains("/checkout/")
        || lower_url.contains("/mobileauth/")
        || lower_url.contains("/two_factor/")
        || lower_url.contains("/steamguard/")
        || lower_url.contains("/imagematch/")
        || lower_url.contains("/forgot")
        // CDN/video/audio domains — not Steam pages, no need to inject
        || lower_url.contains("akamaized.net")
        || lower_url.contains("fastly.")
        || lower_url.contains("cloudflare")
        || lower_url.contains("video.")
        || lower_url.contains(".mpd")
        || lower_url.contains(".m3u8")
        || lower_url.contains("segment/")
        || lower_url.contains("/broadcast/");
    if skip_injection {
        let continue_msg = json!({
            "id": *msg_id,
            "method": "Fetch.continueResponse",
            "params": {"requestId": request_id_str}
        });
        *msg_id += 1;
        send_cdp(socket, &continue_msg);
        return;
    }

    // ── HTML interception: inject theme ──
    log_to_temp(&format!("[cef_hook] Intercepting HTML: {}", &url[..url.len().min(120)]));

    let get_body = json!({
        "id": *msg_id,
        "method": "Fetch.getResponseBody",
        "params": {"requestId": request_id_str}
    });
    let current_id = *msg_id;
    *msg_id += 1;

    if !send_cdp(socket, &get_body) {
        return;
    }

    let body_response = recv_cdp_response(socket, current_id);
    let body_msg = match body_response {
        Some(m) => m,
        None => return,
    };

    let result = match body_msg.get("result") {
        Some(r) => r,
        None => return,
    };
    let body = result.get("body").and_then(|b| b.as_str()).unwrap_or("");
    let is_base64 = result
        .get("base64Encoded")
        .and_then(|b| b.as_bool())
        .unwrap_or(false);

    let decoded_body = if is_base64 {
        STANDARD.decode(body).unwrap_or_default()
    } else {
        body.as_bytes().to_vec()
    };

    let body_str = String::from_utf8_lossy(&decoded_body).to_string();

    // Store pages: only inject CSS (no JS) — JS injection breaks Steam's agecheck redirect
    let is_store_page = lower_url.contains("store.steampowered.com/app/")
        || lower_url.contains("store.steampowered.com/sub/")
        || lower_url.contains("store.steampowered.com/pack/")
        || lower_url.contains("store.steampowered.com/dlc/")
        || lower_url.contains("store.steampowered.com/bundle/")
        || lower_url.contains("store.steampowered.com/explore")
        || lower_url.contains("store.steampowered.com/search")
        || lower_url.contains("store.steampowered.com/category/")
        || lower_url.contains("store.steampowered.com/tags/")
        || lower_url.contains("store.steampowered.com/recommended")
        || lower_url.contains("store.steampowered.com/apphover");

    // Get window title from the URL or page context
    // For now, we use the URL as a hint; the real title comes from Page.frameNavigated
    // or we can try to extract it from the HTML
    let window_title = extract_title_from_html(&body_str);

    let modified = inject_theme_html(&body_str, theme_state, &window_title, url, &theme_state.plugins.clone(), false);

    let encoded = STANDARD.encode(modified.as_bytes());
    let mut fulfill_params = json!({
        "requestId": request_id_str,
        "responseCode": status_code,
        "body": encoded
    });

    if let Some(headers) = response_headers {
        if let Some(obj) = fulfill_params.as_object_mut() {
            obj.insert("responseHeaders".to_string(), headers);
        }
    }

    let fulfill = json!({
        "id": *msg_id,
        "method": "Fetch.fulfillRequest",
        "params": fulfill_params
    });
    *msg_id += 1;
    send_cdp(socket, &fulfill);
}

/// Try to extract a window title hint from the HTML
fn extract_title_from_html(html: &str) -> String {
    // Look for <title> tag
    if let Some(start) = html.to_lowercase().find("<title>") {
        let content_start = start + 7;
        if let Some(end) = html[content_start..].to_lowercase().find("</title>") {
            return html[content_start..content_start + end].trim().to_string();
        }
    }
    // Fallback: empty string (will match .* patches)
    String::new()
}

// ─── CDP main loop ──────────────────────────────────────────────────────────

fn handle_cdp_connection(port: u16) {
    let mut retry_count = 0u32;
    loop {
        let browser_ws_url = match get_browser_ws_url(port) {
            Some(url) => url,
            None => {
                retry_count += 1;
                let delay = if retry_count < 20 { 100 } else { 3000 };
                log_to_temp(&format!("[cef_hook] Could not get browser WebSocket URL, retrying in {}ms (attempt {})", delay, retry_count));
                std::thread::sleep(Duration::from_millis(delay));
                continue;
            }
        };
        log_to_temp(&format!("[cef_hook] Connecting to CDP: {}", browser_ws_url));

        let (mut socket, _) = match connect(&browser_ws_url) {
            Ok(conn) => conn,
            Err(e) => {
                log_to_temp(&format!("[cef_hook] Failed to connect: {}, retrying in 3s", e));
                std::thread::sleep(Duration::from_secs(3));
                continue;
            }
        };
        log_to_temp("[cef_hook] Connected to CDP browser endpoint");

        let mut msg_id = 1u64;

        // Enable Fetch interception
        let enable_fetch = json!({
            "id": msg_id,
            "method": "Fetch.enable",
            "params": {
                "patterns": [
                    {"urlPattern": format!("*{}*", VFS_HOST), "requestStage": "Request"},
                    {"urlPattern": "*/luma-bridge/*", "requestStage": "Request"},
                    {"urlPattern": "*", "requestStage": "Response"}
                ]
            }
        });
        if !send_cdp(&mut socket, &enable_fetch) {
            log_to_temp("[cef_hook] Failed to enable Fetch, reconnecting...");
            std::thread::sleep(Duration::from_secs(2));
            continue;
        }
        msg_id += 1;

        let enable_runtime = json!({
            "id": msg_id,
            "method": "Runtime.enable",
            "params": {}
        });
        if !send_cdp(&mut socket, &enable_runtime) {
            std::thread::sleep(Duration::from_secs(2));
            continue;
        }
        msg_id += 1;

        let enable_page = json!({
            "id": msg_id,
            "method": "Page.enable",
            "params": {}
        });
        if !send_cdp(&mut socket, &enable_page) {
            std::thread::sleep(Duration::from_secs(2));
            continue;
        }
        msg_id += 1;

        let bypass_csp = json!({
            "id": msg_id,
            "method": "Page.setBypassCSP",
            "params": {"enabled": true}
        });
        if !send_cdp(&mut socket, &bypass_csp) {
            std::thread::sleep(Duration::from_secs(2));
            continue;
        }
        msg_id += 1;

        // Load theme manifest
        let mut theme_state = ThemeState::new();
        load_theme_manifest(&mut theme_state);
        load_plugins(&mut theme_state);

        // Register webkit JS globally
        register_webkit_js(&mut socket, &mut msg_id, &theme_state);

        // Register persistent theme injection for ALL documents (including internal Steam windows)
        register_theme_injection_script(&mut socket, &mut msg_id, &theme_state, true);

        install_bridge_shim(&mut socket, &mut msg_id);

        let mut lost = false;
        let mut loop_iter = 0u64;
        while !lost {
            loop_iter += 1;
            if loop_iter % 5 == 0 {
                if check_theme_reload_signal(&mut theme_state) {
                    register_webkit_js(&mut socket, &mut msg_id, &theme_state);
                    register_theme_injection_script(&mut socket, &mut msg_id, &theme_state, true);
                }
                load_plugins(&mut theme_state);
            }

            match socket.read() {
                Ok(Message::Text(text)) => {
                    if let Ok(msg) = serde_json::from_str::<Value>(&text) {
                        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");

                        match method {
                            "Fetch.requestPaused" => {
                                handle_fetch_paused(
                                    &mut socket,
                                    &msg,
                                    &mut msg_id,
                                    &mut theme_state,
                                );
                            }
                            "Runtime.bindingCalled" => {
                                let name = msg.get("params").and_then(|p| p.get("name")).and_then(|n| n.as_str()).unwrap_or("");
                                let payload = msg.get("params").and_then(|p| p.get("payload")).and_then(|p| p.as_str()).unwrap_or("");
                                if name == "__lumaNativeBridge" {
                                    handle_bridge_binding(&mut socket, &mut msg_id, payload);
                                }
                            }
                            "Runtime.consoleAPICalled" => {
                                let args = msg.get("params").and_then(|p| p.get("args")).and_then(|a| a.as_array());
                                if let Some(arr) = args {
                                    let parts: Vec<String> = arr.iter().filter_map(|a| a.get("value").and_then(|v| v.as_str()).map(|s| s.to_string())).collect();
                                    let text = parts.join(" ");
                                    if text.contains("LUMA") || text.contains("Bridge") || text.contains("luma") || text.contains("bridge") {
                                        log_to_temp(&format!("[cef_hook] JS console: {}", &text[..text.len().min(200)]));
                                    }
                                }
                            }
                            "Page.frameNavigated" => {
                                let frame = msg.get("params").and_then(|p| p.get("frame"));
                                let _nav_url = frame.and_then(|f| f.get("url")).and_then(|u| u.as_str()).unwrap_or("");
                                let is_main = frame.and_then(|f| f.get("parentId")).is_none();

                                if is_main {
                                    load_theme_manifest(&mut theme_state);
                                    load_plugins(&mut theme_state);
                                    register_webkit_js(&mut socket, &mut msg_id, &theme_state);
                                    // Re-register only the persistent script — addScriptToEvaluateOnNewDocument
                                    // handles new documents automatically. We must NOT call
                                    // inject_into_existing_targets here because it does Page.reload
                                    // on all targets, causing an infinite reload loop (frameNavigated
                                    // → reload → frameNavigated → ...).
                                    register_theme_injection_script(&mut socket, &mut msg_id, &theme_state, false);
                                    install_bridge_shim(&mut socket, &mut msg_id);
                                }
                            }
                            _ => {}
                        }
                    }
                }
                Ok(Message::Close(_)) => {
                    log_to_temp("[cef_hook] WebSocket closed, reconnecting...");
                    lost = true;
                }
            Err(e) => {
                log_to_temp(&format!("[cef_hook] WebSocket error: {}, reconnecting...", e));
                    lost = true;
                }
                _ => {}
            }
        }

        log_to_temp("[cef_hook] Reconnecting in 2s...");
        std::thread::sleep(Duration::from_secs(2));
    }
}

// ─── Entry point ────────────────────────────────────────────────────────────

unsafe extern "system" fn dll_main_thread(_param: *mut c_void) -> u32 {
    log_to_temp("[cef_hook] DLL loaded into webhelper process");

    let port = match resolve_debug_port() {
        Some(p) => p,
        None => {
            log_to_temp("[cef_hook] No debug port found, exiting");
            return 0;
        }
    };

    handle_cdp_connection(port);

    0
}

#[no_mangle]
pub unsafe extern "system" fn DllMain(
    _hinst_dll: *mut c_void,
    fdw_reason: u32,
    _lpv_reserved: *mut c_void,
) -> i32 {
    match fdw_reason {
        1 => {
            std::thread::spawn(|| {
                dll_main_thread(std::ptr::null_mut());
            });
            1
        }
        0 | 2 | 3 => 1,
        _ => 0,
    }
}
