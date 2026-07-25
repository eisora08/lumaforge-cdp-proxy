use crate::cdp::{CdpClient, Target};
use crate::plugin_loader::load_enabled_plugins;
use regex::Regex;
use serde_json::{json, Value};
use std::fs;

// ─── Theme patch (parsed from theme-manifest.json) ──────────────────────────

pub struct ThemePatchEntry {
    pub match_regex: String,
    pub target_css: Option<String>,
    pub target_js: Option<String>,
}

/// Load theme patches from theme-manifest.json written by main crate
pub fn load_theme_patches() -> (String, Vec<ThemePatchEntry>) {
    let Ok(local_appdata) = std::env::var("LOCALAPPDATA") else {
        return (String::new(), Vec::new());
    };
    let manifest_path = std::path::PathBuf::from(local_appdata)
        .join("LumaForge")
        .join("runtime")
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
            crate::log_to_temp(&format!("[steamcdp] Failed to parse theme-manifest.json: {}", e));
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
        rest.strip_prefix('\\').or_else(|| rest.strip_prefix('/')).unwrap_or(rest)
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
    let pages: Vec<&Target> = targets
        .iter()
        .filter(|t| t.target_type == "page")
        .collect();

    if pages.is_empty() {
        crate::log_to_temp("[steamcdp] No page targets found");
        return Ok(());
    }

    crate::log_to_temp(&format!(
        "[steamcdp] Found {} page targets, {} plugins, {} theme patches",
        pages.len(),
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

        let matches_url = match plugin.target_url {
            Some(ref filter) => target.url.to_lowercase().contains(&filter.to_lowercase()),
            None => true,
        };

        if matches_url {
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
            } else {
                crate::log_to_temp(&format!(
                    "[steamcdp] Plugin '{}' executed in target#{}",
                    plugin.name, target_num
                ));
            }
            msg_id += 1;
        }

        crate::log_to_temp(&format!(
            "[steamcdp] Plugin '{}' -> target#{} url_match={}",
            plugin.name, target_num, matches_url
        ));
    }

    Ok(())
}
