use std::ffi::c_void;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tungstenite::{connect, Message};
use serde_json::{json, Value};
use serde::Deserialize;
use base64::{Engine as _, engine::general_purpose::STANDARD};

// Flattened CDP sessions for targets we auto-attached to (targetId → sessionId),
// plus targets that already got the theme script registered/evaluated.
static SESSIONS: std::sync::Mutex<Vec<(String, String)>> = std::sync::Mutex::new(Vec::new());
static REGISTERED_TARGETS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
// (sessionId, target url) — used to re-push the CSS payload to steamloopback
// sessions after a theme reload.
static SESSION_URLS: std::sync::Mutex<Vec<(String, String)>> = std::sync::Mutex::new(Vec::new());

mod accent;

fn log_to_temp(msg: &str) {
    // Relative timestamp (+ss.mmm since this process loaded the DLL) so that
    // interleaved lines from multiple webhelper processes can be ordered and
    // crash timing can be measured.
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    let elapsed = START.get_or_init(Instant::now).elapsed();
    let stamped = format!(
        "[+{}.{:03}] {}",
        elapsed.as_secs(),
        elapsed.subsec_millis(),
        msg
    );

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
        let _ = writeln!(f, "{}", stamped);
    }
}

// ─── CDP tracing (STEAMCDP_TRACE=1) ─────────────────────────────────────────

fn trace_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var("STEAMCDP_TRACE")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
    })
}

thread_local! {
    // (sent count, last sent "method id", received count, last received summary)
    static CONN_STATS: std::cell::RefCell<(u64, String, u64, String)> =
        std::cell::RefCell::new((0, String::new(), 0, String::new()));
}

/// Record every received CDP message so the log of a dead connection shows
/// exactly what arrived last before the reset.
fn note_rx(msg: &Value) {
    let method = msg.get("method").and_then(|m| m.as_str()).map(|s| s.to_string());
    let summary = match &method {
        Some(m) => m.clone(),
        None => format!(
            "response id={}",
            msg.get("id").and_then(|i| i.as_u64()).unwrap_or(0)
        ),
    };
    CONN_STATS.with(|s| {
        let mut s = s.borrow_mut();
        s.2 += 1;
        s.3 = summary.clone();
    });
    if trace_enabled() {
        log_to_temp(&format!("[cef_hook] ← {}", summary));
    }
}

fn conn_stats_snapshot() -> String {
    CONN_STATS.with(|s| {
        let s = s.borrow();
        format!("tx={} last_tx='{}' rx={} last_rx='{}'", s.0, s.1, s.2, s.3)
    })
}

fn reset_conn_stats() {
    CONN_STATS.with(|s| *s.borrow_mut() = (0, String::new(), 0, String::new()));
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
    /// Relative path (under LumaForge/plugins) of the inject script, used to
    /// build the VFS URL for runtime injection into steamloopback documents.
    vfs_rel: Option<String>,
    /// If set, the plugin is also injected at runtime (fetch via VFS) into
    /// documents whose location.href contains this substring (library window).
    runtime_target_url: Option<String>,
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
    condition_js: Vec<ConditionEntry>,
    slider_css: String,
    last_signal_mtime: Option<u64>,
    active_json_mtime: Option<u64>,
    skin_json_mtime: Option<u64>,
    plugins: Vec<LoadedPlugin>,
    plugins_mtime: Option<u64>,
    // Serialized CSS payload ({webkit, conds, cjs, patches}) pushed into
    // steamloopback sessions as window.__lumaCSS. Built lazily, invalidated
    // whenever the manifest reloads.
    css_payload: Option<String>,
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
            css_payload: None,
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
    #[serde(alias = "TargetJs")]
    target_js: Option<ConditionTargetCss>,
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
    #[serde(default, deserialize_with = "de_string_or_number")]
    default: Option<String>,
}

/// Millennium accepts `default` as either a JSON string or number
/// (see theme_cfg.cc setup_conditionals: is_string / is_number branches).
fn de_string_or_number<'de, D>(d: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v = Option::<Value>::deserialize(d)?;
    Ok(match v {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s),
        Some(Value::Number(n)) => Some(n.to_string()),
        Some(other) => {
            return Err(serde::de::Error::custom(format!(
                "condition default must be string or number, got {}",
                other
            )))
        }
    })
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

/// No active.json (or no activeTheme in it): pick the first theme dir that has
/// a skin.json, mirroring Millennium's behavior of activating themes on import.
fn auto_select_theme(themes_dir: &PathBuf) -> Option<String> {
    let mut entries: Vec<PathBuf> = fs::read_dir(themes_dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    entries.sort();
    for p in entries {
        if p.join("skin.json").exists() {
            if let Some(name) = p.file_name() {
                return Some(name.to_string_lossy().into_owned());
            }
        }
    }
    None
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
    state.css_payload = None;

    let active_json_content = match fs::read_to_string(&active_json_path) {
        Ok(c) => c,
        Err(_) => {
            // No active.json → auto-select the first installed theme so a bare
            // themes/ dir (theme copied in, nothing configured) still applies.
            match auto_select_theme(&themes_dir) {
                Some(name) => {
                    log_to_temp(&format!(
                        "[cef_hook] No active.json found — auto-selected theme '{}'",
                        name
                    ));
                    format!(
                        r#"{{"themes":{{"activeTheme":{}}}}}"#,
                        serde_json::to_string(&name).unwrap_or_else(|_| "\"\"".into())
                    )
                }
                None => {
                    log_to_temp("[cef_hook] No active.json found and no themes installed, loading legacy theme");
                    load_legacy_theme(state);
                    return;
                }
            }
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
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty());

    let theme_name = match theme_name {
        Some(n) => n,
        None => match auto_select_theme(&themes_dir) {
            Some(n) => {
                log_to_temp(&format!(
                    "[cef_hook] No activeTheme in active.json — auto-selected '{}'",
                    n
                ));
                n
            }
            None => {
                log_to_temp("[cef_hook] No activeTheme in active.json");
                load_legacy_theme(state);
                return;
            }
        },
    };

    let theme_dir = themes_dir.join(&theme_name);
    let skin_json_path = theme_dir.join("skin.json");

    let skin_json_mtime = file_mtime_secs(&skin_json_path);
    if skin_json_mtime == state.skin_json_mtime && state.theme_name.as_deref() == Some(&theme_name) {
        return;
    }
    state.skin_json_mtime = skin_json_mtime;
    state.css_payload = None;

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

    // 4. RootColors — read the :root CSS inline (relative url()s → VFS)
    state.root_colors_content = skin.root_colors.as_ref().and_then(|rel| {
        let path = theme_dir.join(rel);
        fs::read_to_string(&path)
            .ok()
            .filter(|s| !s.is_empty())
            .map(|css| {
                let dir = path
                    .parent()
                    .map(|p| p.to_path_buf())
                    .unwrap_or_else(|| theme_dir.clone());
                rewrite_css_urls(&css, &dir, &theme_dir)
            })
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
                // ThemeConditionConfig::save writes selections as strings, so accept
                // both JSON numbers and numeric strings.
                let saved_val = saved_conditions
                    .and_then(|sc| sc.get(cond_name))
                    .and_then(|v| v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse::<f64>().ok())));
                // Millennium seed order (theme_cfg.cc setup_conditionals):
                // saved -> condition-level default (string|number) -> slider.min -> 0
                let fallback_val = cond
                    .default
                    .as_deref()
                    .and_then(|d| d.parse::<f64>().ok())
                    .or(slider.min)
                    .unwrap_or(0.0);
                if let Some(ref var_name) = &slider.css_variable {
                    let val = saved_val.unwrap_or(fallback_val);
                    let unit = slider.unit.as_deref().unwrap_or("");
                    slider_vars.push((var_name.clone(), format!("{}{}", val, unit)));
                }
                continue;
            }

            // Dropdown condition — resolve selected value → TargetCss/TargetJs with affects
            if let Some(values) = cond.values.as_ref().and_then(|v| v.as_object()) {
                // Millennium validates: selected must exist in values, otherwise
                // fall back to condition default, otherwise the first value key.
                let mut selected_owned = selected.to_string();
                if !values.contains_key(&selected_owned) {
                    selected_owned = cond
                        .default
                        .clone()
                        .filter(|d| values.contains_key(d))
                        .or_else(|| values.keys().next().cloned())
                        .unwrap_or_default();
                }
                if selected_owned.is_empty() {
                    continue;
                }
                if let Some(val_obj) = values.get(&selected_owned) {
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
                        if let Some(ref target_js) = entry.target_js {
                            let affects = target_js.affects.clone().unwrap_or_default();
                            if let Some(ref src) = target_js.src {
                                if !src.is_empty() && !affects.is_empty() {
                                    let abs_path = theme_dir.join(src).to_string_lossy().into_owned();
                                    state.condition_js.push(ConditionEntry { affects, src: abs_path });
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

        let runtime_target_url = cef_config
            .and_then(|c| {
                c.get("runtimeTargetUrl")
                    .or_else(|| c.get("runtime_target_url"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            });

        let inject_script_rel = cef_config
            .and_then(|c| {
                c.get("injectScript")
                    .or_else(|| c.get("inject_script"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            });

        let code = match fs::read_to_string(&inject_path) {
            Ok(s) => s,
            Err(_) => continue,
        };

        let vfs_rel = match (entry.file_name().to_str(), inject_script_rel.as_deref()) {
            (Some(dir_name), Some(rel)) => Some(format!("{}/{}", dir_name, rel.replace('\\', "/"))),
            _ => None,
        };

        state.plugins.push(LoadedPlugin {
            name: plugin_id.to_string(),
            code,
            target_url,
            vfs_rel,
            runtime_target_url,
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
    // or: https://lumaforge.local/plugins/<dir>/<file> (plugin scripts)

    // Plugin scripts (checked before the theme fallback prefix, which would
    // otherwise swallow "plugins/..." as a theme-relative path).
    let plugins_prefix = format!("https://{}/plugins/", VFS_HOST);
    if let Some(rest) = url.strip_prefix(&plugins_prefix) {
        let cut = rest.find(|c| c == '?' || c == '#').unwrap_or(rest.len());
        let decoded = percent_decode(&rest[..cut]);
        if decoded.contains("..") {
            return Err(());
        }
        let base = match std::env::var("LOCALAPPDATA") {
            Ok(s) => PathBuf::from(s).join("LumaForge").join("plugins"),
            Err(_) => return Ok(Vec::new()),
        };
        let file_path = base.join(&decoded);
        log_to_temp(&format!("[cef_hook] VFS plugins: {} -> {}", url, file_path.display()));
        return match fs::read(&file_path) {
            Ok(bytes) => Ok(bytes),
            Err(e) => {
                log_to_temp(&format!("[cef_hook] VFS plugins read error: {} -> {}", file_path.display(), e));
                Ok(Vec::new())
            }
        };
    }

    let prefix = format!("https://{}/themes/", VFS_HOST);
    let fallback_prefix = format!("https://{}/", VFS_HOST);

    let relative = if let Some(rest) = url.strip_prefix(&prefix) {
        rest.to_string()
    } else if let Some(rest) = url.strip_prefix(&fallback_prefix) {
        rest.to_string()
    } else {
        return Err(());
    };

    // Strip query string / fragment so cache-busted URLs still map to a file
    let cut = relative.find(|c| c == '?' || c == '#').unwrap_or(relative.len());
    let relative = relative[..cut].to_string();

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

/// Case-insensitive ASCII substring search (byte offsets valid for the input).
fn find_ascii_ci(hay: &str, needle: &str) -> Option<usize> {
    let h = hay.as_bytes();
    let n = needle.as_bytes();
    if n.is_empty() || h.len() < n.len() {
        return None;
    }
    for i in 0..=(h.len() - n.len()) {
        if h[i..i + n.len()].eq_ignore_ascii_case(n) {
            return Some(i);
        }
    }
    None
}

/// Millennium-compatible window matching — port of patcher/index.ts
/// EvaluatePatches + Dispatch.ts classListMatch:
/// - title: regex against the window title
/// - classes: substring match of the pattern against ".<token>" entries built
///   from <html class> + <body class> (Millennium: '.token'.includes(pattern))
/// - alias: patches matching `^Steam$` also apply to "Steam Games List" windows
///   (only for patches; Millennium's EvaluatePatch skips the alias)
fn window_matches(pattern: &str, title: &str, html_class: &str, body_class: &str, use_alias: bool) -> bool {
    if pattern == ".*" {
        return true;
    }
    if regex_matches(pattern, title) {
        return true;
    }
    for token in html_class.split_whitespace().chain(body_class.split_whitespace()) {
        if format!(".{}", token).contains(pattern) {
            return true;
        }
    }
    if use_alias && pattern == "^Steam$" && regex_matches("^Steam Games List$", title) {
        return true;
    }
    false
}

// ─── Theme injection ────────────────────────────────────────────────────────

/// ssh-bridge: maps theme CSS vars to the steam-store-helper plugin's
/// --luma-ssh-* family. Emitted twice: inline by the HTML interceptor (store
/// pages) and as a runtime style by build_theme_js -> injectDoc (internal
/// windows: library, popups). Color-format fallbacks only; RGB triplet vars
/// (e.g. SpaceTheme --st-accent-1) are handled by the extension's runtime
/// resolveThemeColors() which sets inline overrides (inline wins the cascade
/// over both this block and the plugin's own :root defaults).
const SSH_BRIDGE_CSS: &str = ":root {\n  \
     --luma-ssh-accent: var(--accent-color, var(--fill-color-accent-secondary, var(--SystemAccentColor, #66c0ff)));\n  \
     --luma-ssh-bg-panel: var(--background-fill-color-mica-background-base, #1b2838);\n  \
     --luma-ssh-text-primary: var(--fill-color-text-primary, #fff);\n  \
     --luma-ssh-text: var(--fill-color-text-secondary, #c7d5e0);\n  \
     --luma-ssh-text-muted: var(--fill-color-subtle-secondary, #8f98a0);\n\
     }";

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
    html_class: &str,
    body_class: &str,
    url: &str,
    plugins: &[LoadedPlugin],
    css_only: bool,
) -> String {
    let mut result = html.to_string();
    let theme_dir = theme_state.theme_dir_str();

    let mut head_inject = String::new();
    let mut body_inject = String::new();

    // 1a. System accent colors (--SystemAccentColor*), injected before the
    //     theme's own variables so var(--SystemAccentColor*) resolves.
    head_inject.push_str(&format!(
        "<style data-lumaforge=\"accent-colors\" id=\"SystemAccentColorInject\">\n{}\n</style>\n",
        accent::system_accent_css()
    ));

    // 1. Root colors (inline :root variables)
    if let Some(ref root_colors) = theme_state.root_colors_content {
        head_inject.push_str(&format!(
            "<style data-lumaforge=\"root-colors\" id=\"RootColors\">\n{}\n</style>\n",
            root_colors
        ));
    }

    // 1b. Bridge: map theme-specific CSS vars to --luma-ssh-* plugin vars
    //     Uses var() with fallbacks so themes that don't define these still work.
    //     Note: color-format vars only here (RGB triplets like --st-accent-1
    //     can't be wrapped by var() chains — the extension's runtime
    //     resolveThemeColors() handles those with inline overrides).
    head_inject.push_str(&format!(
        "<style id=\"LumaSshBridge\" data-lumaforge=\"ssh-bridge\">\n{}\n</style>\n",
        SSH_BRIDGE_CSS
    ));

    // 2. Webkit CSS (global - injected into ALL documents)
    if let Some(ref webkit_css) = theme_state.webkit_css_path {
        let vfs_url = build_vfs_css_url(&theme_dir, webkit_css);
        head_inject.push_str(&format!(
            "<link rel=\"stylesheet\" data-lumaforge=\"webkit-css\" href=\"{}\">\n",
            vfs_url
        ));
    }

    // 3. Patches matching this window (title + html/body classes, Millennium alias)
    for patch in &theme_state.patches {
        if window_matches(&patch.match_regex, window_title, html_class, body_class, true) {
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

    // 5. Condition CSS (dropdown selections) — match affects against this window
    for cond in &theme_state.condition_css {
        let matches = cond
            .affects
            .iter()
            .any(|affect| window_matches(affect, window_title, html_class, body_class, false));
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

    // 7. Condition JS — same affects matching as condition CSS
    if !css_only {
        for cond in &theme_state.condition_js {
            let matches = cond
                .affects
                .iter()
                .any(|affect| window_matches(affect, window_title, html_class, body_class, false));
            if matches {
                let vfs_url = build_vfs_css_url(&theme_dir, &cond.src);
                body_inject.push_str(&format!(
                    "<script type=\"module\" data-lumaforge=\"condition-js\" src=\"{}\"></script>\n",
                    vfs_url
                ));
            }
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
    let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("?").to_string();
    let id = msg.get("id").and_then(|i| i.as_u64()).unwrap_or(0);
    if let Err(e) = socket.send(Message::Text(msg.to_string())) {
        log_to_temp(&format!(
            "[cef_hook] WebSocket send error: {} (while sending {} id={})",
            e, method, id
        ));
        return false;
    }
    CONN_STATS.with(|s| {
        let mut s = s.borrow_mut();
        s.0 += 1;
        s.1 = format!("{} id={}", method, id);
    });
    if trace_enabled() {
        log_to_temp(&format!("[cef_hook] → {} id={}", method, id));
    }
    true
}

/// Wait for the CDP response with `expected_id`.
///
/// Messages that arrive while waiting (Fetch.requestPaused events, responses
/// to other in-flight commands) are queued into `pending` instead of being
/// dropped: the main loop dispatches them afterwards. Discarding them used to
/// leave paused requests hanging forever — the browser never finished loading
/// those resources (JS/CSS) and pages rendered broken or not at all.
fn recv_cdp_response(
    socket: &mut tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<TcpStream>>,
    pending: &mut Vec<Value>,
    expected_id: u64,
) -> Option<Value> {
    // A response for this id may already have been queued during a previous wait.
    if let Some(pos) = pending
        .iter()
        .position(|m| m.get("id").and_then(|i| i.as_u64()) == Some(expected_id))
    {
        return Some(pending.remove(pos));
    }

    let deadline = SystemTime::now() + Duration::from_secs(10);
    loop {
        if SystemTime::now() > deadline {
            log_to_temp(&format!("[cef_hook] Timeout waiting for response id={}", expected_id));
            return None;
        }
        match socket.read() {
            Ok(Message::Text(text)) => {
                if let Ok(msg) = serde_json::from_str::<Value>(&text) {
                    note_rx(&msg);
                    if msg.get("id").and_then(|i| i.as_u64()) == Some(expected_id) {
                        return Some(msg);
                    }
                    // Not the response we are waiting for — keep it for the
                    // main loop (events must never be discarded).
                    if pending.len() >= 4096 {
                        pending.remove(0);
                    }
                    pending.push(msg);
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
                log_to_temp(&format!(
                    "[cef_hook] WebSocket read error: {} ({})",
                    e,
                    conn_stats_snapshot()
                ));
                return None;
            }
        }
    }
}

// ─── Bridge (unchanged) ─────────────────────────────────────────────────────

const BRIDGE_SHIM_JS: &str = r#"(function(){
  if (window.__luma_bridge_call) return;
  var _cid=0,_p={};
  window.__luma_bridge_resolve=function(id,json){
    var p=_p[id];if(p){delete _p[id];
      var resp={ok:true,status:200,statusText:'OK',
        json:function(){return Promise.resolve(typeof json==='string'?JSON.parse(json):json)},
        text:function(){return Promise.resolve(typeof json==='string'?json:JSON.stringify(json))},
        clone:function(){return resp}};
      p.resolve(resp);}
  };
  window.__luma_bridge_reject=function(id,err){
    var p=_p[id];if(p){delete _p[id];p.reject(new Error(err))}
  };
  window.__luma_bridge_call=function(path,opts){
    var id=++_cid;
    return new Promise(function(resolve,reject){
      // Safety net: if the native side never resolves (CDP error, detach),
      // fail instead of hanging the page's fetch forever.
      var t=setTimeout(function(){
        if(_p[id]){delete _p[id];reject(new Error('bridge timeout'));}
      },10000);
      _p[id]={
        resolve:function(r){clearTimeout(t);resolve(r);},
        reject:function(e){clearTimeout(t);reject(e instanceof Error?e:new Error(e));}
      };
      var body=(opts&&opts.body)?(typeof opts.body==='string'?opts.body:JSON.stringify(opts.body)):null;
      var payload=JSON.stringify({id:id,path:path,method:(opts&&opts.method)||'GET',body:body});
      window.__lumaNativeBridge(payload);
    });
  };
  var _origFetch=window.fetch;
  window.fetch=function(url,opts){
    if(typeof url==='string'&&(url.indexOf('http://127.0.0.1:21775')===0||url.indexOf('http://127.0.0.1:21776')===0)){
      var path=url.replace(/^https?:\/\/[^\/]+/,'');
      if(!path)path='/';
      return window.__luma_bridge_call(path,opts);
    }
    return _origFetch.apply(this,arguments);
  };
  console.log('[LUMA] Bridge shim installed');
})();"#;

fn proxy_bridge_request(path: &str, method: &str, body: Option<&str>) -> Result<String, String> {
    let mut last_err = String::new();
    // Bridge may bind the primary port or fall back (e.g. TIME_WAIT after a
    // flood of connections leaves 21775 unbindable) — try both.
    for port in [21775u16, 21776] {
        match proxy_bridge_request_on(path, method, body, port) {
            Ok(v) => return Ok(v),
            Err(e) => last_err = e,
        }
    }
    Err(last_err)
}

fn proxy_bridge_request_on(
    path: &str,
    method: &str,
    body: Option<&str>,
    port: u16,
) -> Result<String, String> {
    let addr = format!("127.0.0.1:{}", port);
    let mut stream = TcpStream::connect(&addr).map_err(|e| format!("connect({}): {}", addr, e))?;
    stream.set_read_timeout(Some(Duration::from_secs(30))).ok();

    let method_upper = method.to_uppercase();
    let req = match body {
        Some(body_str) => format!(
            "{} {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            method_upper,
            path,
            addr,
            body_str.as_bytes().len(),
            body_str
        ),
        None => format!(
            "{} {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
            method_upper, path, addr
        ),
    };

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
    sid: Option<&str>,
    ecid: Option<i64>,
) {
    let req: Value = match serde_json::from_str(payload) {
        Ok(v) => v,
        Err(_) => return,
    };
    let call_id = req.get("id").and_then(|i| i.as_u64()).unwrap_or(0);
    let path = req.get("path").and_then(|p| p.as_str()).unwrap_or("/");
    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("GET");
    let body = req.get("body").and_then(|b| b.as_str());

    // callFunctionOn requires an explicit target: use the execution context the
    // binding fired from (also correct when the shim runs inside an iframe).
    let mut call_fn_on = |func: &str, args: Value| {
        let mut params = json!({ "functionDeclaration": func, "arguments": args });
        if let Some(e) = ecid {
            params["executionContextId"] = json!(e);
        }
        let mut msg = json!({
            "id": *msg_id,
            "method": "Runtime.callFunctionOn",
            "params": params
        });
        if let Some(s) = sid {
            msg["sessionId"] = json!(s);
        }
        *msg_id += 1;
        send_cdp(socket, &msg);
    };

    let result_json = match proxy_bridge_request(path, method, body) {
        Ok(body) => body,
        Err(e) => {
            call_fn_on(
                "(function(id, err) { window.__luma_bridge_reject(id, err); })",
                json!([
                    {"type": "number", "value": call_id},
                    {"type": "string", "value": &e}
                ]),
            );
            return;
        }
    };

    call_fn_on(
        "(function(id, json) { window.__luma_bridge_resolve(id, json); })",
        json!([
            {"type": "number", "value": call_id},
            {"type": "string", "value": &result_json}
        ]),
    );
}

/// Install the native bridge shim into ONE target session.
///
/// Browser-level (sessionId-less) `Page.*`/`Runtime.*` commands are rejected by
/// this CEF build ("wasn't found" — see register_session_theme), so every
/// command here must carry the target's sessionId. Without this the shim never
/// ran: `__luma_bridge_call` stayed undefined and every extension fetch fell
/// back to the 1s CDP drain queue (slow "Loading..." tabs in the store).
///
/// * `register_for_new_docs` — also register addScriptToEvaluateOnNewDocument
///   so future documents in this session start with the shim (attach-time).
///   Re-sending it on every navigation would accumulate duplicate scripts, so
///   frameNav re-ensure calls pass `false` and only evaluate into the fresh
///   document (BRIDGE_SHIM_JS is idempotent).
fn install_bridge_shim_session(
    socket: &mut tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<TcpStream>>,
    msg_id: &mut u64,
    sid: &str,
    register_for_new_docs: bool,
) {
    // bindingCalled events are only delivered when Runtime is enabled on the session.
    let runtime_enable = json!({
        "id": *msg_id,
        "method": "Runtime.enable",
        "params": {},
        "sessionId": sid
    });
    *msg_id += 1;
    if !send_cdp(socket, &runtime_enable) {
        return;
    }

    let add_binding = json!({
        "id": *msg_id,
        "method": "Runtime.addBinding",
        "params": {"name": "__lumaNativeBridge"},
        "sessionId": sid
    });
    *msg_id += 1;
    if !send_cdp(socket, &add_binding) {
        return;
    }

    if register_for_new_docs {
        let add_script = json!({
            "id": *msg_id,
            "method": "Page.addScriptToEvaluateOnNewDocument",
            "params": {
                "source": BRIDGE_SHIM_JS,
                "runImmediately": true
            },
            "sessionId": sid
        });
        *msg_id += 1;
        send_cdp(socket, &add_script);
    }

    // Cover the document that is already loaded right now.
    let eval = json!({
        "id": *msg_id,
        "method": "Runtime.evaluate",
        "params": {"expression": BRIDGE_SHIM_JS, "returnByValue": true},
        "sessionId": sid
    });
    *msg_id += 1;
    send_cdp(socket, &eval);

    log_to_temp(&format!(
        "[cef_hook] Bridge shim installed (sid={}, new_docs={})",
        sid, register_for_new_docs
    ));
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

/// Regex covering the CSS `@import` forms: `url('…')`, `url("…")`, `url(…)`,
/// `'…'`, `"…"`. Captures the URL in group 1..=6 depending on the variant.
fn import_re() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(
            r#"(?i)@import\s+(?:url\(\s*(?:'([^']+)'|"([^"]+)"|([^'")\s]+))\s*\)|'([^']+)'|"([^"]+)"|([^\s';]+))\s*;"#,
        )
        .expect("import regex")
    })
}

fn is_inlineable_import(url: &str) -> bool {
    let u = url.trim();
    !(u.starts_with("http:")
        || u.starts_with("https:")
        || u.starts_with("//")
        || u.starts_with("data:")
        || u.starts_with("about:")
        || u.starts_with('/'))
}

/// Regex for CSS `url(...)` references (single/double-quoted or bare).
fn url_re() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r#"(?i)url\(\s*(?:'([^']*)'|"([^"]*)"|([^'")\s]+))\s*\)"#)
            .expect("url regex")
    })
}

/// Rewrite relative `url(...)` references to absolute VFS URLs.
///
/// Payload styles are inlined as text into steamloopback documents, where a
/// relative `url('../fonts/x.woff2')` resolves against the page origin and
/// 404s — @font-face glyphs then render as tofu boxes. Point them at the VFS
/// (the same base link mode already uses). Absolute, root-relative, `data:`
/// and fragment refs are left untouched, as are refs that would escape the
/// theme root. `file_dir` is the directory of the file being processed so
/// each recursion level resolves against its own location.
fn rewrite_css_urls(
    css: &str,
    file_dir: &std::path::Path,
    theme_dir: &std::path::Path,
) -> String {
    let rel_dir = match file_dir.strip_prefix(theme_dir) {
        Ok(r) => r.to_path_buf(),
        Err(_) => return css.to_string(),
    };
    let re = url_re();
    let mut out = String::with_capacity(css.len() + 256);
    let mut last = 0;
    for caps in re.captures_iter(css) {
        let m = caps.get(0).unwrap();
        out.push_str(&css[last..m.start()]);
        last = m.end();
        let url = match (1..=3).find_map(|i| caps.get(i)) {
            Some(u) => u.as_str().trim(),
            None => {
                out.push_str(m.as_str());
                continue;
            }
        };
        if !is_inlineable_import(url) || url.starts_with('#') {
            out.push_str(m.as_str());
            continue;
        }
        let mut parts: Vec<String> = rel_dir
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        let mut ok = true;
        for comp in std::path::Path::new(url).components() {
            match comp {
                std::path::Component::Normal(s) => parts.push(s.to_string_lossy().into_owned()),
                std::path::Component::CurDir => {}
                std::path::Component::ParentDir => {
                    if parts.pop().is_none() {
                        ok = false;
                        break;
                    }
                }
                _ => {
                    ok = false;
                    break;
                }
            }
        }
        if !ok || parts.is_empty() {
            out.push_str(m.as_str());
            continue;
        }
        out.push_str(&format!(
            "url(\"https://{}/themes/{}\")",
            VFS_HOST,
            parts.join("/")
        ));
    }
    out.push_str(&css[last..]);
    out
}

/// Recursively inline relative `@import` rules so the CSS is self-contained.
///
/// Payload-mode styles are injected as text, so `@import url('./elements/x.css')`
/// resolves against the document origin (steamloopback.host serves Steam's HTML
/// with zero CSS rules) instead of the theme dir — the entire Library/sidebar
/// theming lives in those imports and was silently dropped. Absolute URLs
/// (CDN fonts) are left for the browser. `skip` holds files emitted separately
/// elsewhere in the payload (webkit.css → its own LmfWebkit style); `stack`
/// guards against import cycles and preserves import order/duplicates so the
/// cascade matches link mode. Text between `@import` statements is run through
/// [`rewrite_css_urls`] with the owning file's directory, so relative `url()`
/// refs (fonts/images) also survive the steamloopback origin; the `@import`
/// statements themselves are never rewritten (they are either inlined by path
/// or already absolute).
fn expand_css_imports(
    css: &str,
    base_dir: &std::path::Path,
    theme_dir: &std::path::Path,
    skip: &[PathBuf],
    stack: &mut Vec<PathBuf>,
) -> String {
    let re = import_re();
    let mut out = String::with_capacity(css.len() + 8192);
    let mut last = 0;
    for caps in re.captures_iter(css) {
        let m = caps.get(0).unwrap();
        out.push_str(&rewrite_css_urls(&css[last..m.start()], base_dir, theme_dir));
        last = m.end();
        let url = match (1..=6).find_map(|i| caps.get(i)) {
            Some(u) => u.as_str().trim(),
            None => {
                out.push_str(m.as_str());
                continue;
            }
        };
        if !is_inlineable_import(url) {
            out.push_str(m.as_str());
            continue;
        }
        let resolved = base_dir.join(url);
        let canon = fs::canonicalize(&resolved).unwrap_or_else(|_| resolved.clone());
        if skip.contains(&canon) {
            continue;
        }
        if stack.contains(&canon) {
            log_to_temp(&format!("[cef_hook] @import cycle dropped: {}", url));
            continue;
        }
        let content = match fs::read_to_string(&resolved) {
            Ok(c) => c,
            Err(e) => {
                log_to_temp(&format!(
                    "[cef_hook] @import not readable ({}): {:?}",
                    e, resolved
                ));
                continue;
            }
        };
        stack.push(canon);
        let sub = resolved
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| base_dir.to_path_buf());
        out.push_str(&expand_css_imports(&content, &sub, theme_dir, skip, stack));
        stack.pop();
    }
    out.push_str(&rewrite_css_urls(&css[last..], base_dir, theme_dir));
    out
}

/// Builds the payload's shared CSS `files` table: several patch entries
/// (Menu$, ^Steam$, .ModalDialogPopup, …) point at the same file, so each
/// unique file is read+expanded once and referenced by index (`f`).
struct PayloadBuilder {
    skip: Vec<PathBuf>,
    theme_dir: PathBuf,
    files: Vec<String>,
    file_index: std::collections::HashMap<PathBuf, usize>,
    raw_total: usize,
    expanded_total: usize,
}

impl PayloadBuilder {
    fn new(skip: Vec<PathBuf>, theme_dir: PathBuf) -> Self {
        Self {
            skip,
            theme_dir,
            files: Vec::new(),
            file_index: std::collections::HashMap::new(),
            raw_total: 0,
            expanded_total: 0,
        }
    }

    /// Read `path` and inline its relative @imports. Returns the expanded CSS
    /// plus the raw byte count (for the build log).
    fn expand_path(&self, path: &str) -> Option<(String, usize)> {
        let p = PathBuf::from(path);
        let raw = fs::read_to_string(&p).ok()?;
        let base = p.parent().map(|x| x.to_path_buf()).unwrap_or_default();
        let mut stack: Vec<PathBuf> = fs::canonicalize(&p).into_iter().collect();
        let expanded = expand_css_imports(&raw, &base, &self.theme_dir, &self.skip, &mut stack);
        let raw_len = raw.len();
        Some((expanded, raw_len))
    }

    /// Like `expand_path`, but dedupes into `files` and returns the index.
    fn add(&mut self, path: &str) -> Option<usize> {
        let canon = fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path));
        if let Some(&idx) = self.file_index.get(&canon) {
            return Some(idx);
        }
        let (expanded, raw_len) = self.expand_path(path)?;
        self.raw_total += raw_len;
        self.expanded_total += expanded.len();
        self.files.push(expanded);
        let idx = self.files.len() - 1;
        self.file_index.insert(canon, idx);
        Some(idx)
    }
}

/// Build (and cache) the serialized CSS payload pushed into steamloopback
/// sessions as `window.__lumaCSS`. Link tags to `lumaforge.local` are only
/// intercepted by Fetch for store-origin documents — requests from
/// steamloopback documents (Shared + every popup window) go out to DNS and
/// fail, so those docs must receive the theme CSS as text instead.
fn theme_css_payload_json(state: &mut ThemeState) -> String {
    if let Some(p) = &state.css_payload {
        return p.clone();
    }
    // webkit.css ships as its own LmfWebkit style (added before any patch in
    // injectDoc), so patch-level imports of it are dropped — inlining it again
    // would double the payload. Its rules land first instead of at their
    // import position; ties with equal specificity go to the importing sheet.
    let skip: Vec<PathBuf> = state
        .webkit_css_path
        .as_deref()
        .and_then(|p| fs::canonicalize(p).ok())
        .into_iter()
        .collect();
    let theme_dir_str = state.theme_dir_str();
    let builder = PayloadBuilder::new(skip, state.theme_dir.clone().unwrap_or_default());
    let webkit = state
        .webkit_css_path
        .as_ref()
        .and_then(|p| builder.expand_path(p).map(|(css, _)| css))
        .unwrap_or_default();
    let conds: Vec<Value> = state
        .condition_css
        .iter()
        .map(|c| {
            json!({
                "affects": c.affects,
                "css": builder.expand_path(&c.src).map(|(css, _)| css).unwrap_or_default(),
            })
        })
        .collect();
    // `ju` points the script at its VFS copy as a real <script type=module>:
    // an inline classic <script> would SyntaxError on ESM (import/export) and
    // take the whole Library root JS down with it. `js` stays as fallback for
    // classic scripts when the VFS fetch is unavailable.
    let cjs: Vec<Value> = state
        .condition_js
        .iter()
        .map(|c| {
            json!({
                "affects": c.affects,
                "ju": build_vfs_css_url(&theme_dir_str, &c.src),
                "js": fs::read_to_string(&c.src).unwrap_or_default(),
            })
        })
        .collect();
    let mut builder = builder;
    let patches: Vec<Value> = state
        .patches
        .iter()
        .map(|p| {
            json!({
                "r": p.match_regex,
                "f": p.target_css.as_ref().and_then(|f| builder.add(f)),
                "ju": p.target_js.as_ref().map(|f| build_vfs_css_url(&theme_dir_str, f)),
                "js": p.target_js.as_ref().and_then(|f| fs::read_to_string(f).ok()),
            })
        })
        .collect();
    let raw_total = builder.raw_total;
    let expanded_total = builder.expanded_total;
    let files = std::mem::take(&mut builder.files);
    let payload = json!({
        "webkit": webkit,
        "webkitjs": state.webkit_js_path.as_ref().map(|p| build_vfs_css_url(&theme_dir_str, p)),
        "files": files,
        "conds": conds,
        "cjs": cjs,
        "patches": patches,
    });
    let s = serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string());
    log_to_temp(&format!(
        "[cef_hook] CSS payload built: {} bytes ({} raw + @import expansion to {} across {} files, {} conditions, {} patches)",
        s.len(),
        raw_total,
        expanded_total,
        builder.file_index.len(),
        state.condition_css.len(),
        state.patches.len()
    ));
    state.css_payload = Some(s.clone());
    s
}

/// Evaluate `window.__lumaCSS = {...}` in the given session.
fn push_css_payload(
    socket: &mut tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<TcpStream>>,
    msg_id: &mut u64,
    state: &mut ThemeState,
    sid: &str,
) {
    let payload = theme_css_payload_json(state);
    // Dispatch lumaforge:theme-reload after the new payload lands so page
    // scripts (steam-store-helper themeColor) can re-resolve theme colors.
    let expr = format!(
        "try{{window.__lumaCSS={};document.dispatchEvent(new CustomEvent('lumaforge:theme-reload'));}}catch(e){{}}",
        payload
    );
    log_to_temp(&format!("[cef_hook] pushCSS: sid={} expr_len={}", sid, expr.len()));
    let eval = json!({
        "id": *msg_id,
        "method": "Runtime.evaluate",
        "params": {"expression": expr, "awaitPromise": false},
        "sessionId": sid
    });
    *msg_id += 1;
    send_cdp(socket, &eval);
}

fn build_theme_js(theme_state: &ThemeState) -> String {
    let theme_dir = theme_state.theme_dir_str();

    let mut js = String::new();
    js.push_str(r#"(function(){
  if(window.__lumaforge_theme_injected) return;
  window.__lumaforge_theme_injected=true;
  // Plugins eligible for runtime injection: [{i: id, u: vfsRelPath, t: [hrefSubstrings]}]
  var _lumaPlugins=PLUGINS_MANIFEST_PLACEHOLDER;
  // ── Millennium compat shims (themes expect these globals) ──
  // renderer.js polls window.opener.__ROUTER_HOOK_INSTANCE.registerForRouterSetup
  // (Millennium's RouterHook lives on SharedJSContext) to know when Steam's
  // React router rendered; without it the sidebar never initializes. The real
  // hook delays the callback until router render, but every fluenty callback
  // starts with waitForElement (MutationObserver), so an immediate async call
  // is safe and resolves once the elements exist.
  try{
    if(!window.__ROUTER_HOOK_INSTANCE){
      window.__ROUTER_HOOK_INSTANCE={
        registerForRouterSetup:function(cb){
          if(typeof cb!=='function')return;
          setTimeout(function(){ try{cb();}catch(e){console.warn('[luma] routerSetup cb',e);} },0);
        }
      };
      window.__ROUTER_HOOK_INSTANCE__=window.__ROUTER_HOOK_INSTANCE;
    }
  }catch(e){}
  // Millennium.findElement(doc, selector, timeout) -> Promise<NodeList>
  try{
    if(!window.Millennium){
      window.Millennium={
        findElement:function(doc,selector,timeout){
          return new Promise(function(resolve,reject){
            var d=doc||document;
            var found=d.querySelectorAll(selector);
            if(found.length){resolve(found);return;}
            var obs=new MutationObserver(function(){
              var m=d.querySelectorAll(selector);
              if(m.length){obs.disconnect();if(timer)clearTimeout(timer);resolve(m);}
            });
            obs.observe(d.body||d.documentElement,{childList:true,subtree:true});
            var timer=timeout?setTimeout(function(){obs.disconnect();reject();},timeout):null;
          });
        }
      };
    }
  }catch(e){}
  // SharedJSContext is headless — Steam mirrors its <head> links into popups
  // (login window, toasts). Theme links there block Steam's renderWhenReady
  // gate and the popup never becomes visible (WasHidden stays 1). Detect it
  // by title (window.name is empty) and strip any links we already added.
  function isSharedCtx(){ try{ return document.title==='SharedJSContext' || window.name==='SP Shared JS Context'; }catch(e){ return false; } }
  function stripLmf(){ try{ var h=document.head; if(!h)return; var ls=h.querySelectorAll('link[data-lmf],style[data-lmf],script[data-lmf]'); for(var i=0;i<ls.length;i++){ ls[i].parentNode.removeChild(ls[i]); } }catch(e){} }
  function waitForHead(cb){
    var tries=0;
    (function poll(){
      if(document.head||document.documentElement||tries++>100) cb();
      else setTimeout(poll,20);
    })();
  }
  function headOf(doc){ return doc.head||doc.documentElement; }
  function addCSS(doc,href){
    var h=headOf(doc);
    if(!href||!h||h.querySelector('link[data-lmf="'+href+'"]'))return;
    var l=doc.createElement('link');
    l.rel='stylesheet';l.href=href;
    l.setAttribute('data-lmf',href);
    h.appendChild(l);
  }
  function addStyle(doc,css,id){
    var h=headOf(doc);
    if(!css||!h)return;
    if(id&&h.querySelector('#'+id))return;
    var s=doc.createElement('style');
    if(id)s.id=id;
    s.setAttribute('data-lmf',id||'css');
    s.textContent=css;h.appendChild(s);
  }
  function addJS(doc,src){
    var h=headOf(doc);
    if(!src||!h||h.querySelector('script[data-lmf="'+src+'"]'))return;
    var s=doc.createElement('script');
    s.type='module';s.src=src;
    s.setAttribute('data-lmf',src);
    h.appendChild(s);
  }
  function addInlineJS(doc,txt,id){
    var h=headOf(doc);
    if(!txt||!h)return;
    if(id&&h.querySelector('#'+id))return;
    var s=doc.createElement('script');
    if(id)s.id=id;
    s.setAttribute('data-lmf',id||'js');
    s.textContent=txt;h.appendChild(s);
  }
  function winClasses(doc){
    var out=[];
    var h=(doc.documentElement&&doc.documentElement.className)||'';
    var b=(doc.body&&doc.body.className)||'';
    (h+' '+b).split(/\s+/).forEach(function(tk){ if(tk) out.push('.'+tk); });
    return out;
  }
  function reTest(p,t){ try{ return new RegExp(p).test(t); }catch(e){ return t.indexOf(p)>-1; } }
  function clsTest(p,cl){ for(var i=0;i<cl.length;i++){ if(cl[i].indexOf(p)>-1) return true; } return false; }
  function matchWin(p,t,cl,alias){
    if(p==='.*') return true;
    if(reTest(p,t)||clsTest(p,cl)) return true;
    if(alias&&p==='^Steam$'&&reTest('^Steam Games List$',t)) return true;
    return false;
  }
  function applyWindow(doc){
    if(!doc)return;
    var P=window.__lumaCSS||null;
    if(P&&P.webkit)addStyle(doc,P.webkit,'LmfWebkit');
    var t=doc.title||'';
    var cl=winClasses(doc);
    var _p=(P&&P.patches)?P.patches:PATCHES_PLACEHOLDER;
    for(var i=0;i<_p.length;i++){
      var p=_p[i];
        if(matchWin(p.r,t,cl,true)){
          if(P){ var pc=p.css||(P.files&&p.f!=null?P.files[p.f]:null); if(pc)addStyle(doc,pc,'LmfP'+i); if(p.ju)addJS(doc,p.ju); else if(p.js)addInlineJS(doc,p.js,'LmfPJ'+i); }
          else{ if(p.c)addCSS(doc,p.c); if(p.j)addJS(doc,p.j); }
        }
    }
    var _c=(P&&P.conds)?P.conds:CONDITION_CSS_PLACEHOLDER;
    for(var j=0;j<_c.length;j++){
      var c=_c[j];
      var ok=false;
      for(var k=0;k<c.affects.length;k++){ if(matchWin(c.affects[k],t,cl,false)){ ok=true; break; } }
      if(ok){
        if(c.css)addStyle(doc,c.css,'LmfC'+j);
        else if(c.url)addCSS(doc,c.url);
      }
    }
    var _jd=(P&&P.cjs)?P.cjs:CONDITION_JS_PLACEHOLDER;
    for(var m=0;m<_jd.length;m++){
      var d=_jd[m];
      var ok2=false;
      for(var n=0;n<d.affects.length;n++){ if(matchWin(d.affects[n],t,cl,false)){ ok2=true; break; } }
      if(ok2){
        if(d.ju)addJS(doc,d.ju);
        else if(d.js)addInlineJS(doc,d.js,'LmfJ'+m);
        else if(d.url)addJS(doc,d.url);
      }
    }
  }
  function injectDoc(doc){
    var P=window.__lumaCSS||null;
    addStyle(doc,'ACCENT_PLACEHOLDER','SystemAccentColorInject');
    addStyle(doc,'ROOTCOLORS_PLACEHOLDER','RootColors');
    addStyle(doc,'SSH_BRIDGE_PLACEHOLDER','LumaSshBridge');
    if(P&&P.webkit){ addStyle(doc,P.webkit,'LmfWebkit'); }
    else{ addCSS(doc,'WEBKITCSS_PLACEHOLDER'); }
    if(P&&P.webkitjs)addJS(doc,P.webkitjs);
    addStyle(doc,'SLIDER_PLACEHOLDER','MillenniumSliderConditions');
    applyWindow(doc);
    // Runtime plugin injection (library window). Store pages already receive
    // plugins via the HTML intercept; here we load via <script src> from the
    // VFS (no CORS concerns) when location.href matches the plugin's
    // runtimeTargetUrl (e.g. "steamloopback.host").
    try{
      var _href=''; try{_href=location.href||'';}catch(e){}
      for(var _pi=0;_pi<_lumaPlugins.length;_pi++){(function(pl){
        if(!pl||!pl.u||!pl.t||!pl.t.length)return;
        var m=false;
        for(var _z=0;_z<pl.t.length;_z++){ if(_href.indexOf(pl.t[_z])>-1){m=true;break;} }
        if(!m)return;
        var sid='LumaPlugin_'+pl.i;
        if(doc.getElementById(sid))return;
        var h=headOf(doc);
        if(!h)return;
        var s=doc.createElement('script');
        s.id=sid;
        s.src='https://lumaforge.local/plugins/'+pl.u;
        s.setAttribute('data-lmf',sid);
        h.appendChild(s);
      })(_lumaPlugins[_pi]);}
    }catch(e){}
  }
  function watchDoc(doc){
    try{
      if(doc.__lumaWatch)return; doc.__lumaWatch=1;
      var titleEl=doc.querySelector('title');
      if(titleEl){
        new MutationObserver(function(){ applyWindow(doc); }).observe(titleEl,{childList:true,subtree:true,characterData:true});
      }
      if(doc.documentElement){
        new MutationObserver(function(){ applyWindow(doc); }).observe(doc.documentElement,{attributes:true,attributeFilter:['class']});
      }
      if(doc.body){
        new MutationObserver(function(){ applyWindow(doc); }).observe(doc.body,{attributes:true,attributeFilter:['class']});
      }
      setTimeout(function(){ applyWindow(doc); },500);
      setTimeout(function(){ applyWindow(doc); },2000);
      var obs2=new MutationObserver(function(){
        if(doc.head&&!doc.head._lumaObs){
          doc.head._lumaObs=1;
          injectDoc(doc);
        }
      });
      obs2.observe(doc.documentElement||doc,{childList:true,subtree:true});
    }catch(e){}
  }
  function startInjection(){
    waitForHead(function(){
      if(isSharedCtx()) return;
      injectDoc(document);
      watchDoc(document);
    });
  }
  // ── SharedJSContext popup patcher (Millennium-style) ──
  // Steam's client UI (library, topbar, footer, menus, supernavs) lives in
  // popup windows created by SharedJSContext, reachable via
  // g_PopupManager.GetPopups(). Inject the theme into each popup's document
  // from here — never into Shared's own head (that blocks renderWhenReady).
  function themedDoc(doc,quiet){
    try{
      injectDoc(doc);
      watchDoc(doc);
      if(!quiet) console.log('[LUMA] popup themed: '+(doc.title||'?'));
      return true;
    }catch(e){ return false; }
  }
  function themedPopup(win){
    try{
      if(!win||!win.document)return;
      var doc=win.document;
      if(doc.title==='SharedJSContext')return;
      if(win.__luma_lmf_done){
        // Re-apply silently if a navigation wiped our nodes (SPA doc swap).
        if(doc.readyState==='complete'&&!doc.querySelector('link[data-lmf],style[data-lmf]')&&!doc.querySelector('#RootColors')){
          themedDoc(doc,true);
        }
        return;
      }
      // Same deferral as the popup path: document written, Steam's
      // renderWhenReady gate (popup_target ctor) already ran.
      if(doc.readyState!=='complete')return;
      if(!doc.getElementById('popup_target'))return;
      if(!themedDoc(doc))return;
      win.__luma_lmf_done=1;
    }catch(e){}
  }
  function sweepPopups(){
    try{
      if(typeof g_PopupManager==='undefined'||!g_PopupManager)return;
      if(typeof g_PopupManager.GetPopups!=='function')return;
      var it=g_PopupManager.GetPopups();var s;
      while(!(s=it.next()).done){
        var w=null;
        try{ w=s.value&&(s.value.window||null); }catch(e){}
        if(!w){ try{ w=s.value&&(s.value.m_popup&&s.value.m_popup.window)||null; }catch(e2){} }
        if(w)themedPopup(w);
      }
    }catch(e){}
  }
  function ensureCreatedHook(){
    if(window.__luma_cb_hooked)return;
    try{
      if(typeof g_PopupManager==='undefined'||!g_PopupManager)return;
      if(typeof g_PopupManager.AddPopupCreatedCallback!=='function')return;
      g_PopupManager.AddPopupCreatedCallback(function(p){
        try{
          var w=p&&(p.window||p.m_popup&&p.m_popup.window)||null;
          if(w){ themedPopup(w); setTimeout(function(){themedPopup(w);},600); setTimeout(function(){themedPopup(w);},2000); }
        }catch(e){}
      });
      window.__luma_cb_hooked=1;
      console.log('[LUMA] popup created-hook installed');
    }catch(e){}
  }
  function startPatcher(){
    if(window.__luma_patcher)return;
    if(typeof g_PopupManager==='undefined'||!g_PopupManager)return;
    window.__luma_patcher=1;
    ensureCreatedHook();
    sweepPopups();
    setInterval(function(){ ensureCreatedHook(); sweepPopups(); },1000);
    console.log('[LUMA] popup patcher started');
  }
  if(isSharedCtx()){
    // Never theme Shared's own head — Steam mirrors it into popups and a
    // dirty head blocks renderWhenReady. Strip anything present, then patch
    // the popup windows from here instead (Millennium's approach).
    startPatcher();
    [100,500,1500].forEach(function(d){ setTimeout(stripLmf,d); });
    return;
  }
  // Popup documents (about:blank?createflags=... or steamloopback UI windows)
  // are created blank and then document.write()'d by Steam; their
  // renderWhenReady gate tracks <link> elements present right after the
  // write. Adding theme links earlier races that gate (login window never
  // shows). Wait until #popup_target exists (= write finished, Q ctor ran)
  // and the document is fully parsed before injecting.
  var isPopupDoc=(location.protocol==='about:'||location.href.indexOf('createflags')!==-1||location.href.indexOf('steamloopback.host')!==-1);
  if(isPopupDoc){
    var started=false;
    var go=function(){ if(started)return; started=true; startInjection(); };
    var consider=function(){
      // Ready state complete = document.write finished and Steam's
      // renderWhenReady gate already ran; injecting earlier lets our links
      // race that gate and the popup never becomes visible.
      if(document.getElementById('popup_target') && document.readyState==='complete') go();
    };
    consider();
    if(!started){
      window.addEventListener('load',consider);
      try{
        new MutationObserver(consider).observe(document,{childList:true,subtree:true});
      }catch(e){}
      var iv=setInterval(function(){ consider(); if(started) clearInterval(iv); },150);
      setTimeout(go,5000);
    }
  }else{
    startInjection();
  }
  // The script can run at document start (title not parsed yet), so Shared
  // detection and g_PopupManager visibility may both be false right now —
  // retry later. startPatcher is idempotent and self-healing.
  [500,1500,3000,6000].forEach(function(d){ setTimeout(function(){
    if(isSharedCtx()){ stripLmf(); }
    startPatcher();
  },d); });
})();"#);

    // Replace placeholders with actual values
    // 0. System accent colors (--SystemAccentColor*)
    let accent_css = accent::system_accent_css();
    js = js.replace("ACCENT_PLACEHOLDER", &js_escape_str(accent_css));

    // 1. Root colors
    let root_colors_inline = theme_state.root_colors_content.as_deref().unwrap_or("");
    js = js.replace("ROOTCOLORS_PLACEHOLDER", &js_escape_str(root_colors_inline));

    // 1b. ssh-bridge (theme var -> --luma-ssh-* plugin vars)
    js = js.replace("SSH_BRIDGE_PLACEHOLDER", &js_escape_str(SSH_BRIDGE_CSS));

    // 1c. runtime plugin manifest (library-window injection via VFS)
    let mut plugins_json = String::from("[");
    for p in &theme_state.plugins {
        let (Some(vfs), Some(rt)) = (p.vfs_rel.as_ref(), p.runtime_target_url.as_ref()) else {
            continue;
        };
        let safe_id: String = p.name.chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
            .collect();
        if safe_id.is_empty() || vfs.is_empty() || rt.is_empty() {
            continue;
        }
        let entry = json!({ "i": safe_id, "u": vfs.as_str(), "t": [rt.as_str()] });
        plugins_json.push_str(&entry.to_string());
        plugins_json.push(',');
    }
    plugins_json.push(']');
    js = js.replace("PLUGINS_MANIFEST_PLACEHOLDER", &plugins_json);

    // 2. Webkit CSS
    let webkit_url = theme_state.webkit_css_path.as_ref()
        .map(|css| build_vfs_css_url(&theme_dir, css))
        .unwrap_or_default();
    js = js.replace("WEBKITCSS_PLACEHOLDER", &webkit_url);

    // 3. Condition CSS — JSON array with affects for runtime window matching
    let cond_css_entries: Vec<Value> = theme_state
        .condition_css
        .iter()
        .map(|cond| {
            json!({
                "url": build_vfs_css_url(&theme_dir, &cond.src),
                "affects": cond.affects,
            })
        })
        .collect();
    let cond_css_json = serde_json::to_string(&cond_css_entries).unwrap_or_else(|_| "[]".to_string());
    js = js.replace("CONDITION_CSS_PLACEHOLDER", &cond_css_json);

    // 4. Slider CSS
    let slider_escaped = js_escape_str(&theme_state.slider_css);
    js = js.replace("SLIDER_PLACEHOLDER", &slider_escaped);

    // 5. Condition JS — same {url, affects} JSON as condition CSS
    let cond_js_entries: Vec<Value> = theme_state
        .condition_js
        .iter()
        .map(|cond| {
            json!({
                "url": build_vfs_css_url(&theme_dir, &cond.src),
                "affects": cond.affects,
            })
        })
        .collect();
    let cond_js_json = serde_json::to_string(&cond_js_entries).unwrap_or_else(|_| "[]".to_string());
    js = js.replace("CONDITION_JS_PLACEHOLDER", &cond_js_json);

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

    js
}

fn register_theme_injection_script(
    socket: &mut tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<TcpStream>>,
    msg_id: &mut u64,
    theme_state: &ThemeState,
    pending: &mut Vec<Value>,
    inject_existing: bool,
) {
    let js = build_theme_js(theme_state);

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
        inject_into_existing_targets(socket, msg_id, pending, &js);
    }
}

/// Check if a target URL belongs to a real Steam page (not internal UI/popups).
fn is_real_steam_page(url: &str) -> bool {
    url.contains("store.steampowered.com")
        || url.contains("steamcommunity.com")
        || url.starts_with("steam://")
        || url.contains("help.steampowered.com")
        || url.contains("library.steampowered.com")
}

/// Iterate all CDP targets and reload real Steam page targets so that
/// addScriptToEvaluateOnNewDocument scripts re-run on them.
fn inject_into_existing_targets(
    socket: &mut tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<TcpStream>>,
    msg_id: &mut u64,
    pending: &mut Vec<Value>,
    js: &str,
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
    let resp = match recv_cdp_response(socket, pending, *msg_id - 1) {
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

        log_to_temp(&format!(
            "[cef_hook] Existing target: type={} url={}",
            target_type,
            &target_url[..target_url.len().min(140)]
        ));

        if target_type != "page" || target_id.is_empty() {
            continue;
        }

        // Skip our own lumaforge.local VFS targets
        if target_url.contains("lumaforge.local") {
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
        let attach_resp = match recv_cdp_response(socket, pending, *msg_id - 1) {
            Some(r) => r,
            None => {
                log_to_temp(&format!(
                    "[cef_hook] No response attaching to target {} ({})",
                    target_id,
                    &target_url[..target_url.len().min(80)]
                ));
                continue;
            }
        };
        let session_id = attach_resp.get("result")
            .and_then(|r| r.get("sessionId"))
            .and_then(|s| s.as_str())
            .unwrap_or("");

        if session_id.is_empty() {
            log_to_temp(&format!("[cef_hook] Failed to attach to target {}: no session", target_id));
            continue;
        }

        if is_real_steam_page(target_url) {
            // Real web pages: evaluate directly. Reloading used to be needed so
            // addScriptToEvaluateOnNewDocument scripts re-ran, but that
            // registration is sent on the browser endpoint (ignored by CEF) —
            // per-session registration now happens via Target.autoAttach.
            let eval = json!({
                "id": *msg_id,
                "method": "Runtime.evaluate",
                "params": {
                    "expression": js,
                    "awaitPromise": false
                },
                "sessionId": session_id
            });
            *msg_id += 1;
            if send_cdp(socket, &eval) {
                injected_count += 1;
            }
            log_to_temp(&format!("Injected target: {} ({})", &target_url[..target_url.len().min(80)], target_id));
        } else {
            // Internal windows (about:blank popups, steamloopback UI, clientui):
            // evaluate the theme script directly — no reload, since reloading
            // internal UI loops (frameNavigated → re-register) and disrupts the
            // client. The script is idempotent (window guard + data-lmf dedupe).
            let eval = json!({
                "id": *msg_id,
                "method": "Runtime.evaluate",
                "params": {
                    "expression": js,
                    "awaitPromise": false
                },
                "sessionId": session_id
            });
            *msg_id += 1;
            if send_cdp(socket, &eval) {
                injected_count += 1;
            }
        }
    }

    log_to_temp(&format!(
        "[cef_hook] Injected into {} existing targets",
        injected_count
    ));
}

fn register_webkit_js(
    socket: &mut tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<TcpStream>>,
    msg_id: &mut u64,
    theme_state: &ThemeState,
) {
    // Register webkit JS as persistent script if available
    if let Some(ref webkit_js_path) = theme_state.webkit_js_path {
        if fs::metadata(webkit_js_path).map(|m| !m.is_file()).unwrap_or(true) {
            log_to_temp(&format!(
                "[cef_hook] Webkit JS not readable: {}",
                webkit_js_path
            ));
            return;
        }
        // The source is injected as a classic script, so load the theme file
        // as an ES module via dynamic import — themes ship ESM (top-level
        // await / import / export) which SyntaxErrors as inline classic JS.
        let url = build_vfs_css_url(&theme_state.theme_dir_str(), webkit_js_path);
        let url_lit = serde_json::to_string(&url).unwrap_or_else(|_| "\"\"".to_string());
        let code = format!(
            "import({}).catch(function(e){{ try {{ console.log('[LUMA] webkit js error', e); }} catch (_) {{}} }});",
            url_lit
        );
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
        log_to_temp(&format!("[cef_hook] Webkit JS registered for all documents ({})", url));
    }
}

// ─── Fetch handler ──────────────────────────────────────────────────────────

/// Send a Fetch command, echoing the pause event's `sessionId` when it came
/// from a session-scoped interception — interception IDs are only valid in
/// the session that reported them (root-scope fulfill → InvalidInterceptionId).
fn fetch_cmd(
    socket: &mut tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<TcpStream>>,
    msg_id: &mut u64,
    method: &str,
    params: Value,
    sid: Option<&str>,
) -> u64 {
    let id = *msg_id;
    let mut m = json!({ "id": id, "method": method, "params": params });
    if let Some(s) = sid {
        m["sessionId"] = Value::String(s.to_string());
    }
    *msg_id += 1;
    send_cdp(socket, &m);
    id
}

fn handle_fetch_paused(
    socket: &mut tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<TcpStream>>,
    msg: &Value,
    msg_id: &mut u64,
    theme_state: &mut ThemeState,
    pending: &mut Vec<Value>,
    sid: Option<&str>,
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
    if is_request_stage && url.contains(VFS_HOST) {
        log_to_temp(&format!("[cef_hook] FETCH-REQ: {}", url));
    }
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
                fetch_cmd(
                    socket,
                    msg_id,
                    "Fetch.failRequest",
                    json!({"requestId": request_id_str, "errorReason": "Failed"}),
                    sid,
                );
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
        fetch_cmd(
            socket,
            msg_id,
            "Fetch.fulfillRequest",
            json!({
                "requestId": request_id_str,
                "responseCode": 200,
                "responseHeaders": resp_headers,
                "body": body_b64
            }),
            sid,
        );
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
                fetch_cmd(
                    socket,
                    msg_id,
                    "Fetch.fulfillRequest",
                    json!({
                        "requestId": request_id_str,
                        "responseCode": 200,
                        "responseHeaders": resp_headers,
                        "body": body_b64
                    }),
                    sid,
                );
                return;
            }
            Err(()) => {
                // Not a VFS request or file not found, fail it
                fetch_cmd(
                    socket,
                    msg_id,
                    "Fetch.failRequest",
                    json!({"requestId": request_id_str, "errorReason": "NameNotResolved"}),
                    sid,
                );
                return;
            }
        }
    }

    // ── Non-HTML request stage: continue ──
    if is_request_stage {
        fetch_cmd(
            socket,
            msg_id,
            "Fetch.continueRequest",
            json!({"requestId": request_id_str}),
            sid,
        );
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
        fetch_cmd(
            socket,
            msg_id,
            "Fetch.continueResponse",
            json!({"requestId": request_id_str}),
            sid,
        );
        return;
    }

    // ── Skip sensitive URLs where HTML injection can break page functionality ──
    // let skip_injection = lower_url.contains("/login")
    //     || lower_url.contains("login.steampowered")
    //     || lower_url.contains("help.steampowered.com")
    //     || lower_url.contains("/checkout/")
    //     || lower_url.contains("/mobileauth/")
    //     || lower_url.contains("/two_factor/")
    //     || lower_url.contains("/steamguard/")
    //     || lower_url.contains("/imagematch/")
    //     || lower_url.contains("/forgot")
    //     // CDN/video/audio domains — not Steam pages, no need to inject
    //     || lower_url.contains("akamaized.net")
    //     || lower_url.contains("fastly.")
    //     || lower_url.contains("cloudflare")
    //     || lower_url.contains("video.")
    //     || lower_url.contains(".mpd")
    //     || lower_url.contains(".m3u8")
    //     || lower_url.contains("segment/")
    //     || lower_url.contains("/broadcast/");
    // if skip_injection {
    //     let continue_msg = json!({
    //         "id": *msg_id,
    //         "method": "Fetch.continueResponse",
    //         "params": {"requestId": request_id_str}
    //     });
    //     *msg_id += 1;
    //     send_cdp(socket, &continue_msg);
    //     return;
    // }

    // ── HTML interception: inject theme ──
    log_to_temp(&format!("[cef_hook] Intercepting HTML: {}", &url[..url.len().min(120)]));

    let current_id = fetch_cmd(
        socket,
        msg_id,
        "Fetch.getResponseBody",
        json!({"requestId": request_id_str}),
        sid,
    );

    // Helper: always continueResponse on failure so the page doesn't hang
    let do_continue = |socket: &mut tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<TcpStream>>, msg_id: &mut u64| {
        fetch_cmd(
            socket,
            msg_id,
            "Fetch.continueResponse",
            json!({"requestId": request_id_str}),
            sid,
        );
    };

    let body_response = recv_cdp_response(socket, pending, current_id);
    let body_msg = match body_response {
        Some(m) => m,
        None => {
            log_to_temp(&format!("[cef_hook] Timeout getting body, continuing response: {}", &url[..url.len().min(80)]));
            do_continue(socket, msg_id);
            return;
        }
    };

    let result = match body_msg.get("result") {
        Some(r) => r,
        None => {
            log_to_temp(&format!("[cef_hook] No result in body response, continuing: {}", &url[..url.len().min(80)]));
            do_continue(socket, msg_id);
            return;
        }
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

    // Get window identity from the HTML (title + <html>/<body> classes)
    let (window_title, html_class, body_class) = extract_window_identity(&body_str);

    let modified = inject_theme_html(
        &body_str,
        theme_state,
        &window_title,
        &html_class,
        &body_class,
        url,
        &theme_state.plugins.clone(),
        false,
    );

    let encoded = STANDARD.encode(modified.as_bytes());
    let mut fulfill_params = json!({
        "requestId": request_id_str,
        "responseCode": status_code,
        "body": encoded
    });

    // Strip encoding/length headers — CDP returns decoded body
    // Original headers would tell the browser to double-decode
    if let Some(headers) = response_headers {
        if let Value::Array(arr) = &headers {
            let filtered: Vec<Value> = arr.iter().filter(|item| {
                if let Value::Object(map) = item {
                    let name = map.get("name").and_then(|v| v.as_str()).unwrap_or("").to_lowercase();
                    return name != "content-encoding"
                        && name != "content-length"
                        && name != "transfer-encoding";
                }
                true
            }).cloned().collect();
            if let Some(obj) = fulfill_params.as_object_mut() {
                obj.insert("responseHeaders".to_string(), Value::Array(filtered));
            }
        }
    }

    fetch_cmd(socket, msg_id, "Fetch.fulfillRequest", fulfill_params, sid);
}

/// Extract window identity hints from raw HTML: <title> text plus the class
/// attributes of <html> and <body> (used by window_matches, mirroring what
/// Millennium reads from g_PopupManager params html_class/body_class).
fn extract_window_identity(html: &str) -> (String, String, String) {
    let mut title = String::new();
    if let Some(start) = find_ascii_ci(html, "<title>") {
        let content_start = start + 7;
        if content_start < html.len() {
            if let Some(rel_end) = find_ascii_ci(&html[content_start..], "</title>") {
                title = html[content_start..content_start + rel_end].trim().to_string();
            }
        }
    }
    (
        title,
        extract_tag_class(html, "<html"),
        extract_tag_class(html, "<body"),
    )
}

/// Find `class="..."` inside the first occurrence of `tag` (case-insensitive).
fn extract_tag_class(html: &str, tag: &str) -> String {
    let start = match find_ascii_ci(html, tag) {
        Some(s) => s,
        None => return String::new(),
    };
    let tag_end = html[start..].find('>').map(|e| start + e).unwrap_or(html.len());
    let region = &html[start..tag_end];
    let lc_region = region.to_ascii_lowercase();
    if let Some(cs) = lc_region.find("class=\"") {
        let val_start = cs + 7;
        if let Some(ce) = lc_region[val_start..].find('"') {
            return region[val_start..val_start + ce].to_string();
        }
    }
    String::new()
}

// ─── CDP main loop ──────────────────────────────────────────────────────────

/// Dispatch one received CDP message. Events are handled here; plain responses
/// to our fire-and-forget commands are dropped. Messages that arrived while a
/// recv_cdp_response wait was in progress were already queued into `pending`
/// and are re-dispatched through this function by the main loop.
fn dispatch_cdp_message(
    socket: &mut tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<TcpStream>>,
    msg: &Value,
    msg_id: &mut u64,
    theme_state: &mut ThemeState,
    pending: &mut Vec<Value>,
) {
    let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");

    match method {
        "Fetch.requestPaused" => {
            let sid = msg
                .get("sessionId")
                .and_then(|s| s.as_str())
                .map(|s| s.to_string());
            handle_fetch_paused(socket, msg, msg_id, theme_state, pending, sid.as_deref());
        }
        "Runtime.bindingCalled" => {
            let name = msg.get("params").and_then(|p| p.get("name")).and_then(|n| n.as_str()).unwrap_or("");
            let payload = msg.get("params").and_then(|p| p.get("payload")).and_then(|p| p.as_str()).unwrap_or("");
            if name == "__lumaNativeBridge" {
                let sid = msg.get("sessionId").and_then(|s| s.as_str());
                let ecid = msg
                    .get("params")
                    .and_then(|p| p.get("executionContextId"))
                    .and_then(|v| v.as_i64());
                handle_bridge_binding(socket, msg_id, payload, sid, ecid);
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
                load_theme_manifest(theme_state);
                load_plugins(theme_state);
                register_webkit_js(socket, msg_id, theme_state);
                // Re-register only the persistent script — addScriptToEvaluateOnNewDocument
                // handles new documents automatically. We must NOT call
                // inject_into_existing_targets here because it does Page.reload
                // on all targets, causing an infinite reload loop (frameNavigated
                // → reload → frameNavigated → ...).
                register_theme_injection_script(socket, msg_id, theme_state, pending, false);
                // Re-ensure the bridge shim in the session's fresh document.
                // New documents would already be covered by the attach-time
                // registration; evaluate-only here (idempotent) to avoid
                // stacking duplicate addScriptToEvaluateOnNewDocument entries.
                if let Some(sid) = msg.get("sessionId").and_then(|s| s.as_str()) {
                    if !sid.is_empty() {
                        install_bridge_shim_session(socket, msg_id, sid, false);
                    }
                }
                // New document in a payload-carrying session: the window realm
                // is fresh (window.__lumaCSS gone) — re-push the payload. The
                // patcher's delayed re-applies pick it up.
                if let Some(sid) = msg.get("sessionId").and_then(|s| s.as_str()) {
                    let needed = SESSION_URLS
                        .lock()
                        .unwrap()
                        .iter()
                        .any(|(s, _)| s == sid);
                    log_to_temp(&format!(
                        "[cef_hook] frameNav main: sid={} needed={}",
                        sid, needed
                    ));
                    if needed {
                        push_css_payload(socket, msg_id, theme_state, sid);
                    }
                }
            }
        }
        "Target.attachedToTarget" => {
            let params = msg.get("params");
            let raw = params.map(|p| p.to_string()).unwrap_or_default();
            log_to_temp(&format!(
                "[cef_hook] attachedToTarget: {}",
                &raw[..raw.len().min(700)]
            ));
            let sid = params
                .and_then(|p| p.get("sessionId"))
                .and_then(|s| s.as_str())
                .unwrap_or("");
            let tinfo = params.and_then(|p| p.get("targetInfo"));
            let ttype = tinfo.and_then(|t| t.get("type")).and_then(|t| t.as_str()).unwrap_or("");
            let turl = tinfo.and_then(|t| t.get("url")).and_then(|t| t.as_str()).unwrap_or("");
            let tid = tinfo
                .and_then(|t| t.get("targetId"))
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string();
            let waiting = params
                .and_then(|p| p.get("waitingForDebugger"))
                .and_then(|w| w.as_bool())
                .unwrap_or(false);
            log_to_temp(&format!(
                "[cef_hook] attachedToTarget: type={} waiting={} url={}",
                ttype, waiting, turl
            ));

            if sid.is_empty() || tid.is_empty() || turl.contains("lumaforge.local") {
                return;
            }
            if let Ok(mut sessions) = SESSIONS.lock() {
                sessions.retain(|(t, _)| *t != tid);
                sessions.push((tid.clone(), sid.to_string()));
            }
            // Always release the target — CEF can hold a freshly created popup
            // before its renderer starts (login window then never loads and the
            // native window stays hidden forever).
            let resume = json!({
                "id": *msg_id,
                "method": "Runtime.runIfWaitingForDebugger",
                "params": {},
                "sessionId": sid
            });
            *msg_id += 1;
            send_cdp(socket, &resume);

            // Popup targets attach as type=other with an empty URL before their
            // document exists; register the script right away (browser-side, no
            // renderer needed) but skip the evaluate until it's a real page.
            // steamloopback documents (Shared, main window) and popup windows
            // (about:blank/empty target url) can't fetch lumaforge.local — no
            // Fetch interception for them — so push the CSS payload as text
            // before the script eval. Store docs keep the link path (Fetch
            // interception works from that origin). SESSION_URLS doubles as
            // the set of sessions that carry the payload, so frameNavigated
            // and theme-reload know who needs a re-push.
            if ttype == "page"
                && !turl.contains("devtools://")
                && !turl.contains("store.steampowered.com")
            {
                // Request-stage interception must also be enabled per session:
                // the browser-level Fetch.enable above only pauses requests
                // from store-origin documents — steamloopback docs (Shared, main
                // window, popups) never pause, so their module scripts, fonts
                // and images to lumaforge.local fell into DNS and failed. The
                // response-stage "*" pattern stays browser-level only so this
                // session's HTML responses are never rewritten (inject_theme_html
                // must not touch the Steam shell).
                let fetch_enable = json!({
                    "id": *msg_id,
                    "method": "Fetch.enable",
                    "params": {
                        "patterns": [
                            {"urlPattern": format!("*{}*", VFS_HOST), "requestStage": "Request"},
                            {"urlPattern": "*/luma-bridge/*", "requestStage": "Request"}
                        ]
                    },
                    "sessionId": sid
                });
                *msg_id += 1;
                send_cdp(socket, &fetch_enable);
                push_css_payload(socket, msg_id, theme_state, sid);
                if let Ok(mut urls) = SESSION_URLS.lock() {
                    urls.retain(|(s, _)| s != sid);
                    urls.push((sid.to_string(), turl.to_string()));
                }
            }
            register_session_theme(socket, msg_id, theme_state, sid);
            if let Ok(mut reg) = REGISTERED_TARGETS.lock() {
                if !reg.contains(&tid) {
                    reg.push(tid.clone());
                }
            }
            if ttype == "page" && !turl.is_empty() {
                // Native bridge shim for this session. Attach happens at target
                // creation — before the document starts — so register the
                // binding + fetch wrapper now and cover the current document.
                if !turl.contains("devtools://") {
                    install_bridge_shim_session(socket, msg_id, sid, true);
                }
                let js = build_theme_js(theme_state);
                let eval = json!({
                    "id": *msg_id,
                    "method": "Runtime.evaluate",
                    "params": {"expression": js, "awaitPromise": false},
                    "sessionId": sid
                });
                *msg_id += 1;
                send_cdp(socket, &eval);
            }
            log_to_temp(&format!(
                "[cef_hook] Registered theme script for target ({}) {}",
                ttype,
                &turl[..turl.len().min(100)]
            ));
        }
        "Target.targetInfoChanged" => {
            let params = msg.get("params");
            let tinfo = params.and_then(|p| p.get("targetInfo"));
            let ttype = tinfo.and_then(|t| t.get("type")).and_then(|t| t.as_str()).unwrap_or("");
            let turl = tinfo.and_then(|t| t.get("url")).and_then(|t| t.as_str()).unwrap_or("");
            let tid = tinfo
                .and_then(|t| t.get("targetId"))
                .and_then(|t| t.as_str())
                .unwrap_or("");
            if ttype != "page" || turl.is_empty() || turl.contains("lumaforge.local") || tid.is_empty() {
                return;
            }
            {
                let reg = REGISTERED_TARGETS.lock().unwrap();
                if reg.iter().any(|t| t == tid) {
                    return;
                }
            }
            let sid = {
                let sessions = SESSIONS.lock().unwrap();
                sessions.iter().find(|(t, _)| t == tid).map(|(_, s)| s.clone())
            };
            if let Some(sid) = sid {
                let js = build_theme_js(theme_state);
                let eval = json!({
                    "id": *msg_id,
                    "method": "Runtime.evaluate",
                    "params": {"expression": js, "awaitPromise": false},
                    "sessionId": &sid
                });
                *msg_id += 1;
                send_cdp(socket, &eval);
                if let Ok(mut reg) = REGISTERED_TARGETS.lock() {
                    reg.push(tid.to_string());
                }
                log_to_temp(&format!(
                    "[cef_hook] targetInfoChanged → evaluated theme in target {} ({})",
                    tid, turl
                ));
            }
        }
        "Target.detachedFromTarget" => {
            let sid = msg
                .get("params")
                .and_then(|p| p.get("sessionId"))
                .and_then(|s| s.as_str())
                .unwrap_or("");
            if !sid.is_empty() {
                if let Ok(mut sessions) = SESSIONS.lock() {
                    sessions.retain(|(_, s)| s != sid);
                }
            }
        }
        "" => {
            // Response to a fire-and-forget command: surface errors/exceptions
            // (evaluating our script in a target must never fail silently).
            if let Some(err) = msg.get("error") {
                log_to_temp(&format!("[cef_hook] CDP command error: {}", err));
            }
            if let Some(ed) = msg.pointer("/result/exceptionDetails") {
                let s = ed.to_string();
                log_to_temp(&format!("[cef_hook] CDP exception: {}", &s[..s.len().min(300)]));
            }
        }
        _ => {}
    }
}

/// Register the theme script on a per-session Page domain. Browser-level
/// Page.* commands are rejected by this CEF build ("wasn't found"), so every
/// session needs its own registration for future documents.
fn register_session_theme(
    socket: &mut tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<TcpStream>>,
    msg_id: &mut u64,
    theme_state: &ThemeState,
    sid: &str,
) {
    let enable = json!({
        "id": *msg_id,
        "method": "Page.enable",
        "params": {},
        "sessionId": sid
    });
    *msg_id += 1;
    send_cdp(socket, &enable);

    let js = build_theme_js(theme_state);
    let add = json!({
        "id": *msg_id,
        "method": "Page.addScriptToEvaluateOnNewDocument",
        "params": {"source": js, "runImmediately": true},
        "sessionId": sid
    });
    *msg_id += 1;
    send_cdp(socket, &add);
}

/// Periodically discover page targets that acquired a real URL and attach to
/// them so the Target.attachedToTarget handler registers the theme script for
/// their session. about:blank popup targets are skipped on purpose: attaching
/// while they are being created stalls CEF's target creation (the login
/// window's renderer never starts and the native window stays hidden).
fn poll_new_targets(
    socket: &mut tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<TcpStream>>,
    msg_id: &mut u64,
    pending: &mut Vec<Value>,
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
    let resp = match recv_cdp_response(socket, pending, *msg_id - 1) {
        Some(r) => r,
        None => return,
    };
    let targets = match resp.get("result").and_then(|r| r.get("targetInfos")).and_then(|t| t.as_array()) {
        Some(arr) => arr,
        None => return,
    };

    for target in targets {
        let ttype = target.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let tid = target.get("targetId").and_then(|t| t.as_str()).unwrap_or("");
        let url = target.get("url").and_then(|u| u.as_str()).unwrap_or("");
        if ttype != "page" || tid.is_empty() || url.is_empty() {
            continue;
        }
        if url.starts_with("about:")
            || url.starts_with("devtools:")
            || url.contains("lumaforge.local")
        {
            continue;
        }
        {
            let reg = REGISTERED_TARGETS.lock().unwrap();
            if reg.iter().any(|t| t == tid) {
                continue;
            }
        }
        {
            let sessions = SESSIONS.lock().unwrap();
            if sessions.iter().any(|(t, _)| t == tid) {
                continue;
            }
        }
        let attach = json!({
            "id": *msg_id,
            "method": "Target.attachToTarget",
            "params": {"targetId": tid, "flatten": true}
        });
        *msg_id += 1;
        if send_cdp(socket, &attach) {
            log_to_temp(&format!(
                "[cef_hook] Poll: attaching to new target {} ({})",
                tid, url
            ));
        }
    }
}

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
        // Messages received while waiting for a specific response are queued
        // here and dispatched by the main loop (never discarded).
        let mut pending: Vec<Value> = Vec::new();

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

        // NOTE: Target.setAutoAttach is intentionally NOT used. Auto-attaching a
        // popup the instant it is created (type=other, url="") stalls CEF's
        // target creation: the login window's renderer never starts and the
        // native window stays hidden forever. New targets are picked up by the
        // periodic poll instead, only once they have a real URL.

        // Load theme manifest
        let mut theme_state = ThemeState::new();
        load_theme_manifest(&mut theme_state);
        load_plugins(&mut theme_state);

        // Register webkit JS globally
        register_webkit_js(&mut socket, &mut msg_id, &theme_state);

        // Register persistent theme injection for ALL documents (including internal Steam windows)
        register_theme_injection_script(&mut socket, &mut msg_id, &theme_state, &mut pending, true);

        // Bridge shim is installed per-session in Target.attachedToTarget —
        // browser-level Page.* commands are rejected by this CEF build.

        reset_conn_stats();
        let mut lost = false;
        let mut loop_iter = 0u64;
        let mut last_poll = Instant::now();
        while !lost {
            loop_iter += 1;
            if loop_iter % 5 == 0 {
                if check_theme_reload_signal(&mut theme_state) {
                    register_webkit_js(&mut socket, &mut msg_id, &theme_state);
                    register_theme_injection_script(&mut socket, &mut msg_id, &theme_state, &mut pending, true);
                    // Re-push the (rebuilt) CSS payload to every session that
                    // carries it so the patcher sees the new CSS in existing docs.
                    let sids: Vec<String> = SESSION_URLS
                        .lock()
                        .unwrap()
                        .iter()
                        .map(|(s, _)| s.clone())
                        .collect();
                    for sid in &sids {
                        push_css_payload(&mut socket, &mut msg_id, &mut theme_state, sid);
                    }
                }
                load_plugins(&mut theme_state);
            }

            // Dispatch messages that were queued while a recv_cdp_response
            // wait was in progress (Fix: Fetch.requestPaused events must
            // never be dropped or the paused request hangs forever).
            let mut drained = 0usize;
            while !pending.is_empty() && drained < 512 && !lost {
                let queued = pending.remove(0);
                drained += 1;
                dispatch_cdp_message(&mut socket, &queued, &mut msg_id, &mut theme_state, &mut pending);
            }
            if lost {
                break;
            }

            if last_poll.elapsed() >= Duration::from_secs(1) {
                last_poll = Instant::now();
                poll_new_targets(&mut socket, &mut msg_id, &mut pending);
            }

            match socket.read() {
                Ok(Message::Text(text)) => {
                    if let Ok(msg) = serde_json::from_str::<Value>(&text) {
                        note_rx(&msg);
                        dispatch_cdp_message(&mut socket, &msg, &mut msg_id, &mut theme_state, &mut pending);
                    }
                }
                Ok(Message::Close(_)) => {
                    log_to_temp(&format!(
                        "[cef_hook] WebSocket closed, reconnecting... ({})",
                        conn_stats_snapshot()
                    ));
                    lost = true;
                }
                Err(e) => {
                    log_to_temp(&format!(
                        "[cef_hook] WebSocket error: {}, reconnecting... ({})",
                        e,
                        conn_stats_snapshot()
                    ));
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
    log_to_temp(&format!(
        "[cef_hook] DLL loaded into webhelper process (pid={})",
        std::process::id()
    ));

    let port = match resolve_debug_port() {
        Some(p) => p,
        None => {
            log_to_temp("[cef_hook] No debug port found, exiting");
            return 0;
        }
    };

    handle_cdp_connection(port);

    log_to_temp(&format!(
        "[cef_hook] CDP thread exiting (pid={})",
        std::process::id()
    ));
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

#[cfg(test)]
mod window_match_tests {
    use super::window_matches;

    #[test]
    fn title_regex_match() {
        assert!(window_matches("^Steam$", "Steam", "", "", true));
        assert!(window_matches("^Account", "Account Menu", "", "", false));
        assert!(!window_matches("^Steam$", "Account Menu", "", "", true));
    }

    #[test]
    fn steam_games_list_alias_only_for_patches() {
        assert!(window_matches("^Steam$", "Steam Games List", "", "", true));
        assert!(!window_matches("^Steam$", "Steam Games List", "", "", false));
    }

    #[test]
    fn class_token_substring_match() {
        assert!(window_matches(
            ".friendsui-container",
            "Friends List",
            "SomeOtherClass",
            "friendsui-container DesktopUI",
            false
        ));
        assert!(window_matches(
            ".friendsui-container",
            "whatever",
            "client_chat_frame friendsui-container",
            "",
            false
        ));
        assert!(!window_matches(".friendsui-container", "Steam", "", "", true));
    }

    #[test]
    fn dot_star_matches_everything() {
        assert!(window_matches(".*", "", "", "", true));
    }

    #[test]
    fn invalid_regex_falls_back_to_substring() {
        assert!(window_matches("Menu$[", "a Menu$[ b", "", "", false));
    }
}

#[cfg(test)]
mod accent_tests {
    #[test]
    fn accent_css_has_required_vars() {
        let css = super::accent::system_accent_css();
        for var in [
            "--SystemAccentColor:",
            "--SystemAccentColor-RGB:",
            "--SystemAccentColorAccent:",
            "--SystemAccentColorAccent-RGB:",
            "--SystemAccentColorLight1:",
            "--SystemAccentColorDark3:",
        ] {
            assert!(css.contains(var), "missing {} in {}", var, css);
        }
    }
}

#[cfg(test)]
mod import_expand_tests {
    use super::*;

    struct TmpTheme(PathBuf);

    impl TmpTheme {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "lmf_import_test_{}_{}",
                tag,
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(dir.join("elements")).unwrap();
            TmpTheme(dir)
        }

        fn write(&self, rel: &str, content: &str) {
            fs::write(self.0.join(rel), content).unwrap();
        }
    }

    impl Drop for TmpTheme {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn expand(path: &PathBuf, skip: &[PathBuf]) -> String {
        let raw = fs::read_to_string(path).unwrap();
        let base = path.parent().unwrap().to_path_buf();
        // Tests build paths directly under the temp theme root, so the root
        // is the first ancestor named lmf_import_test_*.
        let mut theme_dir = base.clone();
        loop {
            if theme_dir
                .file_name()
                .map(|n| n.to_string_lossy().starts_with("lmf_import_test_"))
                .unwrap_or(false)
            {
                break;
            }
            match theme_dir.parent() {
                Some(p) => theme_dir = p.to_path_buf(),
                None => {
                    theme_dir = base.clone();
                    break;
                }
            }
        }
        let mut stack: Vec<PathBuf> = fs::canonicalize(path).into_iter().collect();
        expand_css_imports(&raw, &base, &theme_dir, skip, &mut stack)
    }

    #[test]
    fn inlines_relative_import_tree() {
        let t = TmpTheme::new("tree");
        t.write(
            "main.css",
            "@import url('./elements/sidebar.css');\n.main{color:red}",
        );
        t.write("elements/sidebar.css", ".sidebar{background:blue}");
        let out = expand(&t.0.join("main.css"), &[]);
        assert!(out.contains(".sidebar{background:blue}"), "child not inlined: {}", out);
        assert!(out.contains(".main{color:red}"), "parent rules lost: {}", out);
        assert!(!out.contains("@import"), "import not removed: {}", out);
    }

    #[test]
    fn keeps_absolute_and_root_relative_imports() {
        let t = TmpTheme::new("abs");
        t.write(
            "main.css",
            "@import url('https://fonts.cdnfonts.com/css/x');\n@import \"/public/a.css\";",
        );
        let out = expand(&t.0.join("main.css"), &[]);
        assert!(out.contains("https://fonts.cdnfonts.com/css/x"));
        assert!(out.contains("/public/a.css"));
        assert!(out.contains("@import"), "absolute imports must stay: {}", out);
    }

    #[test]
    fn skip_drops_import_but_keeps_rest() {
        let t = TmpTheme::new("skip");
        t.write("main.css", "@import url('webkit.css');\n.main{}");
        t.write("webkit.css", ".wk{}");
        let skip = vec![fs::canonicalize(t.0.join("webkit.css")).unwrap()];
        let out = expand(&t.0.join("main.css"), &skip);
        assert!(!out.contains(".wk{}"), "skipped file leaked: {}", out);
        assert!(out.contains(".main{}"));
        assert!(!out.contains("@import"));
    }

    #[test]
    fn cycle_terminates_and_drops_import() {
        let t = TmpTheme::new("cycle");
        t.write("a.css", "@import url('b.css');\n.a{}");
        t.write("b.css", "@import url('a.css');\n.b{}");
        let out = expand(&t.0.join("a.css"), &[]);
        assert!(out.contains(".a{}"));
        assert!(out.contains(".b{}"));
        assert!(!out.contains("@import"), "cycle import must be dropped: {}", out);
    }

    #[test]
    fn missing_import_dropped_without_panic() {
        let t = TmpTheme::new("missing");
        t.write("main.css", "@import url('./nope.css');\n.main{}");
        let out = expand(&t.0.join("main.css"), &[]);
        assert!(out.contains(".main{}"));
        assert!(!out.contains("@import"));
    }

    #[test]
    fn builder_dedupes_shared_files() {
        let t = TmpTheme::new("dedup");
        t.write("lib.css", "@import url('elements/sidebar.css');\n.lib{}");
        t.write("elements/sidebar.css", ".sidebar{}");
        let mut b = PayloadBuilder::new(Vec::new(), t.0.clone());
        let i1 = b.add(&t.0.join("lib.css").to_string_lossy()).unwrap();
        let i2 = b.add(&t.0.join("lib.css").to_string_lossy()).unwrap();
        assert_eq!(i1, i2, "same file must map to one index");
        assert_eq!(b.files.len(), 1);
        assert!(b.files[0].contains(".sidebar{}"));
        assert!(b.files[0].contains(".lib{}"));
    }

    #[test]
    fn rewrites_relative_urls_to_vfs() {
        let t = TmpTheme::new("url");
        t.write(
            "main.css",
            "@font-face{font-family:X;src:url(fonts/x.woff2)}\
             \n.icon{background:url('./images/icon.png')}",
        );
        let out = expand(&t.0.join("main.css"), &[]);
        assert!(
            out.contains("url(\"https://lumaforge.local/themes/fonts/x.woff2\")"),
            "relative url not rewritten: {}",
            out
        );
        assert!(
            out.contains("url(\"https://lumaforge.local/themes/images/icon.png\")"),
            "dot-relative url not rewritten: {}",
            out
        );
        assert!(!out.contains("url(fonts"), "raw url left: {}", out);
        assert!(!out.contains("url('./images"), "raw url left: {}", out);
    }

    #[test]
    fn rewrites_urls_inside_imported_files_with_their_own_dir() {
        let t = TmpTheme::new("urlimport");
        t.write("main.css", "@import url('elements/sub.css');\n.main{}");
        t.write(
            "elements/sub.css",
            ".a{background:url(../img/bg.png)}\n.b{background:url(icon.svg)}",
        );
        let out = expand(&t.0.join("main.css"), &[]);
        assert!(
            out.contains("url(\"https://lumaforge.local/themes/img/bg.png\")"),
            "imported relative url not rewritten: {}",
            out
        );
        assert!(
            out.contains("url(\"https://lumaforge.local/themes/elements/icon.svg\")"),
            "imported sibling url not rewritten: {}",
            out
        );
        assert!(!out.contains("@import"));
    }

    #[test]
    fn keeps_data_absolute_root_and_fragment_urls() {
        let t = TmpTheme::new("urlabs");
        t.write(
            "main.css",
            "a{background:url(data:font/woff2;base64,AAAA)}\
             \nb{background:url(https://cdn.example.com/x.png)}\
             \nc{background:url(/shared/y.png)}\
             \nd{mask:url(#m)}\
             \ne{background:url('//cdn.example.com/z.png')}",
        );
        let out = expand(&t.0.join("main.css"), &[]);
        assert!(out.contains("url(data:font/woff2;base64,AAAA)"), "{}", out);
        assert!(out.contains("url(https://cdn.example.com/x.png)"), "{}", out);
        assert!(out.contains("url(/shared/y.png)"), "{}", out);
        assert!(out.contains("url(#m)"), "{}", out);
        assert!(out.contains("url('//cdn.example.com/z.png')"), "{}", out);
        assert!(
            !out.contains("lumaforge.local"),
            "absolute url was rewritten: {}",
            out
        );
    }

    #[test]
    fn keeps_urls_escaping_theme_root() {
        let t = TmpTheme::new("urlesc");
        t.write("main.css", "a{background:url('../../../../etc/passwd')}");
        let out = expand(&t.0.join("main.css"), &[]);
        assert!(
            out.contains("url('../../../../etc/passwd')"),
            "escaping url must stay untouched: {}",
            out
        );
        assert!(!out.contains("lumaforge.local"), "{}", out);
    }
}

#[cfg(test)]
mod condition_default_tests {
    use super::*;

    #[test]
    fn numeric_condition_default_parses_to_string() {
        let v: Value = serde_json::json!({
            "default": 8,
            "slider": { "cssVariable": "--st-border-radius", "min": 0, "max": 16, "step": 1, "unit": "px" }
        });
        let cond: SkinCondition = serde_json::from_value(v).expect("numeric default must parse");
        assert_eq!(cond.default.as_deref(), Some("8"));
        assert!(cond.slider.is_some());
    }

    #[test]
    fn string_condition_default_parses() {
        let v: Value = serde_json::json!({ "default": "yes", "values": { "no": {}, "yes": {} } });
        let cond: SkinCondition = serde_json::from_value(v).expect("string default must parse");
        assert_eq!(cond.default.as_deref(), Some("yes"));
    }

    #[test]
    fn spacetheme_skin_json_parses_all_conditions() {
        let path = std::path::PathBuf::from(
            r"C:\Users\einey.J4F\AppData\Local\LumaForge\themes\Steam\skin.json",
        );
        if !path.exists() {
            eprintln!("Skipping test: Steam skin.json not installed");
            return;
        }
        let content = std::fs::read_to_string(&path).unwrap();
        let manifest: SkinJson = serde_json::from_str(&content).unwrap();
        assert_eq!(manifest.patches.len(), 14);
        assert!(manifest.root_colors.is_some());
        assert!(manifest.steam_webkit.is_some());
        // Every condition must parse (numeric slider defaults used to drop the
        // whole condition via Option<String>).
        let conds = manifest.conditions.as_ref().and_then(|c| c.as_object()).unwrap();
        assert!(conds.len() > 40, "expected full Conditions block, got {}", conds.len());
        let mut sliders = 0;
        for (name, c) in conds {
            let cond: SkinCondition = serde_json::from_value(c.clone())
                .unwrap_or_else(|e| panic!("condition '{}' failed to parse: {}", name, e));
            if cond.slider.is_some() {
                sliders += 1;
                assert!(cond.default.is_some(), "slider '{}' missing numeric default", name);
            }
        }
        assert_eq!(sliders, 3, "Border radius / Mica Transparency / Max Width");
    }
}
