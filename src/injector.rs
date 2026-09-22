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

// ─── Theme patch (parsed from theme-manifest.json) ──────────────────────────

pub struct ThemePatchEntry {
    pub match_regex: String,
    pub target_css: Option<String>,
    pub target_js: Option<String>,
}

/// Load theme patches from theme-manifest.json written by main crate
pub fn load_theme_patches() -> (String, Vec<ThemePatchEntry>) {
    let manifest_path = crate::platform::runtime_dir()
        .join("theme-manifest.json");

    let content = match fs::read_to_string(&manifest_path) {
        Ok(c) => c,
        Err(_) => {
            crate::log_to_temp("[steamcdp] No theme-manifest.json found");
            return (String::new(), Vec::new());
        }
    };

    let manifest: Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(e) => {
            crate::log_to_temp(&format!(
                "[steamcdp] Failed to parse theme-manifest.json: {}",
                e
            ));
            return (String::new(), Vec::new());
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

    crate::log_to_temp(&format!(
        "[steamcdp] Loaded {} theme patches, theme_dir={}",
        patches.len(),
        theme_dir
    ));
    (theme_dir, patches)
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
pub const BRIDGE_PROXY_JS: &str = r#"
(function(){
  if (window.__lumaBridgeProxyInstalled) return;
  window.__lumaBridgeProxyInstalled = true;
  window.__lumaBridgeQueue = [];
  window.__lumaBridgeResults = {};
  var _origFetch = window.fetch;
  window.fetch = function(url, opts) {
    var urlStr = (typeof url === 'string') ? url : (url && url.url) || '';
    if (urlStr.indexOf('127.0.0.1:21775') !== -1 || urlStr.indexOf('localhost:21775') !== -1) {
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
        var iv = setInterval(function() {
          elapsed += 100;
          if (window.__lumaBridgeResults[id]) {
            clearInterval(iv);
            var r = window.__lumaBridgeResults[id];
            delete window.__lumaBridgeResults[id];
            var h = new Headers();
            if (r.headers) { for (var k in r.headers) h.set(k, r.headers[k]); }
            resolve(new Response(r.body || '', {status: r.status || 200, statusText: r.statusText || 'OK', headers: h}));
          } else if (elapsed > 15000) {
            clearInterval(iv);
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
    let (theme_dir, patches) = load_theme_patches();

    // Skip injection entirely when there's nothing to inject — avoids unnecessary
    // CDP connections, Page.enable, and Page.setBypassCSP that can break page
    // functionality (e.g., Steam agecheck pages).
    if plugins.is_empty() && patches.is_empty() {
        crate::log_to_temp("[steamcdp] No plugins or theme patches, skipping injection");
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
        patches.len()
    ));

    for (idx, target) in pages.iter().enumerate() {
        inject_into_target(client, target, &plugins, &theme_dir, &patches, idx + 1)?;
    }

    Ok(())
}

// ─── Per-target injection ───────────────────────────────────────────────────

pub fn inject_into_target(
    client: &mut CdpClient,
    target: &Target,
    plugins: &[crate::plugin::LoadedPlugin],
    theme_dir: &str,
    theme_patches: &[ThemePatchEntry],
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

    // ─── Theme patches: inject <link>/<script type="module"> via VFS URLs ─
    for patch in theme_patches {
        let matches_title = regex_matches(&patch.match_regex, &target.title);
        let matches_url = regex_matches(&patch.match_regex, &target.url);

        if !matches_title && !matches_url {
            continue;
        }

        let matched_by = if matches_title { "title" } else { "url" };

        // CSS: inject <link rel="stylesheet" href="VFS_URL">
        if let Some(ref css_path) = patch.target_css {
            let vfs_url = path_to_vfs_url(theme_dir, css_path);
            let script = format!(
                "(function(){{\
                    var l=document.createElement('link');\
                    l.rel='stylesheet';\
                    l.href='{}';\
                    (document.head||document.documentElement).appendChild(l);\
                }})();",
                vfs_url
            );

            let resp = client.send_cdp_wait(
                &json!({
                    "id": msg_id,
                    "method": "Runtime.evaluate",
                    "params": {
                        "expression": &script,
                        "returnByValue": true
                    }
                }),
                msg_id,
            )?;

            if let Some(err) = resp.get("error") {
                crate::log_to_temp(&format!(
                    "[steamcdp] Theme CSS error patch='{}' target#{}: {}",
                    patch.match_regex, target_num, err
                ));
            } else {
                crate::log_to_temp(&format!(
                    "[steamcdp] Patch '{}' CSS link injected (matched by {}) target#{}: {}",
                    patch.match_regex, matched_by, target_num, vfs_url
                ));
            }
            msg_id += 1;
        }

        // JS: inject <script type="module" src="VFS_URL">
        if let Some(ref js_path) = patch.target_js {
            let vfs_url = path_to_vfs_url(theme_dir, js_path);
            let script = format!(
                "(function(){{\
                    var s=document.createElement('script');\
                    s.type='module';\
                    s.src='{}';\
                    (document.head||document.documentElement).appendChild(s);\
                }})();",
                vfs_url
            );

            let resp = client.send_cdp_wait(
                &json!({
                    "id": msg_id,
                    "method": "Runtime.evaluate",
                    "params": {
                        "expression": &script,
                        "returnByValue": true
                    }
                }),
                msg_id,
            )?;

            if let Some(err) = resp.get("error") {
                crate::log_to_temp(&format!(
                    "[steamcdp] Theme JS error patch='{}' target#{}: {}",
                    patch.match_regex, target_num, err
                ));
            } else {
                crate::log_to_temp(&format!(
                    "[steamcdp] Patch '{}' JS module injected (matched by {}) target#{}: {}",
                    patch.match_regex, matched_by, target_num, vfs_url
                ));
            }
            msg_id += 1;
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
