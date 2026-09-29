use crate::cdp::{CdpClient, Target};
use regex::Regex;
use serde_json::{json, Value};
use std::fs;

#[cfg(target_os = "windows")]
fn load_enabled_plugins() -> Result<Vec<crate::plugin::LoadedPlugin>, String> {
    crate::plugin_loader::load_enabled_plugins()
}

#[cfg(target_os = "linux")]
fn load_enabled_plugins() -> Result<Vec<crate::plugin::LoadedPlugin>, String> {
    crate::plugin_loader_linux::load_enabled_plugins()
}

// ─── Theme data (parsed from theme-manifest.json) ───────────────────────────

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
    let manifest_path = crate::platform::runtime_dir()
        .join("theme-manifest.json");

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

    // Conditions: resolve selectedValue → values[targetCss/targetJs] with affects
    let mut condition_css = Vec::new();
    let mut condition_js = Vec::new();
    let mut slider_css = String::new();
    let mut slider_vars: Vec<(String, String)> = Vec::new();

    if let Some(conds) = manifest.get("conditions").and_then(|c| c.as_object()) {
        for (name, cond) in conds {
            // Slider conditions → :root variables
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

// ─── Regex matching ─────────────────────────────────────────────────────────

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
/// <html>/<body> classes, plus the `^Steam$` → `^Steam Games List$` alias for
/// patches (Millennium patcher/index.ts EvaluatePatches).
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

/// Fallback accent palette (Steam blue) — the injector path can't call the
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
            css.push_str(&format!("    --SystemAccentColor{}-RGB: {};\n", name, rgb(hex)));
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

fn eval_js(client: &mut CdpClient, msg_id: &mut u64, expression: String) -> Result<(), String> {
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

/// Add a <link rel=stylesheet> to the page, deduped via a data-lmf attribute.
fn inject_stylesheet(client: &mut CdpClient, msg_id: &mut u64, vfs_url: &str) -> Result<(), String> {
    let script = format!(
        "(function(){{\
            var u='{}';\
            if(document.querySelector('[data-lmf=\"'+u+'\"]'))return;\
            var l=document.createElement('link');\
            l.rel='stylesheet';l.href=u;l.setAttribute('data-lmf',u);\
            (document.head||document.documentElement).appendChild(l);\
        }})();",
        vfs_url
    );
    eval_js(client, msg_id, script)
}

/// Add a <script type=module> to the page, deduped via a data-lmf attribute.
fn inject_module_script(client: &mut CdpClient, msg_id: &mut u64, vfs_url: &str) -> Result<(), String> {
    let script = format!(
        "(function(){{\
            var u='{}';\
            if(document.querySelector('[data-lmf=\"'+u+'\"]'))return;\
            var s=document.createElement('script');\
            s.type='module';s.src=u;s.setAttribute('data-lmf',u);\
            (document.head||document.documentElement).appendChild(s);\
        }})();",
        vfs_url
    );
    eval_js(client, msg_id, script)
}

/// Add an inline <style> with the given id, deduped by id.
fn inject_inline_style(
    client: &mut CdpClient,
    msg_id: &mut u64,
    style_id: &str,
    css: &str,
) -> Result<(), String> {
    let script = format!(
        "(function(){{\
            if(document.getElementById('{}'))return;\
            var s=document.createElement('style');\
            s.id='{}';\
            s.textContent='{}';\
            (document.head||document.documentElement).appendChild(s);\
        }})();",
        style_id,
        style_id,
        js_escape_str(css)
    );
    eval_js(client, msg_id, script)
}

// ─── Main injection entry point ─────────────────────────────────────────────

/// Check if a target URL belongs to a real Steam page (not a context menu, footer, or internal UI).
pub(crate) fn is_real_steam_page(url: &str) -> bool {
    url.contains("store.steampowered.com")
        || url.contains("steamcommunity.com")
        || url.starts_with("steam://")
        || url.contains("help.steampowered.com")
        || url.contains("library.steampowered.com")
}

/// Bridge proxy JS — intercepts fetch() to the CDP proxy bridge and routes
/// it through a global queue so the Rust watcher loop can fulfill requests
/// without mixed-content issues (HTTPS page → HTTP bridge).
/// If the cef_hook native bridge (`__luma_bridge_call`) is present, that is
/// preferred — it fulfills immediately without waiting for the Rust drain.
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
      // Prefer native bridge (cef_hook) when available — no drain latency
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

pub fn inject_all(client: &mut CdpClient) -> Result<(), String> {
    let plugins = load_enabled_plugins().unwrap_or_default();
    let bundle = load_theme_patches();

    // Skip injection entirely when there's nothing to inject — avoids unnecessary
    // CDP connections, Page.enable, and Page.setBypassCSP that can break page
    // functionality (e.g., Steam agecheck pages).
    if plugins.is_empty() && bundle.is_empty() {
        crate::log_to_temp("[steamcdp] No plugins or theme data, skipping injection");
        return Ok(());
    }

    let targets = client.get_targets()?;
    let pages: Vec<&Target> = targets.iter()
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

// ─── Per-target injection ───────────────────────────────────────────────────

pub fn inject_into_target(
    client: &mut CdpClient,
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

    // ─── Theme: window identity (title + <html>/<body> classes) ────────────
    let identity_expr = r#"JSON.stringify({t:document.title||'',h:(document.documentElement&&document.documentElement.className)||'',b:(document.body&&document.body.className)||''})"#;
    let (win_title, html_class, body_class) = match client.send_cdp_wait(
        &json!({
            "id": msg_id,
            "method": "Runtime.evaluate",
            "params": { "expression": identity_expr, "returnByValue": true }
        }),
        msg_id,
    ) {
        Ok(resp) => {
            resp.get("result")
                .and_then(|r| r.get("result"))
                .and_then(|r| r.get("value"))
                .and_then(|v| v.as_str())
                .and_then(|v| serde_json::from_str::<Value>(v).ok())
                .map(|v| {
                    (
                        v.get("t").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                        v.get("h").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                        v.get("b").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                    )
                })
                .unwrap_or_else(|| (target.title.clone(), String::new(), String::new()))
        }
        Err(_) => (target.title.clone(), String::new(), String::new()),
    };
    msg_id += 1;

    // ─── Theme: accent colors, root colors, webkit, slider vars ────────────
    if !bundle.is_empty() {
        inject_inline_style(client, &mut msg_id, "SystemAccentColorInject", &accent_css())?;
        if let Some(ref rc) = bundle.root_colors {
            match fs::read_to_string(rc) {
                Ok(css) => inject_inline_style(client, &mut msg_id, "RootColors", &css)?,
                Err(e) => crate::log_to_temp(&format!(
                    "[steamcdp] rootColors read error ({}): {}",
                    rc, e
                )),
            }
        }
        if let Some(ref css) = bundle.webkit_css {
            inject_stylesheet(client, &mut msg_id, css)?;
        }
        if !bundle.slider_css.is_empty() {
            inject_inline_style(client, &mut msg_id, "MillenniumSliderConditions", &bundle.slider_css)?;
        }
    }

    // ─── Theme patches: inject <link>/<script type="module"> via VFS URLs ─
    for patch in &bundle.patches {
        let matches = window_matches(&patch.match_regex, &win_title, &html_class, &body_class, true)
            || regex_matches(&patch.match_regex, &target.url);

        if !matches {
            continue;
        }

        // CSS: inject <link rel="stylesheet" href="VFS_URL"> (deduped)
        if let Some(ref css_path) = patch.target_css {
            let vfs_url = path_to_vfs_url(&bundle.dir, css_path);
            inject_stylesheet(client, &mut msg_id, &vfs_url)?;
            crate::log_to_temp(&format!(
                "[steamcdp] Patch '{}' CSS link injected target#{}: {}",
                patch.match_regex, target_num, vfs_url
            ));
        }

        // JS: inject <script type="module" src="VFS_URL"> (deduped)
        if let Some(ref js_path) = patch.target_js {
            let vfs_url = path_to_vfs_url(&bundle.dir, js_path);
            inject_module_script(client, &mut msg_id, &vfs_url)?;
            crate::log_to_temp(&format!(
                "[steamcdp] Patch '{}' JS module injected target#{}: {}",
                patch.match_regex, target_num, vfs_url
            ));
        }
    }

    // ─── Conditions: selected value → targetCss/targetJs with affects ──────
    for cond in &bundle.condition_css {
        let matched = cond.affects.iter().any(|a| window_matches(a, &win_title, &html_class, &body_class, false));
        if matched {
            let vfs_url = path_to_vfs_url(&bundle.dir, &cond.src);
            inject_stylesheet(client, &mut msg_id, &vfs_url)?;
            crate::log_to_temp(&format!(
                "[steamcdp] Condition CSS injected target#{}: {}",
                target_num, vfs_url
            ));
        }
    }
    if !bundle.condition_js.is_empty() {
        for cond in &bundle.condition_js {
            let matched = cond.affects.iter().any(|a| window_matches(a, &win_title, &html_class, &body_class, false));
            if matched {
                let vfs_url = path_to_vfs_url(&bundle.dir, &cond.src);
                inject_module_script(client, &mut msg_id, &vfs_url)?;
            }
        }
    }

    // ─── Bridge proxy: intercept fetch() for HTTPS→HTTP mixed content ───
    // Register for future navigations
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

    // ─── Plugins ──────────────────────────────────────────────────────
    for plugin in plugins {
        // Respect activation.targetUrl — only inject into matching pages
        if let Some(ref pattern) = plugin.target_url {
            if !pattern.is_empty() && !target.url.contains(pattern.as_str()) {
                crate::log_to_temp(&format!(
                    "[steamcdp] Plugin '{}' skipped target#{} (targetUrl '{}' not in {})",
                    plugin.name,
                    target_num,
                    pattern,
                    &target.url[..target.url.len().min(80)]
                ));
                continue;
            }
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

        let script_id = resp
            .get("result")
            .and_then(|r| r.get("identifier"))
            .and_then(|i| i.as_str())
            .unwrap_or("none");

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
                let text = exc.get("text").and_then(|v| v.as_str()).unwrap_or("unknown");
                let desc = exc.get("exception")
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
    let result_str = test_resp.get("result")
        .and_then(|r| r.get("result"))
        .and_then(|r| r.get("value"))
        .and_then(|v| v.as_str())
        .unwrap_or("<no value>");
    crate::log_to_temp(&format!(
        "[steamcdp] Diag target#{}: {}",
        target_num, result_str
    ));
    msg_id += 1;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_matches_millennium_semantics() {
        assert!(window_matches("^Steam$", "Steam", "", "", true));
        assert!(window_matches("^Steam$", "Steam Games List", "", "", true));
        assert!(!window_matches("^Steam$", "Steam Games List", "", "", false));
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
}
