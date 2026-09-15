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
    Some(new_argv)
}

/// Main CDP injection loop for Linux.
/// Called from the init() constructor.
/// Polls for steamwebhelper processes and injects via CDP.
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

    // Wait for CDP to become available
    let max_attempts = 30;
    for attempt in 1..=max_attempts {
        std::thread::sleep(std::time::Duration::from_secs(1));

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
                        if t.target_type == "page" {
                            injected_targets.insert(t.id.clone());
                        }
                    }
                }

                crate::log_to_temp("[steamcdp] Watching for new targets...");
                let (theme_dir, theme_patches) = crate::injector::load_theme_patches();
                while client.is_alive() {
                    std::thread::sleep(std::time::Duration::from_secs(1));

                    if let Ok(new_targets) = client.get_targets() {
                        for t in &new_targets {
                            if t.target_type == "page" && !injected_targets.contains(&t.id) {
                                crate::log_to_temp(&format!(
                                    "[steamcdp] New target: id={}, title=\"{}\", url={}",
                                    t.id,
                                    t.title,
                                    &t.url[..t.url.len().min(100)]
                                ));
                                // TODO: Load plugins from plugin_loader_linux
                                let plugins = Vec::new();
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
