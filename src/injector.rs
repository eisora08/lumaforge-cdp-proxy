use crate::cdp::Target;
use crate::transport::Transport;
use regex::Regex;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::sync::{Mutex, OnceLock};

#[cfg(target_os = "windows")]
fn load_enabled_plugins() -> Result<Vec<crate::plugin::LoadedPlugin>, String> {
    crate::plugin_loader::load_enabled_plugins()
}

#[cfg(target_os = "linux")]
fn load_enabled_plugins() -> Result<Vec<crate::plugin::LoadedPlugin>, String> {
    crate::plugin_loader_linux::load_enabled_plugins()
}

/// Identifiers of the `Page.addScriptToEvaluateOnNewDocument` entries
/// registered for a target by the previous inject cycle. Kept so the next
/// cycle can remove them before re-adding: without removal every cycle
/// stacked another copy and each new document evaluated the script once per
/// stacked entry (64 duplicate extension injections per session).
struct RegisteredScripts {
    bridge: Option<String>,
    plugins: Vec<(String, Option<String>)>,
    theme: Option<String>,
}

fn registered_scripts() -> &'static Mutex<HashMap<String, RegisteredScripts>> {
    static CELL: OnceLock<Mutex<HashMap<String, RegisteredScripts>>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(HashMap::new()))
}

// â”€â”€â”€ Theme data (parsed from theme-manifest.json) â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

pub struct ThemePatchEntry {
    pub match_regex: String,
    pub target_css: Option<String>,
    pub target_js: Option<String>,
}

pub struct ThemeConditionEntry {
    pub affects: Vec<String>,
    pub src: String,
}

/// Everything the injector needs for one injection pass.
pub struct ThemeBundle {
    pub dir: String,
    pub patches: Vec<ThemePatchEntry>,
    pub webkit_css: Option<String>,
    pub webkit_js: Option<String>,
    pub root_colors: Option<String>,
    pub condition_css: Vec<ThemeConditionEntry>,
    pub condition_js: Vec<ThemeConditionEntry>,
    pub slider_css: String,
}

impl ThemeBundle {
    pub fn is_empty(&self) -> bool {
        self.patches.is_empty()
            && self.webkit_css.is_none()
            && self.webkit_js.is_none()
            && self.root_colors.is_none()
            && self.condition_css.is_empty()
            && self.condition_js.is_empty()
            && self.slider_css.is_empty()
    }
}

/// Load theme data from theme-manifest.json written by theme.rs
pub fn load_theme_patches() -> ThemeBundle {
    let manifest_path = crate::platform::runtime_dir().join("theme-manifest.json");

    let content = match fs::read_to_string(&manifest_path) {
        Ok(c) => c,
        Err(_) => {
            crate::log_to_temp("[steamcdp] No theme-manifest.json found");
            return ThemeBundle {
                dir: String::new(),
                patches: Vec::new(),
                webkit_css: None,
                webkit_js: None,
                root_colors: None,
                condition_css: Vec::new(),
                condition_js: Vec::new(),
                slider_css: String::new(),
            };
        }
    };

    let manifest: Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(e) => {
            crate::log_to_temp(&format!(
                "[steamcdp] Failed to parse theme-manifest.json: {}",
                e
            ));
            return ThemeBundle {
                dir: String::new(),
                patches: Vec::new(),
                webkit_css: None,
                webkit_js: None,
                root_colors: None,
                condition_css: Vec::new(),
                condition_js: Vec::new(),
                slider_css: String::new(),
            };
        }
    };

    let theme_dir = manifest
        .get("dir")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let mut patches = Vec::new();
    if let Some(patches_arr) = manifest.get("patches").and_then(|p| p.as_array()) {
        for patch_val in patches_arr {
            let match_regex = patch_val
                .get("matchRegex")
                .and_then(|v| v.as_str())
                .unwrap_or(".*")
                .to_string();
            let target_css = patch_val
                .get("targetCss")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let target_js = patch_val
                .get("targetJs")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            if target_css.is_some() || target_js.is_some() {
                patches.push(ThemePatchEntry {
                    match_regex,
                    target_css,
                    target_js,
                });
            }
        }
    }

    let webkit_css = manifest
        .get("webkitCss")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let webkit_js = manifest
        .get("webkitJs")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let root_colors = manifest
        .get("rootColors")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    // Conditions: resolve selectedValue â†’ values[targetCss/targetJs] with affects
    let mut condition_css = Vec::new();
    let mut condition_js = Vec::new();
    let mut slider_css = String::new();
    let mut slider_vars: Vec<(String, String)> = Vec::new();

    if let Some(conds) = manifest.get("conditions").and_then(|c| c.as_object()) {
        for (name, cond) in conds {
            // Slider conditions â†’ :root variables
            if let Some(slider) = cond.get("slider") {
                if let (Some(var_name), Some(current)) = (
                    slider.get("cssVariable").and_then(|v| v.as_str()),
                    slider.get("currentValue").and_then(|v| v.as_f64()),
                ) {
                    let unit = slider.get("unit").and_then(|v| v.as_str()).unwrap_or("");
                    slider_vars.push((var_name.to_string(), format!("{}{}", current, unit)));
                }
                continue;
            }

            let selected = match cond.get("selectedValue").and_then(|v| v.as_str()) {
                Some(s) if !s.is_empty() => s,
                _ => cond.get("default").and_then(|v| v.as_str()).unwrap_or(""),
            };
            if selected.is_empty() {
                continue;
            }
            let values = match cond.get("values").and_then(|v| v.as_object()) {
                Some(v) => v,
                None => continue,
            };
            let val_obj = match values.get(selected) {
                Some(v) => v,
                None => continue,
            };

            let mut push_entry = |target: &str, dest: &mut Vec<ThemeConditionEntry>| {
                if let Some(obj) = val_obj.get(target) {
                    let src = obj.get("src").and_then(|v| v.as_str()).unwrap_or("");
                    let affects: Vec<String> = obj
                        .get("affects")
                        .and_then(|v| v.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|a| a.as_str().map(|s| s.to_string()))
                                .collect()
                        })
                        .unwrap_or_default();
                    if !src.is_empty() && !affects.is_empty() {
                        dest.push(ThemeConditionEntry {
                            affects,
                            src: src.to_string(),
                        });
                    }
                }
            };
            let _ = name;
            push_entry("targetCss", &mut condition_css);
            push_entry("targetJs", &mut condition_js);
        }
    }

    if !slider_vars.is_empty() {
        slider_css.push_str(":root {\n");
        for (var, val) in &slider_vars {
            slider_css.push_str(&format!("    {}: {};\n", var, val));
        }
        slider_css.push_str("}\n");
    }

    crate::log_to_temp(&format!(
        "[steamcdp] Loaded theme: {} patches, webkit={}, rootColors={}, cond_css={}, cond_js={}, slider_vars={}, theme_dir={}",
        patches.len(),
        webkit_css.is_some(),
        root_colors.is_some(),
        condition_css.len(),
        condition_js.len(),
        slider_vars.len(),
        theme_dir
    ));

    ThemeBundle {
        dir: theme_dir,
        patches,
        webkit_css,
        webkit_js,
        root_colors,
        condition_css,
        condition_js,
        slider_css,
    }
}

/// Convert a Windows absolute path to a VFS URL.
/// Strips the theme_dir prefix, normalizes to forward slashes, prepends VFS host.
fn path_to_vfs_url(theme_dir: &str, absolute_path: &str) -> String {
    // Try stripping the theme dir prefix (with or without trailing separator)
    let relative = if let Some(rest) = absolute_path.strip_prefix(theme_dir) {
        rest.strip_prefix('\\')
            .or_else(|| rest.strip_prefix('/'))
            .unwrap_or(rest)
    } else {
        absolute_path
    };
    let relative_fwd = relative.replace('\\', "/");
    format!("https://lumaforge.local/themes/{}", relative_fwd)
}

// â”€â”€â”€ Regex matching â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

fn regex_matches(pattern: &str, text: &str) -> bool {
    if pattern == ".*" {
        return true;
    }
    match Regex::new(pattern) {
        Ok(re) => re.is_match(text),
        Err(_) => text.contains(pattern),
    }
}

/// Millennium-compatible window matching (same semantics as cef_hook):
/// regex on the window title, substring on ".<class token>" entries from
/// <html>/<body> classes, plus the `^Steam$` â†’ `^Steam Games List$` alias for
/// patches (Millennium patcher/index.ts EvaluatePatches).
fn window_matches(
    pattern: &str,
    title: &str,
    html_class: &str,
    body_class: &str,
    use_alias: bool,
) -> bool {
    if pattern == ".*" {
        return true;
    }
    if regex_matches(pattern, title) {
        return true;
    }
    for token in html_class
        .split_whitespace()
        .chain(body_class.split_whitespace())
    {
        if format!(".{}", token).contains(pattern) {
            return true;
        }
    }
    if use_alias && pattern == "^Steam$" && regex_matches("^Steam Games List$", title) {
        return true;
    }
    false
}

/// Fallback accent palette (Steam blue) â€” the injector path can't call the
/// Windows uxtheme APIs (cef_hook does that on Windows); Linux gets this.
const ACCENT_FALLBACK: [&str; 7] = [
    "#66c0ff", "#8fd1ff", "#abdfff", "#ccefff", "#4da6e8", "#3b8bc9", "#2a70aa",
];

fn accent_css() -> String {
    let rgb = |hex: &str| -> String {
        let h = hex.trim_start_matches('#');
        let r = u8::from_str_radix(&h[0..2], 16).unwrap_or(0);
        let g = u8::from_str_radix(&h[2..4], 16).unwrap_or(0);
        let b = u8::from_str_radix(&h[4..6], 16).unwrap_or(0);
        format!("{}, {}, {}", r, g, b)
    };
    let mut css = String::from(":root {\n");
    let mut push = |name: &str, hex: &str| {
        if name.is_empty() {
            css.push_str(&format!("    --SystemAccentColor: {};\n", hex));
            css.push_str(&format!("    --SystemAccentColor-RGB: {};\n", rgb(hex)));
        } else {
            css.push_str(&format!("    --SystemAccentColor{}: {};\n", name, hex));
            css.push_str(&format!(
                "    --SystemAccentColor{}-RGB: {};\n",
                name,
                rgb(hex)
            ));
        }
    };
    push("", ACCENT_FALLBACK[0]);
    push("Accent", ACCENT_FALLBACK[0]);
    push("Light1", ACCENT_FALLBACK[1]);
    push("Light2", ACCENT_FALLBACK[2]);
    push("Light3", ACCENT_FALLBACK[3]);
    push("Dark1", ACCENT_FALLBACK[4]);
    push("Dark2", ACCENT_FALLBACK[5]);
    push("Dark3", ACCENT_FALLBACK[6]);
    push("OriginalAccent", ACCENT_FALLBACK[0]);
    css.push_str("}\n");
    css
}

fn js_escape_str(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('\'', "\\'")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

fn eval_js(client: &mut Transport<'_>, msg_id: &mut u64, expression: String) -> Result<(), String> {
    let id = *msg_id;
    let resp = client.send_cdp_wait(
        &json!({
            "id": id,
            "method": "Runtime.evaluate",
            "params": { "expression": expression, "returnByValue": true }
        }),
        id,
    )?;
    if let Some(err) = resp.get("error") {
        crate::log_to_temp(&format!("[steamcdp] theme evaluate error: {}", err));
    }
    *msg_id += 1;
    Ok(())
}

/// Build a JS snippet that adds a <link rel=stylesheet>, deduped via data-lmf.
fn stylesheet_js(vfs_url: &str) -> String {
    format!(
        "(function(){{\
            var u='{}';\
            if(document.querySelector('[data-lmf=\"'+u+'\"]'))return;\
            var l=document.createElement('link');\
            l.rel='stylesheet';l.href=u;l.setAttribute('data-lmf',u);\
            (document.head||document.documentElement).appendChild(l);\
        }})();",
        vfs_url
    )
}

/// Build a JS snippet that adds a <script type=module>, deduped via data-lmf.
fn module_script_js(vfs_url: &str) -> String {
    format!(
        "(function(){{\
            var u='{}';\
            if(document.querySelector('[data-lmf=\"'+u+'\"]'))return;\
            var s=document.createElement('script');\
            s.type='module';s.src=u;s.setAttribute('data-lmf',u);\
            (document.head||document.documentElement).appendChild(s);\
        }})();",
        vfs_url
    )
}

/// Build a JS snippet that adds an inline <style> with the given id, deduped by id.
fn inline_style_js(style_id: &str, css: &str) -> String {
    format!(
        "(function(){{\
            if(document.getElementById('{}'))return;\
            var s=document.createElement('style');\
            s.id='{}';\
            s.setAttribute('data-lmf','{}');\
            s.textContent='{}';\
            (document.head||document.documentElement).appendChild(s);\
        }})();",
        style_id,
        style_id,
        style_id,
        js_escape_str(css)
    )
}

/// Wrap each theme snippet in `try{…}catch(e){}` so one failing snippet
/// can't skip the rest of the bundle (and the resulting string is valid JS).
fn wrap_theme_body(snippets: &[String]) -> String {
    snippets
        .iter()
        .map(|s| format!("try{{{}}}catch(e){{}}", s))
        .collect()
}

fn inject_stylesheet(
    client: &mut Transport<'_>,
    msg_id: &mut u64,
    vfs_url: &str,
) -> Result<(), String> {
    eval_js(client, msg_id, stylesheet_js(vfs_url))
}

fn inject_module_script(
    client: &mut Transport<'_>,
    msg_id: &mut u64,
    vfs_url: &str,
) -> Result<(), String> {
    eval_js(client, msg_id, module_script_js(vfs_url))
}

fn inject_inline_style(
    client: &mut Transport<'_>,
    msg_id: &mut u64,
    style_id: &str,
    css: &str,
) -> Result<(), String> {
    eval_js(client, msg_id, inline_style_js(style_id, css))
}

// â”€â”€â”€ Main injection entry point â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

/// Check if a target URL belongs to a real Steam page (not a context menu, footer, or internal UI).
pub(crate) fn is_real_steam_page(url: &str) -> bool {
    url.contains("store.steampowered.com")
        || url.contains("steamcommunity.com")
        || url.starts_with("steam://")
        || url.contains("help.steampowered.com")
        || url.contains("library.steampowered.com")
}

/// Bridge proxy JS â€” intercepts fetch() to the CDP proxy bridge and routes
/// it through a global queue so the Rust watcher loop can fulfill requests
/// without mixed-content issues (HTTPS page â†’ HTTP bridge).
/// If the cef_hook native bridge (`__luma_bridge_call`) is present, that is
/// preferred â€” it fulfills immediately without waiting for the Rust drain.
pub const BRIDGE_PROXY_JS: &str = r#"
(function(){
  if (window.__lumaBridgeProxyInstalled) return;
  window.__lumaBridgeProxyInstalled = true;
  window.__lumaBridgeQueue = [];
  window.__lumaBridgeResults = {};
  var _origFetch = window.fetch;
  window.fetch = function(url, opts) {
    var urlStr = (typeof url === 'string') ? url : (url && url.url) || '';
    var isBridge = (urlStr.indexOf('127.0.0.1:21775') !== -1 || urlStr.indexOf('localhost:21775') !== -1 ||
                    urlStr.indexOf('127.0.0.1:21776') !== -1 || urlStr.indexOf('localhost:21776') !== -1);
    if (isBridge) {
      // Prefer native bridge (cef_hook) when available â€” no drain latency
      if (typeof window.__luma_bridge_call === 'function') {
        var nativePath = urlStr.replace(/^https?:\/\/[^\/]+/, '');
        if (!nativePath) nativePath = '/';
        return window.__luma_bridge_call(nativePath, opts);
      }
      var id = 'bq' + Date.now() + '_' + Math.random().toString(36).substr(2,6);
      var method = (opts && opts.method) || 'GET';
      var body = (opts && opts.body) || null;
      var headers = {};
      if (opts && opts.headers) {
        if (opts.headers.forEach) {
          opts.headers.forEach(function(v,k){ headers[k]=v; });
        } else {
          for (var k in opts.headers) headers[k] = opts.headers[k];
        }
      }
      window.__lumaBridgeQueue.push({id:id, url:urlStr, method:method, body:body, headers:headers});
      return new Promise(function(resolve, reject) {
        var elapsed = 0;
        var signal = opts && opts.signal;
        if (signal && signal.aborted) {
          var ab0 = new Error('Aborted'); ab0.name = 'AbortError'; reject(ab0); return;
        }
        var onAbort = function() {
          clearInterval(iv);
          if (signal && signal.removeEventListener) signal.removeEventListener('abort', onAbort);
          var ab = new Error('Aborted'); ab.name = 'AbortError'; reject(ab);
        };
        if (signal) {
          if (signal.addEventListener) signal.addEventListener('abort', onAbort);
          else signal.onabort = onAbort;
        }
        var iv = setInterval(function() {
          elapsed += 100;
          if (window.__lumaBridgeResults[id]) {
            clearInterval(iv);
            if (signal && signal.removeEventListener) signal.removeEventListener('abort', onAbort);
            var r = window.__lumaBridgeResults[id];
            delete window.__lumaBridgeResults[id];
            var h = new Headers();
            if (r.headers) { for (var k in r.headers) h.set(k, r.headers[k]); }
            resolve(new Response(r.body || '', {status: r.status || 200, statusText: r.statusText || 'OK', headers: h}));
          } else if (elapsed > 8000) {
            clearInterval(iv);
            if (signal && signal.removeEventListener) signal.removeEventListener('abort', onAbort);
            reject(new Error('Bridge proxy timeout'));
          }
        }, 100);
      });
    }
    return _origFetch.apply(this, arguments);
  };
  window.__lumaBridgeDrain = function() {
    var q = window.__lumaBridgeQueue;
    window.__lumaBridgeQueue = [];
    return JSON.stringify(q);
  };
})();
"#;

pub fn inject_all(client: &mut Transport<'_>) -> Result<(), String> {
    let plugins = load_enabled_plugins().unwrap_or_default();
    let bundle = load_theme_patches();

    // Skip injection entirely when there's nothing to inject â€” avoids unnecessary
    // CDP connections, Page.enable, and Page.setBypassCSP that can break page
    // functionality (e.g., Steam agecheck pages).
    if plugins.is_empty() && bundle.is_empty() {
        crate::log_to_temp("[steamcdp] No plugins or theme data, skipping injection");
        return Ok(());
    }

    let targets = client.get_targets()?;
    let pages: Vec<&Target> = targets
        .iter()
        .filter(|t| t.target_type == "page" && is_real_steam_page(&t.url))
        .collect();

    if pages.is_empty() {
        crate::log_to_temp("[steamcdp] No page targets found");
        return Ok(());
    }

    crate::log_to_temp(&format!(
        "[steamcdp] Found {} page targets ({} total targets), {} plugins, {} theme patches",
        pages.len(),
        targets.len(),
        plugins.len(),
        bundle.patches.len()
    ));

    for (idx, target) in pages.iter().enumerate() {
        inject_into_target(client, target, &plugins, &bundle, idx + 1)?;
    }

    Ok(())
}

// â”€â”€â”€ Per-target injection â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

pub fn inject_into_target(
    client: &mut Transport<'_>,
    target: &Target,
    plugins: &[crate::plugin::LoadedPlugin],
    bundle: &ThemeBundle,
    target_num: usize,
) -> Result<(), String> {
    crate::log_to_temp(&format!(
        "[steamcdp] Target #{}: id={}, title=\"{}\", url={}",
        target_num,
        target.id,
        target.title,
        &target.url[..target.url.len().min(120)]
    ));
    client.attach_to_target(&target.id)?;

    let mut msg_id = 100u64;

    let resp = client.send_cdp_wait(
        &json!({
            "id": msg_id,
            "method": "Page.enable",
            "params": {}
        }),
        msg_id,
    )?;
    if let Some(err) = resp.get("error") {
        crate::log_to_temp(&format!("[steamcdp] Page.enable error: {}", err));
    }
    msg_id += 1;

    let resp = client.send_cdp_wait(
        &json!({
            "id": msg_id,
            "method": "Page.setBypassCSP",
            "params": {"enabled": true}
        }),
        msg_id,
    )?;
    if let Some(err) = resp.get("error") {
        crate::log_to_temp(&format!("[steamcdp] setBypassCSP error: {}", err));
    }
    msg_id += 1;

    // â”€â”€â”€ Theme: window identity (title + <html>/<body> classes) â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€
    let identity_expr = r#"JSON.stringify({t:document.title||'',h:(document.documentElement&&document.documentElement.className)||'',b:(document.body&&document.body.className)||''})"#;
    let (win_title, html_class, body_class) = match client.send_cdp_wait(
        &json!({
            "id": msg_id,
            "method": "Runtime.evaluate",
            "params": { "expression": identity_expr, "returnByValue": true }
        }),
        msg_id,
    ) {
        Ok(resp) => resp
            .get("result")
            .and_then(|r| r.get("result"))
            .and_then(|r| r.get("value"))
            .and_then(|v| v.as_str())
            .and_then(|v| serde_json::from_str::<Value>(v).ok())
            .map(|v| {
                (
                    v.get("t")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string(),
                    v.get("h")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string(),
                    v.get("b")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string(),
                )
            })
            .unwrap_or_else(|| (target.title.clone(), String::new(), String::new())),
        Err(_) => (target.title.clone(), String::new(), String::new()),
    };
    msg_id += 1;

    // SharedJSContext: never inject theme into its head. Steam mirrors that
    // head into popups, so theme links there block the renderWhenReady gate
    // and the popup never becomes visible (black window). Matches Windows
    // cef_hook's isSharedCtx() skip. Bridge + plugins below still run.
    let is_shared_ctx = win_title == "SharedJSContext" || win_title == "SP Shared JS Context";
    if is_shared_ctx {
        crate::log_to_temp(&format!(
            "[steamcdp] Theme skipped (SharedJSContext) target#{}",
            target_num
        ));
    }

    // Popup documents (about:blank?createflags=N, steamloopback UI windows)
    // are created blank and then document.write()'d by Steam. Their
    // renderWhenReady gate tracks <link>/<style> elements present right after
    // the write — injecting earlier races that gate and the popup never shows
    // (black). Mirror Windows: defer theme application until #popup_target
    // exists (= write finished, Q ctor ran) and the document is fully parsed.
    let is_popup_doc = target.url.starts_with("about:")
        || target.url.starts_with("data:")
        || target.url.is_empty()
        || target.url.contains("createflags")
        || target.url.contains("steamloopback.host");

    // Source registered via Page.addScriptToEvaluateOnNewDocument so the
    // theme re-applies on every future document in this target (a refresh
    // builds a new document; Runtime.evaluate snippets only live in the
    // current one). Mirrors cef_hook's register_theme_injection_script.
    let mut theme_reg_source: Option<String> = None;

    if !bundle.is_empty() && !is_shared_ctx {
        // Collect theme snippets so popups can defer them as one bootstrap
        // (a per-snippet wait would race between injections) and store pages
        // apply them immediately as before.
        let mut theme_snippets: Vec<String> = Vec::new();

        // --luma-theme-active:1 marker: the extension's themeColor.ts gates
        // accent/panel adoption on it (parity with cef_hook's setThemeActive).
        // Without it the plugin keeps its own defaults instead of the theme's.
        theme_snippets.push(inline_style_js(
            "LmfThemeActive",
            ":root{--luma-theme-active:1}",
        ));
        theme_snippets.push(inline_style_js("SystemAccentColorInject", &accent_css()));
        if let Some(ref rc) = bundle.root_colors {
            match fs::read_to_string(rc) {
                Ok(css) => theme_snippets.push(inline_style_js("RootColors", &css)),
                Err(e) => {
                    crate::log_to_temp(&format!("[steamcdp] rootColors read error ({}): {}", rc, e))
                }
            }
        }
        if let Some(ref css) = bundle.webkit_css {
            theme_snippets.push(stylesheet_js(&path_to_vfs_url(&bundle.dir, css)));
        }
        if let Some(ref js) = bundle.webkit_js {
            theme_snippets.push(module_script_js(&path_to_vfs_url(&bundle.dir, js)));
        }
        if !bundle.slider_css.is_empty() {
            theme_snippets.push(inline_style_js(
                "MillenniumSliderConditions",
                &bundle.slider_css,
            ));
        }

        // Theme patches: collect <link>/<script type="module"> VFS URLs
        for patch in &bundle.patches {
            let matches = window_matches(
                &patch.match_regex,
                &win_title,
                &html_class,
                &body_class,
                true,
            ) || regex_matches(&patch.match_regex, &target.url);

            if !matches {
                continue;
            }

            if let Some(ref css_path) = patch.target_css {
                let vfs_url = path_to_vfs_url(&bundle.dir, css_path);
                theme_snippets.push(stylesheet_js(&vfs_url));
                crate::log_to_temp(&format!(
                    "[steamcdp] Patch '{}' CSS link collected target#{}: {}",
                    patch.match_regex, target_num, vfs_url
                ));
            }

            if let Some(ref js_path) = patch.target_js {
                let vfs_url = path_to_vfs_url(&bundle.dir, js_path);
                theme_snippets.push(module_script_js(&vfs_url));
                crate::log_to_temp(&format!(
                    "[steamcdp] Patch '{}' JS module collected target#{}: {}",
                    patch.match_regex, target_num, vfs_url
                ));
            }
        }

        // Conditions: selected value → targetCss/targetJs with affects
        for cond in &bundle.condition_css {
            let matched = cond
                .affects
                .iter()
                .any(|a| window_matches(a, &win_title, &html_class, &body_class, false));
            if matched {
                let vfs_url = path_to_vfs_url(&bundle.dir, &cond.src);
                theme_snippets.push(stylesheet_js(&vfs_url));
                crate::log_to_temp(&format!(
                    "[steamcdp] Condition CSS collected target#{}: {}",
                    target_num, vfs_url
                ));
            }
        }
        for cond in &bundle.condition_js {
            let matched = cond
                .affects
                .iter()
                .any(|a| window_matches(a, &win_title, &html_class, &body_class, false));
            if matched {
                let vfs_url = path_to_vfs_url(&bundle.dir, &cond.src);
                theme_snippets.push(module_script_js(&vfs_url));
            }
        }

        if !theme_snippets.is_empty() {
            // Each snippet isolated so one failure can't skip the rest.
            let body = wrap_theme_body(&theme_snippets);

            if is_popup_doc {
                // Defer: one bootstrap that waits for the popup's
                // renderWhenReady gate, then applies every snippet at once.
                let defer_js = format!(
                    "(function(){{\
                        var apply=function(){{{}}};\
                        var isPopup=(location.protocol==='about:'\
                            ||location.href.indexOf('createflags')!==-1\
                            ||location.href.indexOf('steamloopback.host')!==-1\
                            ||location.href.indexOf('data:')===0\
                            ||!document.getElementById('popup_target'));\
                        if(!isPopup){{apply();return;}}\
                        var started=false;\
                        var go=function(){{if(started)return;started=true;apply();}};\
                        var consider=function(){{\
                            if(document.getElementById('popup_target')&&document.readyState==='complete')go();\
                        }};\
                        consider();\
                        if(!started){{\
                            window.addEventListener('load',consider);\
                            try{{new MutationObserver(consider).observe(document,{{childList:true,subtree:true}});}}catch(e){{}}\
                            var iv=setInterval(function(){{consider();if(started)clearInterval(iv);}},150);\
                            setTimeout(go,5000);\
                        }}\
                    }})();",
                    body
                );
                // Same bootstrap registered for future documents in this
                // target (cleanup below drops the previous registration).
                theme_reg_source = Some(defer_js.clone());
                client.send_cdp_wait(
                    &json!({
                        "id": msg_id,
                        "method": "Runtime.evaluate",
                        "params": { "expression": defer_js, "returnByValue": true }
                    }),
                    msg_id,
                )?;
                msg_id += 1;
                crate::log_to_temp(&format!(
                    "[steamcdp] Theme deferred bootstrap ({} snippet(s)) target#{}",
                    theme_snippets.len(),
                    target_num
                ));
            } else {
                // Registered source: wait for a root element (the script can
                // run before parsing starts), then apply. Snippet dedupe
                // makes a registration also running on the current document
                // harmless alongside the immediate evaluation below.
                theme_reg_source = Some(format!(
                    "(function(){{\
                        var apply=function(){{{}}};\
                        var tries=0;\
                        (function poll(){{\
                            if(document.head||document.documentElement||tries++>250){{try{{apply();}}catch(e){{}}}}\
                            else setTimeout(poll,20);\
                        }})();\
                    }})();",
                    body
                ));
                for snippet in &theme_snippets {
                    eval_js(client, &mut msg_id, snippet.clone())?;
                }
            }
        }
    }

    // â”€â”€â”€ Bridge proxy: intercept fetch() for HTTPSâ†’HTTP mixed content â”€â”€â”€
    // Register for future navigations
    // Drop the registrations made by the previous inject cycle first:
    // Page.addScriptToEvaluateOnNewDocument accumulates one entry per call,
    // so re-adding every cycle made each new document evaluate the script
    // once per stacked entry.
    {
        let prev = registered_scripts()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&target.id);
        if let Some(prev) = prev {
            let mut stale: Vec<String> =
                prev.plugins.into_iter().filter_map(|(_, id)| id).collect();
            if let Some(id) = prev.bridge {
                stale.push(id);
            }
            if let Some(id) = prev.theme {
                stale.push(id);
            }
            for ident in stale {
                let removed = client.send_cdp_wait(
                    &json!({
                        "id": msg_id,
                        "method": "Page.removeScriptToEvaluateOnNewDocument",
                        "params": {"identifier": ident}
                    }),
                    msg_id,
                );
                msg_id += 1;
                match removed {
                    Ok(resp) => {
                        if let Some(err) = resp.get("error") {
                            crate::log_to_temp(&format!(
                                "[steamcdp] removeScript '{}' error: {}",
                                ident, err
                            ));
                        }
                    }
                    Err(e) => crate::log_to_temp(&format!(
                        "[steamcdp] removeScript '{}' failed: {}",
                        ident, e
                    )),
                }
            }
        }
    }
    let mut plugin_idents: Vec<(String, Option<String>)> = Vec::new();

    // Theme: register the snippets for every future document in this target
    // so a refresh/navigation re-applies them (the immediate evaluation above
    // only covers the current document). Registered before bridge/plugins so
    // --luma-theme-active is in place when the extension resolves colors.
    let mut theme_ident: Option<String> = None;
    if let Some(src) = &theme_reg_source {
        let resp = client.send_cdp_wait(
            &json!({
                "id": msg_id,
                "method": "Page.addScriptToEvaluateOnNewDocument",
                "params": {
                    "source": src,
                    "runImmediately": true
                }
            }),
            msg_id,
        )?;
        if let Some(err) = resp.get("error") {
            crate::log_to_temp(&format!("[steamcdp] Theme addScript error: {}", err));
        } else {
            theme_ident = resp
                .get("result")
                .and_then(|r| r.get("identifier"))
                .and_then(|i| i.as_str())
                .map(String::from);
            crate::log_to_temp(&format!(
                "[steamcdp] Theme registered for new documents (id={}) target#{}",
                theme_ident.as_deref().unwrap_or("none"),
                target_num
            ));
        }
        msg_id += 1;
    }

    let resp = client.send_cdp_wait(
        &json!({
            "id": msg_id,
            "method": "Page.addScriptToEvaluateOnNewDocument",
            "params": {
                "source": BRIDGE_PROXY_JS,
                "runImmediately": true
            }
        }),
        msg_id,
    )?;
    if let Some(err) = resp.get("error") {
        crate::log_to_temp(&format!("[steamcdp] Bridge proxy addScript error: {}", err));
    } else {
        crate::log_to_temp("[steamcdp] Bridge proxy registered for new documents");
    }
    msg_id += 1;
    let bridge_ident: Option<String> = resp
        .get("result")
        .and_then(|r| r.get("identifier"))
        .and_then(|i| i.as_str())
        .map(String::from);

    // Also execute immediately on current page
    let resp = client.send_cdp_wait(
        &json!({
            "id": msg_id,
            "method": "Runtime.evaluate",
            "params": {
                "expression": BRIDGE_PROXY_JS,
                "returnByValue": true
            }
        }),
        msg_id,
    )?;
    if let Some(err) = resp.get("error") {
        crate::log_to_temp(&format!("[steamcdp] Bridge proxy eval error: {}", err));
    } else {
        crate::log_to_temp("[steamcdp] Bridge proxy installed on current page");
    }
    msg_id += 1;

    // â”€â”€â”€ Plugins â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€
    for plugin in plugins {
        // activation.targetUrl gates the plugin on store pages;
        // activation.runtimeTargetUrl (e.g. steamloopback.host) is an
        // additional allowed pattern. Client-UI popup documents (Library,
        // friends, settings) are opened with about:blank?createflags=N, an
        // empty URL, or steamloopback.host — their real URL is not known to
        // Rust when the target is first seen, so deliver the script and let
        // the JS-side location.href check (same model as cef_hook) decide.
        // On Windows this third clause is unreachable: every CDP target there
        // is a real Steam store/community page.
        let t_ok = match plugin.target_url.as_deref() {
            Some(p) if !p.is_empty() => target.url.contains(p),
            _ => true,
        };
        let r_ok = match plugin.runtime_target_url.as_deref() {
            Some(p) if !p.is_empty() => target.url.contains(p),
            _ => false,
        };
        let popup_ok = target.url.is_empty()
            || target.url.starts_with("about:")
            || target.url.starts_with("data:")
            || target.url.contains("steamloopback.host");
        if !(t_ok || r_ok || popup_ok) {
            crate::log_to_temp(&format!(
                "[steamcdp] Plugin '{}' skipped target#{} (targetUrl '{}' not in {})",
                plugin.name,
                target_num,
                plugin.target_url.as_deref().unwrap_or(""),
                &target.url[..target.url.len().min(80)]
            ));
            continue;
        }
        let resp = client.send_cdp_wait(
            &json!({
                "id": msg_id,
                "method": "Page.addScriptToEvaluateOnNewDocument",
                "params": {
                    "source": &plugin.code,
                    "runImmediately": true
                }
            }),
            msg_id,
        )?;

        let script_id_opt: Option<String> = resp
            .get("result")
            .and_then(|r| r.get("identifier"))
            .and_then(|i| i.as_str())
            .map(String::from);
        plugin_idents.push((plugin.name.clone(), script_id_opt.clone()));
        let script_id = script_id_opt.as_deref().unwrap_or("none");

        if let Some(err) = resp.get("error") {
            crate::log_to_temp(&format!(
                "[steamcdp] Plugin addScript error '{}' target#{}: {}",
                plugin.name, target_num, err
            ));
        } else {
            crate::log_to_temp(&format!(
                "[steamcdp] Plugin '{}' registered target#{} (id={})",
                plugin.name, target_num, script_id
            ));
        }
        msg_id += 1;

        let resp = client.send_cdp_wait(
            &json!({
                "id": msg_id,
                "method": "Runtime.evaluate",
                "params": {
                    "expression": &plugin.code,
                    "awaitPromise": true,
                    "returnByValue": true
                }
            }),
            msg_id,
        )?;

        if let Some(err) = resp.get("error") {
            crate::log_to_temp(&format!(
                "[steamcdp] Plugin eval error '{}' target#{}: {}",
                plugin.name, target_num, err
            ));
        } else if let Some(result) = resp.get("result") {
            if let Some(exc) = result.get("exceptionDetails") {
                let text = exc
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                let desc = exc
                    .get("exception")
                    .and_then(|e| e.get("description"))
                    .and_then(|d| d.as_str())
                    .unwrap_or("");
                crate::log_to_temp(&format!(
                    "[steamcdp] Plugin '{}' exception in target#{}: {} {}",
                    plugin.name, target_num, text, desc
                ));
            } else {
                crate::log_to_temp(&format!(
                    "[steamcdp] Plugin '{}' executed in target#{}",
                    plugin.name, target_num
                ));
            }
        }
        msg_id += 1;
    }

    // Diagnostic: test if Runtime.evaluate actually works and check inject state
    let test_resp = client.send_cdp_wait(
        &json!({
            "id": msg_id,
            "method": "Runtime.evaluate",
            "params": {
                "expression": r#"(function(){
                    var ns = window.__lumaforge_ssh__;
                    var APP_URL_RE = /\/app\/(\d+)(?:\/|$)/;
                    var locMatch = (window.location.href||'').match(APP_URL_RE);
                    var appLinks = document.querySelectorAll('a[href*="/app/"]');
                    var appLinkHrefs = [];
                    for(var i=0;i<appLinks.length;i++) appLinkHrefs.push(appLinks[i].getAttribute('href'));
                    var subInput = document.querySelector('input[name="subid"]');
                    var dataAppid = document.querySelectorAll('[data-appid]');
                    var btnExists = !!document.getElementById('luma-action-btn');
                    var actionBar = document.querySelector('#game_area_purchase_game') || document.querySelector('.game_area_purchase_game') || document.querySelector('.apphub_OtherSiteInfo') || document.querySelector('.app_title_area');
                    return JSON.stringify({
                        url: window.location.href.substring(0,150),
                        bodyLen: document.body ? document.body.innerHTML.length : -1,
                        hasLuma: !!ns, lumaActive: ns && ns.active, lumaAppId: ns && ns.currentAppId,
                        locMatch: locMatch ? locMatch[1] : null,
                        appLinkCount: appLinks.length, appLinkHrefs: appLinkHrefs.slice(0,5),
                        hasSubInput: !!subInput, subInputVal: subInput ? subInput.value : null,
                        dataAppidCount: dataAppid.length,
                        btnExists: btnExists,
                        actionBarFound: !!actionBar, actionBarTag: actionBar ? actionBar.tagName : null,
                        title: document.title.substring(0,80)
                    });
                })()"#,
                "returnByValue": true
            }
        }),
        msg_id,
    )?;
    let result_str = test_resp
        .get("result")
        .and_then(|r| r.get("result"))
        .and_then(|r| r.get("value"))
        .and_then(|v| v.as_str())
        .unwrap_or("<no value>");
    crate::log_to_temp(&format!(
        "[steamcdp] Diag target#{}: {}",
        target_num, result_str
    ));
    msg_id += 1;

    registered_scripts()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(
            target.id.clone(),
            RegisteredScripts {
                bridge: bridge_ident,
                plugins: plugin_idents,
                theme: theme_ident,
            },
        );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_matches_millennium_semantics() {
        assert!(window_matches("^Steam$", "Steam", "", "", true));
        assert!(window_matches("^Steam$", "Steam Games List", "", "", true));
        assert!(!window_matches(
            "^Steam$",
            "Steam Games List",
            "",
            "",
            false
        ));
        assert!(window_matches(
            ".friendsui-container",
            "Friends",
            "",
            "friendsui-container",
            false
        ));
        assert!(window_matches(".*", "", "", "", false));
        assert!(!window_matches("^Account", "Steam", "", "", false));
        assert!(window_matches("^Account", "Account Menu", "", "", false));
    }

    #[test]
    fn accent_css_contains_core_vars() {
        let css = accent_css();
        assert!(css.contains("--SystemAccentColorAccent: #66c0ff"));
        assert!(css.contains("--SystemAccentColorLight1: #8fd1ff"));
        assert!(css.contains("--SystemAccentColor-RGB: 102, 192, 255"));
    }

    #[test]
    fn wrap_theme_body_wraps_each_snippet_in_try_catch() {
        let snippets = vec![
            "(function(){var a=1;})();".to_string(),
            "(function(){var b=2;})();".to_string(),
        ];
        let body = wrap_theme_body(&snippets);
        assert_eq!(
            body,
            "try{(function(){var a=1;})();}catch(e){}try{(function(){var b=2;})();}catch(e){}"
        );
        // The wrapped body must be parseable: every `try{` has a matching catch.
        assert_eq!(body.matches("try{").count(), body.matches("catch(e){").count());
        assert_eq!(body.matches("try{").count(), snippets.len());
    }

    #[test]
    fn wrap_theme_body_empty_is_empty() {
        assert_eq!(wrap_theme_body(&[]), "");
    }
}
