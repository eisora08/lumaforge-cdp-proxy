use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

/// Write file contents, handling Windows file sharing.
/// On Windows, fs::write may briefly truncate then write. Readers should handle this.
fn safe_write(path: &std::path::Path, content: &str) -> Result<(), String> {
    fs::write(path, content).map_err(|e| format!("write {}: {}", path.display(), e))
}

fn log_to_temp(msg: &str) {
    let Ok(local_appdata) = std::env::var("LOCALAPPDATA") else {
        return;
    };
    let log_path = PathBuf::from(local_appdata)
        .join("LumaForge")
        .join("runtime")
        .join("theme.log");
    let _ = std::fs::create_dir_all(log_path.parent().unwrap());
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        let _ = writeln!(f, "{}", msg);
    }
}

fn themes_base_dir() -> Option<PathBuf> {
    let local_appdata = std::env::var("LOCALAPPDATA").ok()?;
    Some(PathBuf::from(local_appdata).join("LumaForge").join("themes"))
}

fn runtime_dir() -> Option<PathBuf> {
    let local_appdata = std::env::var("LOCALAPPDATA").ok()?;
    Some(PathBuf::from(local_appdata).join("LumaForge").join("runtime"))
}

// ─── Export theme data for cef_hook ─────────────────────────────────────────

/// Write the active theme's resolved data to disk so cef_hook can consume it.
/// Produces: theme-manifest.json (patches, webkit, root_colors, conditions)
/// The cef_hook reads this file instead of trying to parse skin.json itself.
pub fn export_theme_for_cef_hook() -> Result<(), String> {
    let theme = load_active_theme().ok_or("No active theme found")?;
    let runtime = runtime_dir().ok_or("No LOCALAPPDATA")?;
    let _ = fs::create_dir_all(&runtime);

    // Build the manifest JSON for cef_hook
    let mut manifest = serde_json::Map::new();
    manifest.insert("name".into(), serde_json::Value::String(theme.name.clone()));
    manifest.insert("dir".into(), serde_json::Value::String(theme.dir.to_string_lossy().into_owned()));

    // Patches — explicit + auto-generated defaults when UseDefaultPatches is true
    let use_defaults = theme.manifest.use_default_patches.unwrap_or(true);
    let mut patches: Vec<Value> = theme.manifest.patches.iter().map(|p| {
        let mut obj = serde_json::Map::new();
        obj.insert("matchRegex".into(), Value::String(p.match_regex_string.clone()));
        if let Some(ref css) = p.target_css {
            let resolved = theme.resolve_path(css).to_string_lossy().into_owned();
            obj.insert("targetCss".into(), Value::String(resolved));
        }
        if let Some(ref js) = p.target_js {
            let resolved = theme.resolve_path(js).to_string_lossy().into_owned();
            obj.insert("targetJs".into(), Value::String(resolved));
        }
        Value::Object(obj)
    }).collect();

    // Auto-generate default patches for Millennium-compatible themes
    if use_defaults && patches.is_empty() {
        let defaults: Vec<(&str, Option<&str>, Option<&str>)> = vec![
            (".*", Some("elements/config.css"), None),
            ("^Steam$", Some("libraryroot.custom.css"), None),
            ("^Steam$", Some("elements/sidebar.css"), None),
            ("^Steam$", Some("elements/library.css"), None),
            ("^Steam$", Some("elements/gamepage.css"), None),
            ("^Steam$", Some("elements/downloads.css"), None),
            (".friendsui-container", Some("friends.custom.css"), None),
            ("^notificationtoasts_", Some("elements/notifications.css"), None),
            (".*", Some("elements/scrollbar.css"), None),
            (".*", Some("elements/overlay.css"), None),
            (".*", Some("elements/miniprofile.css"), None),
        ];

        for (regex, css_opt, js_opt) in defaults {
            let mut obj = serde_json::Map::new();
            obj.insert("matchRegex".into(), Value::String(regex.to_string()));
            if let Some(css_rel) = css_opt {
                let full = theme.resolve_path(css_rel);
                if full.exists() {
                    obj.insert("targetCss".into(), Value::String(full.to_string_lossy().into_owned()));
                }
            }
            if let Some(js_rel) = js_opt {
                let full = theme.resolve_path(js_rel);
                if full.exists() {
                    obj.insert("targetJs".into(), Value::String(full.to_string_lossy().into_owned()));
                }
            }
            if obj.contains_key("targetCss") || obj.contains_key("targetJs") {
                patches.push(Value::Object(obj));
            }
        }
    }
    manifest.insert("patches".into(), Value::Array(patches));

    // Webkit CSS/JS
    if let Some(ref css) = theme.manifest.webkit_css {
        let resolved = theme.resolve_path(css).to_string_lossy().into_owned();
        manifest.insert("webkitCss".into(), Value::String(resolved));
    }
    if let Some(ref js) = theme.manifest.webkit_js {
        let resolved = theme.resolve_path(js).to_string_lossy().into_owned();
        manifest.insert("webkitJs".into(), Value::String(resolved));
    }

    // Root colors
    if let Some(ref rc) = theme.manifest.root_colors {
        let resolved = theme.resolve_path(rc).to_string_lossy().into_owned();
        manifest.insert("rootColors".into(), Value::String(resolved));
    }

    // Conditions (with resolved paths and user selections)
    let condition_config = ThemeConditionConfig::load();
    let mut conditions_out = serde_json::Map::new();
    for (name, cond) in &theme.manifest.conditions {
        let mut cond_obj = serde_json::Map::new();
        cond_obj.insert("description".into(), Value::String(cond.description.clone()));
        cond_obj.insert("tab".into(), Value::String(cond.tab.clone()));
        cond_obj.insert("section".into(), Value::String(cond.section.clone()));
        cond_obj.insert("default".into(), cond.default.clone());

        if let Some(ref values) = cond.values {
            let selected = condition_config.get_selection(&theme.name, name, &cond.default);
            cond_obj.insert("selectedValue".into(), Value::String(selected.clone()));

            let mut values_out = serde_json::Map::new();
            for (val_name, val) in values {
                let mut val_obj = serde_json::Map::new();
                if let Some(ref target_css) = val.target_css {
                    let mut css_obj = serde_json::Map::new();
                    if let Some(ref src) = target_css.src {
                        let resolved = theme.resolve_path(src).to_string_lossy().into_owned();
                        css_obj.insert("src".into(), Value::String(resolved));
                    }
                    css_obj.insert("affects".into(), Value::Array(
                        target_css.affects.iter().map(|a| Value::String(a.clone())).collect()
                    ));
                    val_obj.insert("targetCss".into(), Value::Object(css_obj));
                }
                if let Some(ref target_js) = val.target_js {
                    let mut js_obj = serde_json::Map::new();
                    if let Some(ref src) = target_js.src {
                        let resolved = theme.resolve_path(src).to_string_lossy().into_owned();
                        js_obj.insert("src".into(), Value::String(resolved));
                    }
                    js_obj.insert("affects".into(), Value::Array(
                        target_js.affects.iter().map(|a| Value::String(a.clone())).collect()
                    ));
                    val_obj.insert("targetJs".into(), Value::Object(js_obj));
                }
                values_out.insert(val_name.clone(), Value::Object(val_obj));
            }
            cond_obj.insert("values".into(), Value::Object(values_out));
        }

        if let Some(ref slider) = cond.slider {
            let mut slider_obj = serde_json::Map::new();
            slider_obj.insert("cssVariable".into(), Value::String(slider.css_variable.clone()));
            slider_obj.insert("min".into(), Value::from(slider.min));
            slider_obj.insert("max".into(), Value::from(slider.max));
            slider_obj.insert("step".into(), Value::from(slider.step));
            slider_obj.insert("unit".into(), Value::String(slider.unit.clone()));

            // Compute current value from selection or default
            let current_str = condition_config.get_selection(&theme.name, name, &cond.default);
            let current_val: f64 = current_str.parse().unwrap_or_else(|_| {
                match &cond.default {
                    Value::Number(n) => n.as_f64().unwrap_or(slider.min),
                    _ => slider.min,
                }
            });
            slider_obj.insert("currentValue".into(), Value::from(current_val));
            cond_obj.insert("slider".into(), Value::Object(slider_obj));
        }

        conditions_out.insert(name.clone(), Value::Object(cond_obj));
    }
    manifest.insert("conditions".into(), Value::Object(conditions_out));

    // UseDefaultPatches
    manifest.insert("useDefaultPatches".into(),
        Value::Bool(theme.manifest.use_default_patches.unwrap_or(true)));

    let manifest_path = runtime.join("theme-manifest.json");
    let json_str = serde_json::to_string_pretty(&Value::Object(manifest))
        .map_err(|e| format!("serialize: {}", e))?;
    fs::write(&manifest_path, &json_str)
        .map_err(|e| format!("write {}: {}", manifest_path.display(), e))?;

    // Also write the theme dir path for VFS resolution
    let theme_dir_path = runtime.join("theme-active-dir.txt");
    fs::write(&theme_dir_path, theme.dir.to_string_lossy().as_ref())
        .map_err(|e| format!("write {}: {}", theme_dir_path.display(), e))?;

    log_to_temp(&format!(
        "[theme] Exported theme manifest for cef_hook: {} patches, webkit={}",
        theme.manifest.patches.len(),
        theme.manifest.webkit_css.is_some() || theme.manifest.webkit_js.is_some(),
    ));

    Ok(())
}

// ─── Active.json ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct ActiveThemeFile {
    pub theme: String,
}

pub fn read_active_theme_name() -> Option<String> {
    let base = themes_base_dir()?;
    let active_path = base.join("active.json");
    let raw = fs::read_to_string(&active_path).ok()?;
    // Strip UTF-8 BOM if present
    let content = raw.trim_start_matches('\u{FEFF}');

    // Try Millennium format first: {"themes": {"activeTheme": "name", ...}}
    if let Ok(parsed) = serde_json::from_str::<Value>(content) {
        if let Some(theme_name) = parsed.get("themes")
            .and_then(|t| t.get("activeTheme"))
            .and_then(|v| v.as_str())
        {
            return Some(theme_name.to_string());
        }
    }

    // Fallback: legacy format {"theme": "name"}
    if let Ok(parsed) = serde_json::from_str::<ActiveThemeFile>(content) {
        return Some(parsed.theme);
    }

    None
}

pub fn write_active_theme_name(name: &str) -> Result<(), String> {
    let base = themes_base_dir().ok_or("No LOCALAPPDATA")?;
    let active_path = base.join("active.json");
    let raw = fs::read_to_string(&active_path).unwrap_or_default();
    let content = raw.trim_start_matches('\u{FEFF}');

    // Try to update Millennium format in-place
    if let Ok(mut parsed) = serde_json::from_str::<Value>(content) {
        if let Some(themes) = parsed.get_mut("themes") {
            if let Some(obj) = themes.as_object_mut() {
                obj.insert("activeTheme".into(), Value::String(name.to_string()));
                let json = serde_json::to_string_pretty(&parsed).map_err(|e| format!("serialize: {}", e))?;
                safe_write(&active_path, &json)?;
                crate::log_to_temp(&format!("[theme] Active theme set to: {} (Millennium format)", name));
                return Ok(());
            }
        }
    }

    // Fallback: legacy format
    let json = serde_json::json!({ "theme": name });
    safe_write(&active_path, &serde_json::to_string_pretty(&json).unwrap_or_default())?;
    crate::log_to_temp(&format!("[theme] Active theme set to: {} (legacy format)", name));
    Ok(())
}

// ─── skin.json structs ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct SkinJson {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub header_image: String,
    #[serde(default)]
    pub splash_image: String,

    #[serde(default)]
    pub github: Option<GitHubInfo>,
    #[serde(default)]
    pub funding: Option<Value>,

    /// Path to CSS file with :root variables (relative to theme dir)
    #[serde(default, alias = "RootColors")]
    pub root_colors: Option<String>,

    /// Path to CSS injected into ALL Steam documents (relative to theme dir)
    #[serde(default, alias = "webkitCSS", alias = "Steam-WebKit")]
    pub webkit_css: Option<String>,

    /// Path to JS injected into ALL Steam documents (relative to theme dir)
    #[serde(default, alias = "webkitJS")]
    pub webkit_js: Option<String>,

    /// Whether to merge with Millennium's default patches
    #[serde(default, alias = "UseDefaultPatches")]
    pub use_default_patches: Option<bool>,

    /// Patches: per-window CSS/JS injection rules
    #[serde(default, alias = "Patches")]
    pub patches: Vec<ThemePatch>,

    /// Conditions: user-configurable options (dropdowns, sliders)
    #[serde(default, alias = "Conditions")]
    pub conditions: HashMap<String, Condition>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GitHubInfo {
    #[serde(default)]
    pub owner: String,
    #[serde(default)]
    pub repo_name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ThemePatch {
    /// Regex matched against window title
    #[serde(default, alias = "MatchRegexString")]
    pub match_regex_string: String,
    /// CSS file path relative to theme dir
    #[serde(default, alias = "TargetCss")]
    pub target_css: Option<String>,
    /// JS file path relative to theme dir
    #[serde(default, alias = "TargetJs")]
    pub target_js: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Condition {
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub default: Value,
    #[serde(default)]
    pub tab: String,
    #[serde(default)]
    pub section: String,

    /// Dropdown condition: maps value name to its config
    #[serde(default)]
    pub values: Option<HashMap<String, ConditionValue>>,

    /// Slider condition
    #[serde(default)]
    pub slider: Option<SliderConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConditionValue {
    #[serde(default, alias = "TargetCss")]
    pub target_css: Option<ConditionTarget>,
    #[serde(default, alias = "TargetJs")]
    pub target_js: Option<ConditionTarget>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConditionTarget {
    /// CSS/JS file path relative to theme dir
    #[serde(default)]
    pub src: Option<String>,
    /// URL patterns this applies to (regex strings)
    #[serde(default)]
    pub affects: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SliderConfig {
    #[serde(alias = "cssVariable")]
    pub css_variable: String,
    #[serde(default)]
    pub min: f64,
    #[serde(default)]
    pub max: f64,
    #[serde(default)]
    pub step: f64,
    #[serde(default)]
    pub unit: String,
}

// ─── Loaded theme (fully resolved) ──────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct LoadedTheme {
    pub name: String,
    pub dir: PathBuf,
    pub manifest: SkinJson,
}

impl LoadedTheme {
    /// Resolve a relative path from skin.json against the theme directory
    pub fn resolve_path(&self, relative: &str) -> PathBuf {
        // Strip leading "./" if present
        let clean = relative.strip_prefix("./").unwrap_or(relative);
        self.dir.join(clean)
    }

    /// Read a theme file as string
    pub fn read_file(&self, relative: &str) -> Option<String> {
        let path = self.resolve_path(relative);
        fs::read_to_string(&path).ok()
    }

    /// Read a theme file as bytes
    pub fn read_file_bytes(&self, relative: &str) -> Option<Vec<u8>> {
        let path = self.resolve_path(relative);
        fs::read(&path).ok()
    }

    /// Check if a theme file exists
    pub fn file_exists(&self, relative: &str) -> bool {
        let path = self.resolve_path(relative);
        path.exists() && path.is_file()
    }

    /// Get all patch CSS files that match a given window title
    pub fn matching_css_patches(&self, window_title: &str) -> Vec<PathBuf> {
        self.manifest
            .patches
            .iter()
            .filter(|p| regex_matches(&p.match_regex_string, window_title))
            .filter_map(|p| p.target_css.as_ref())
            .map(|css| self.resolve_path(css))
            .filter(|p| p.exists())
            .collect()
    }

    /// Get all patch JS files that match a given window title
    pub fn matching_js_patches(&self, window_title: &str) -> Vec<PathBuf> {
        self.manifest
            .patches
            .iter()
            .filter(|p| regex_matches(&p.match_regex_string, window_title))
            .filter_map(|p| p.target_js.as_ref())
            .map(|js| self.resolve_path(js))
            .filter(|p| p.exists())
            .collect()
    }

    /// Get the webkit CSS path (global injection)
    pub fn webkit_css_path(&self) -> Option<PathBuf> {
        let rel = self.manifest.webkit_css.as_ref()?;
        let path = self.resolve_path(rel);
        if path.exists() { Some(path) } else { None }
    }

    /// Get the webkit JS path (global injection)
    pub fn webkit_js_path(&self) -> Option<PathBuf> {
        let rel = self.manifest.webkit_js.as_ref()?;
        let path = self.resolve_path(rel);
        if path.exists() { Some(path) } else { None }
    }

    /// Get the root colors CSS path
    pub fn root_colors_path(&self) -> Option<PathBuf> {
        let rel = self.manifest.root_colors.as_ref()?;
        let path = self.resolve_path(rel);
        if path.exists() { Some(path) } else { None }
    }
}

// ─── Theme discovery ────────────────────────────────────────────────────────

/// List all available theme directory names
pub fn list_available_themes() -> Vec<String> {
    let base = match themes_base_dir() {
        Some(b) => b,
        None => return vec![],
    };
    let mut themes = Vec::new();
    if let Ok(entries) = fs::read_dir(&base) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let skin_path = path.join("skin.json");
                if skin_path.exists() {
                    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                        themes.push(name.to_string());
                    }
                }
            }
        }
    }
    themes.sort();
    themes
}

/// Load a theme by directory name
pub fn load_theme(name: &str) -> Option<LoadedTheme> {
    let base = themes_base_dir()?;
    let dir = base.join(name);
    let skin_path = dir.join("skin.json");

    if !skin_path.exists() {
        return None;
    }

    let raw_content = match fs::read_to_string(&skin_path) {
        Ok(c) => c,
        Err(_) => return None,
    };
    // Strip UTF-8 BOM if present
    let content = raw_content.trim_start_matches('\u{FEFF}');

    match serde_json::from_str::<SkinJson>(content) {
        Ok(manifest) => {
            log_to_temp(&format!(
                "[theme] Loaded theme '{}': {} patches, {} conditions",
                name, manifest.patches.len(), manifest.conditions.len(),
            ));
            Some(LoadedTheme {
                name: name.to_string(),
                dir,
                manifest,
            })
        }
        Err(_) => None,
    }
}

/// Load the currently active theme
pub fn load_active_theme() -> Option<LoadedTheme> {
    let name = read_active_theme_name()?;
    load_theme(&name)
}

// ─── Condition config persistence ───────────────────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ThemeConditionConfig {
    /// theme_name -> condition_name -> selected_value
    #[serde(default)]
    pub selections: HashMap<String, HashMap<String, String>>,
}

impl ThemeConditionConfig {
    pub fn load() -> Self {
        // Try reading from Millennium's active.json first
        let base = match themes_base_dir() {
            Some(b) => b,
            None => return Self::default(),
        };
        let active_path = base.join("active.json");
        if let Ok(raw) = fs::read_to_string(&active_path) {
            let content = raw.trim_start_matches('\u{FEFF}');
            if let Ok(parsed) = serde_json::from_str::<Value>(content) {
                if let Some(conditions) = parsed.get("themes")
                    .and_then(|t| t.get("conditions"))
                    .and_then(|c| c.as_object())
                {
                    let selections: HashMap<String, HashMap<String, String>> = conditions
                        .iter()
                        .map(|(theme, theme_conds)| {
                            let conds: HashMap<String, String> = theme_conds
                                .as_object()
                                .map(|m| m.iter()
                                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                                    .collect())
                                .unwrap_or_default();
                            (theme.clone(), conds)
                        })
                        .collect();
                    if !selections.is_empty() {
                        return Self { selections };
                    }
                }
            }
        }

        // Fallback to theme-conditions.json
        let runtime = match runtime_dir() {
            Some(r) => r,
            None => return Self::default(),
        };
        let path = runtime.join("theme-conditions.json");
        let content = match fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => return Self::default(),
        };
        serde_json::from_str(&content).unwrap_or_default()
    }

    pub fn save(&self) -> Result<(), String> {
        // Write back to active.json (Millennium format)
        let base = themes_base_dir().ok_or("No themes base dir")?;
        let active_path = base.join("active.json");

        let raw = fs::read_to_string(&active_path).map_err(|e| format!("read active.json: {}", e))?;
        let content = raw.trim_start_matches('\u{FEFF}');
        let mut parsed: Value = serde_json::from_str(&content)
            .map_err(|e| format!("parse active.json: {}", e))?;

        // Build the conditions object from our selections
        let mut conditions_obj = serde_json::Map::new();
        for (theme, theme_conds) in &self.selections {
            let conds: serde_json::Map<String, Value> = theme_conds
                .iter()
                .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                .collect();
            conditions_obj.insert(theme.clone(), Value::Object(conds));
        }

        // Insert into themes.conditions
        if let Some(themes) = parsed.get_mut("themes") {
            themes["conditions"] = Value::Object(conditions_obj);
        }

        let json = serde_json::to_string_pretty(&parsed).map_err(|e| format!("serialize: {}", e))?;
        safe_write(&active_path, &json)
    }

    /// Get the selected value for a condition, falling back to default
    pub fn get_selection(&self, theme: &str, condition: &str, default: &Value) -> String {
        self.selections
            .get(theme)
            .and_then(|c| c.get(condition))
            .cloned()
            .unwrap_or_else(|| match default {
                Value::String(s) => s.clone(),
                Value::Number(n) => n.to_string(),
                _ => "default".to_string(),
            })
    }

    /// Set a selection
    pub fn set_selection(&mut self, theme: &str, condition: &str, value: &str) {
        self.selections
            .entry(theme.to_string())
            .or_insert_with(HashMap::new)
            .insert(condition.to_string(), value.to_string());
    }
}

// ─── Regex matching ─────────────────────────────────────────────────────────

fn regex_matches(pattern: &str, text: &str) -> bool {
    if pattern == ".*" {
        return true;
    }
    match regex::Regex::new(pattern) {
        Ok(re) => re.is_match(text),
        Err(_) => text.contains(pattern),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_regex_literal() {
        assert!(regex_matches("Steam", "Steam"));
        assert!(regex_matches("Steam", "Steam Overlay")); // substring match
    }

    #[test]
    fn test_regex_anchored() {
        assert!(regex_matches("^Steam$", "Steam"));
        assert!(!regex_matches("^Steam$", "Steam Overlay"));
    }

    #[test]
    fn test_regex_wildcard() {
        assert!(regex_matches(".*", "anything"));
        assert!(regex_matches("^Steam.*", "Steam Overlay"));
        assert!(regex_matches("^notificationtoasts_", "notificationtoasts_123"));
    }

    #[test]
    fn test_regex_url_pattern() {
        assert!(regex_matches(
            "https://.*\\.steampowered\\.com(/.*)?",
            "https://store.steampowered.com/app/12345"
        ));
    }

    #[test]
    fn test_regex_css_class() {
        assert!(regex_matches(".*friendsui-container.*", ".friendsui-container"));
        assert!(regex_matches(".*ModalDialogPopup.*", "SomeModalDialogPopup"));
    }

    #[test]
    fn test_parse_skin_json_real() {
        let skin_json_path = std::path::PathBuf::from(
            r"C:\Users\einey.J4F\AppData\Local\LumaForge\themes\Steam\skin.json"
        );
        if !skin_json_path.exists() {
            eprintln!("Skipping test: skin.json not found");
            return;
        }
        let content = std::fs::read_to_string(&skin_json_path).unwrap();
        let manifest: SkinJson = serde_json::from_str(&content).unwrap();

        assert_eq!(manifest.name, "SpaceTheme for Steam");
        assert!(!manifest.patches.is_empty(), "Should have patches");
        assert!(!manifest.conditions.is_empty(), "Should have conditions");
        assert!(manifest.root_colors.is_some(), "Should have root_colors");
        assert!(manifest.webkit_css.is_some(), "Should have webkit_css");
        assert_eq!(manifest.use_default_patches, None);

        // Verify patches have valid structure
        for patch in &manifest.patches {
            assert!(!patch.match_regex_string.is_empty());
            assert!(patch.target_css.is_some() || patch.target_js.is_some());
        }

        // Verify conditions have valid structure
        for (name, cond) in &manifest.conditions {
            assert!(!cond.description.is_empty() || !name.is_empty());
            if let Some(ref values) = cond.values {
                assert!(!values.is_empty());
            }
            if let Some(ref slider) = cond.slider {
                assert!(!slider.css_variable.is_empty());
            }
        }

        println!("Parsed skin.json: {} patches, {} conditions",
            manifest.patches.len(), manifest.conditions.len());
    }
}
