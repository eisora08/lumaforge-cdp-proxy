use std::sync::OnceLock;

static DEBUG_PORT: OnceLock<u16> = OnceLock::new();

/// Get the CDP debug port, resolving it lazily.
fn get_debug_port() -> u16 {
    *DEBUG_PORT.get_or_init(crate::discovery::resolve_debug_port)
}

/// Build the modified command line for steamwebhelper with --remote-debugging-port.
/// Returns the new args vector if this is a steamwebhelper launch that needs injection.
pub fn build_webhelper_args(argv: &[String], port: u16) -> Option<Vec<String>> {
    let is_webhelper = argv.iter().any(|a| a.contains("steamwebhelper"));
    if !is_webhelper {
        return None;
    }
    // Skip --type= sub-processes
    if argv.iter().any(|a| a.starts_with("--type=")) {
        return None;
    }
    // Don't add if already present
    if argv.iter().any(|a| a.starts_with("--remote-debugging-port")) {
        return None;
    }

    let mut new_argv = argv.to_vec();
    new_argv.push(format!("--remote-debugging-port={}", port));
    new_argv.push("--remote-allow-origins=*".to_string());
    Some(new_argv)
}

/// Main CDP injection loop for Linux.
/// Called from the init() constructor.
/// Polls for steamwebhelper processes and injects via CDP.

/// Drain the bridge proxy queue from all injected targets and fulfill
/// pending HTTP requests from Rust (bypasses mixed-content blocking).
fn drain_bridge_queue(client: &mut crate::cdp::CdpClient, injected: &std::collections::HashSet<String>) {
    for target_id in injected {
        if target_id.is_empty() {
            continue;
        }
        if let Err(_) = client.attach_to_target(target_id) {
            continue;
        }
        let drain_expr = r#"(function(){
            if (!window.__lumaBridgeDrain) return '[]';
            return window.__lumaBridgeDrain();
        })()"#;
        let resp = match client.send_cdp_wait(
            &serde_json::json!({
                "id": 7700,
                "method": "Runtime.evaluate",
                "params": { "expression": drain_expr, "returnByValue": true }
            }),
            7700,
        ) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let queue_str = resp.get("result")
            .and_then(|r| r.get("result"))
            .and_then(|r| r.get("value"))
            .and_then(|v| v.as_str())
            .unwrap_or("[]");
        let queue: Vec<serde_json::Value> = serde_json::from_str(queue_str).unwrap_or_default();
        if queue.is_empty() {
            continue;
        }

        for req in &queue {
            let id = req.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let url = req.get("url").and_then(|v| v.as_str()).unwrap_or("");
            let method = req.get("method").and_then(|v| v.as_str()).unwrap_or("GET");
            let body = req.get("body").and_then(|v| v.as_str());

            let result = make_bridge_request(method, url, body);
            let inject_expr = format!(
                "window.__lumaBridgeResults['{}'] = {};",
                id.replace('\'', "\\'"),
                serde_json::to_string(&result).unwrap_or_default()
            );
            let _ = client.send_cdp_wait(
                &serde_json::json!({
                    "id": 7701,
                    "method": "Runtime.evaluate",
                    "params": { "expression": &inject_expr, "returnByValue": true }
                }),
                7701,
            );
        }
    }
}

/// Make an HTTP request to the local bridge (called from Rust, not CEF).
/// Tries port 21775 first (luma-lite primary), then 21777 (fallback).
fn make_bridge_request(method: &str, url: &str, body: Option<&str>) -> serde_json::Value {
    let client = match reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build() {
            Ok(c) => c,
            Err(e) => {
                return serde_json::json!({"status": 0, "body": "", "headers": {}, "statusText": format!("Client build error: {}", e)});
            }
        };

    // Try luma-lite port first (21777), then CDP proxy stub (21775)
    let ports = [21777u16, 21775];
    for port in &ports {
        // Replace port in URL
        let target_url = if url.contains("127.0.0.1:") {
            let prefix = url.split("127.0.0.1:").next().unwrap_or("");
            let suffix = url.splitn(2, "127.0.0.1:").nth(1).unwrap_or("");
            let path = suffix.splitn(2, '/').nth(1).unwrap_or("");
            if path.is_empty() {
                format!("http://127.0.0.1:{}/", port)
            } else {
                format!("http://127.0.0.1:{}/{}", port, path)
            }
        } else {
            url.to_string()
        };

        let mut req = match method {
            "POST" => {
                let mut r = client.post(&target_url);
                if let Some(b) = body {
                    r = r.body(b.to_string()).header("content-type", "application/json");
                }
                r
            }
            _ => client.get(&target_url),
        };

        match req.send() {
            Ok(resp) => {
                let status = resp.status().as_u16();
                // If we got a response, use this port for future requests
                let headers: serde_json::Map<String, serde_json::Value> = resp.headers()
                    .iter()
                    .map(|(k, v)| (k.as_str().to_string(), serde_json::Value::String(v.to_str().unwrap_or("").to_string())))
                    .collect();
                let body = resp.text().unwrap_or_default();
                return serde_json::json!({
                    "status": status,
                    "body": body,
                    "headers": headers,
                    "statusText": ""
                });
            }
            Err(_) => {
                // Try next port
                continue;
            }
        }
    }

    // All ports failed
    serde_json::json!({"status": 0, "body": "", "headers": {}, "statusText": "Bridge not available on any port"})
}

pub fn start_cdp_injection_loop() {
    let port = get_debug_port();

    // Publish discovery
    if let Err(e) = crate::discovery::publish_if_needed(port) {
        crate::log_to_temp(&format!(
            "[steamcdp] Failed to publish CDP discovery: {}",
            e
        ));
    }

    crate::log_to_temp(&format!(
        "[steamcdp] Linux CDP injection loop started, port={}",
        port
    ));

    // Wait for CDP to become available — exponential backoff: 100ms → 200ms → 400ms → 500ms
    let max_attempts = 30;
    for attempt in 1..=max_attempts {
        let delay_ms = std::cmp::min(100 * (1u64 << ((attempt - 1).min(2))), 500);
        std::thread::sleep(std::time::Duration::from_millis(delay_ms));

        match crate::cdp::CdpClient::connect(port) {
            Ok(mut client) => {
                crate::log_to_temp(&format!(
                    "[steamcdp] Connected to CDP (attempt {})",
                    attempt
                ));
                match crate::injector::inject_all(&mut client) {
                    Ok(()) => {
                        crate::log_to_temp("[steamcdp] Injection complete");
                    }
                    Err(e) => {
                        crate::log_to_temp(&format!("[steamcdp] Injection error: {}", e));
                    }
                }

                // Watch for new targets
                let mut injected_targets: std::collections::HashSet<String> =
                    std::collections::HashSet::new();
                if let Ok(targets) = client.get_targets() {
                    for t in &targets {
                        if t.target_type == "page"
                            && t.title != "Shutdown"
                            && !t.url.contains("createflags=2")
                            && !t.url.contains("centerOnBrowserID")
                        {
                            injected_targets.insert(t.id.clone());
                        }
                    }
                }

                crate::log_to_temp("[steamcdp] Watching for new targets...");
                let (theme_dir, theme_patches) = crate::injector::load_theme_patches();
                let mut recheck_counter = 0u32;
                // Track last-known URLs to detect store page navigations immediately
                let mut known_urls: std::collections::HashMap<String, String> = std::collections::HashMap::new();
                if let Ok(targets) = client.get_targets() {
                    for t in &targets {
                        if t.target_type == "page" && injected_targets.contains(&t.id) {
                            known_urls.insert(t.id.clone(), t.url.clone());
                        }
                    }
                }
                loop {
                    std::thread::sleep(std::time::Duration::from_secs(1));
                    recheck_counter += 1;

                    // Drain bridge proxy queue every cycle (~1s) so extension fetch
                    // requests are fulfilled within the 15s JS timeout
                    drain_bridge_queue(&mut client, &injected_targets);

                    // Every 30s, re-evaluate diagnostic on ALL injected targets to catch SPA navigations.
                    // Store pages are checked first since they're where the button appears.
                    if recheck_counter % 30 == 0 {
                        match client.get_targets() {
                            Ok(all_targets) => {
                                let page_count = all_targets.iter().filter(|t| t.target_type == "page").count();
                                crate::log_to_temp(&format!("[steamcdp] Recheck: {} total targets ({} pages), {} injected", all_targets.len(), page_count, injected_targets.len()));

                                // Collect injected page targets, sort store pages first
                                let mut pages: Vec<_> = all_targets.iter()
                                    .filter(|t| t.target_type == "page" && injected_targets.contains(&t.id))
                                    .collect();
                                pages.sort_by(|a, b| {
                                    let a_store = a.url.contains("store.steampowered.com");
                                    let b_store = b.url.contains("store.steampowered.com");
                                    b_store.cmp(&a_store) // store pages first
                                });

                                let mut checked = 0u32;
                                for t in pages {
                                    checked += 1;
                                    if let Err(_) = client.attach_to_target(&t.id) {
                                        continue;
                                    }
                                    let diag_expr = r#"JSON.stringify({url:window.location.href.substring(0,200),title:document.title.substring(0,80),bodyLen:document.body?document.body.innerHTML.length:-1,hasLuma:!!window.__lumaforge_ssh__,lumaActive:window.__lumaforge_ssh__&&window.__lumaforge_ssh__.active,lumaAppId:window.__lumaforge_ssh__&&window.__lumaforge_ssh__.currentAppId,btnExists:!!document.getElementById('luma-action-btn'),appLinks:document.querySelectorAll('a[href*="/app/"]').length})"#;
                                    let msg_id = 9000 + checked as u64;
                                    let mut diag_val = String::new();
                                    if let Ok(resp) = client.send_cdp_wait(
                                        &serde_json::json!({
                                            "id": msg_id,
                                            "method": "Runtime.evaluate",
                                            "params": { "expression": diag_expr, "returnByValue": true }
                                        }),
                                        msg_id,
                                    ) {
                                        if let Some(v) = resp.get("result").and_then(|r| r.get("result")).and_then(|r| r.get("value")).and_then(|v| v.as_str()) {
                                            diag_val = v.to_string();
                                            crate::log_to_temp(&format!(
                                                "[steamcdp] Recheck p#{}: {}",
                                                checked, v
                                            ));
                                        }
                                    } else {
                                        crate::log_to_temp(&format!(
                                            "[steamcdp] Recheck p#{}: CDP eval failed",
                                            checked
                                        ));
                                    }

                                    let has_luma = diag_val.contains("\"hasLuma\":true");
                                    if !has_luma {
                                        crate::log_to_temp(&format!(
                                            "[steamcdp] Re-injecting target (hasLuma=false): id={}, title=\"{}\", url={}",
                                            t.id, t.title, &t.url[..t.url.len().min(100)]
                                        ));
                                        let plugins = crate::plugin_loader_linux::load_all_plugins().unwrap_or_default();
                                        if let Err(e) = crate::injector::inject_into_target(
                                            &mut client, t, &plugins, &theme_dir, &theme_patches, checked as usize,
                                        ) {
                                            crate::log_to_temp(&format!("[steamcdp] Re-inject failed: {}", e));
                                        }
                                    }
                                }
                            }
                            Err(_) => {}
                        }
                    }

                    match client.get_targets() {
                        Ok(new_targets) => {
                            for t in &new_targets {
                                if t.target_type == "page" && !injected_targets.contains(&t.id) {
                                    // Skip shutdown/close targets — injecting into these can destabilize Steam
                                    if t.title == "Shutdown" || t.url.contains("createflags=2") {
                                        injected_targets.insert(t.id.clone());
                                        continue;
                                    }
                                    crate::log_to_temp(&format!(
                                        "[steamcdp] New target: id={}, title=\"{}\", url={}",
                                        t.id,
                                        t.title,
                                        &t.url[..t.url.len().min(100)]
                                    ));
                                    let plugins = crate::plugin_loader_linux::load_all_plugins()
                                        .unwrap_or_default();
                                    if let Err(e) = crate::injector::inject_into_target(
                                        &mut client,
                                        t,
                                        &plugins,
                                        &theme_dir,
                                        &theme_patches,
                                        injected_targets.len() + 1,
                                    ) {
                                        crate::log_to_temp(&format!(
                                            "[steamcdp] New target injection failed: {}",
                                            e
                                        ));
                                    } else {
                                        injected_targets.insert(t.id.clone());
                                        known_urls.insert(t.id.clone(), t.url.clone());
                                    }
                                } else if t.target_type == "page" && injected_targets.contains(&t.id) {
                                    // Detect URL change on existing store target → re-inject immediately
                                    if t.url.contains("store.steampowered.com") {
                                        if let Some(prev_url) = known_urls.get(&t.id) {
                                            if prev_url != &t.url {
                                                crate::log_to_temp(&format!(
                                                    "[steamcdp] Store URL changed: id={}, prev={}, new={}",
                                                    t.id,
                                                    &prev_url[..prev_url.len().min(80)],
                                                    &t.url[..t.url.len().min(80)]
                                                ));
                                                let plugins = crate::plugin_loader_linux::load_all_plugins()
                                                    .unwrap_or_default();
                                                if let Err(e) = crate::injector::inject_into_target(
                                                    &mut client,
                                                    t,
                                                    &plugins,
                                                    &theme_dir,
                                                    &theme_patches,
                                                    1,
                                                ) {
                                                    crate::log_to_temp(&format!("[steamcdp] URL-change re-inject failed: {}", e));
                                                }
                                            }
                                        }
                                        known_urls.insert(t.id.clone(), t.url.clone());
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            crate::log_to_temp(&format!("[steamcdp] get_targets error: {}, trying reconnect...", e));
                            match crate::cdp::CdpClient::connect(port) {
                                Ok(mut new_client) => {
                                    crate::log_to_temp("[steamcdp] Reconnected to CDP");
                                    client = new_client;
                                    injected_targets.clear();
                                    known_urls.clear();
                                    if let Ok(targets) = client.get_targets() {
                                        for t in &targets {
                                            if t.target_type == "page" {
                                                injected_targets.insert(t.id.clone());
                                                known_urls.insert(t.id.clone(), t.url.clone());
                                            }
                                        }
                                    }
                                }
                                Err(e2) => {
                                    crate::log_to_temp(&format!("[steamcdp] Reconnect failed: {}", e2));
                                }
                            }
                        }
                    }
                }
                crate::log_to_temp("[steamcdp] CDP connection lost, reconnecting...");
            }
            Err(e) => {
                crate::log_to_temp(&format!(
                    "[steamcdp] CDP connect attempt {} failed: {}",
                    attempt, e
                ));
            }
        }
    }
    crate::log_to_temp("[steamcdp] Max attempts reached, giving up");
}

/// Inject --remote-debugging-port into execve arguments.
/// This is called from the execve hook (LD_PRELOAD).
/// Returns the modified argv if injection is needed, or None to pass through.
pub fn maybe_inject_debug_port(argv: &[String]) -> Option<Vec<String>> {
    let port = get_debug_port();
    build_webhelper_args(argv, port)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_inject_debug_port() {
        let argv = vec![
            "steamwebhelper".to_string(),
            "--some-arg".to_string(),
        ];
        let result = maybe_inject_debug_port(&argv);
        assert!(result.is_some());
        let new_argv = result.unwrap();
        assert!(new_argv.iter().any(|a| a.contains("--remote-debugging-port")));
    }

    #[test]
    fn test_skip_type_flag() {
        let argv = vec![
            "steamwebhelper".to_string(),
            "--type=renderer".to_string(),
        ];
        assert!(maybe_inject_debug_port(&argv).is_none());
    }

    #[test]
    fn test_skip_non_webhelper() {
        let argv = vec![
            "steam".to_string(),
            "--some-arg".to_string(),
        ];
        assert!(maybe_inject_debug_port(&argv).is_none());
    }

    #[test]
    fn test_skip_already_has_port() {
        let argv = vec![
            "steamwebhelper".to_string(),
            "--remote-debugging-port=9222".to_string(),
        ];
        assert!(maybe_inject_debug_port(&argv).is_none());
    }
}
