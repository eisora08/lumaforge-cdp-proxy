use crate::cdp::{CdpClient, Target};
use crate::plugin_loader::load_enabled_plugins;
use serde_json::json;

pub fn inject_all_plugins(client: &mut CdpClient) -> Result<(), String> {
    let plugins = load_enabled_plugins()?;
    if plugins.is_empty() {
        crate::log_to_temp("[steamcdp] No enabled plugins to inject");
        return Ok(());
    }

    let targets = client.get_targets()?;
    let pages: Vec<&Target> = targets.iter()
        .filter(|t| t.target_type == "page")
        .collect();

    if pages.is_empty() {
        crate::log_to_temp("[steamcdp] No page targets found");
        return Ok(());
    }

    crate::log_to_temp(&format!(
        "[steamcdp] Found {} page targets, {} plugins",
        pages.len(),
        plugins.len()
    ));

    for (idx, target) in pages.iter().enumerate() {
        inject_into_target(client, target, &plugins, idx + 1)?;
    }

    Ok(())
}

pub fn inject_into_target(
    client: &mut CdpClient,
    target: &Target,
    plugins: &[crate::plugin::LoadedPlugin],
    target_num: usize,
) -> Result<(), String> {
    crate::log_to_temp(&format!(
        "[steamcdp] Target #{}: id={}, url={}",
        target_num, target.id, target.url
    ));
    client.attach_to_target(&target.id)?;

    let mut msg_id = 100u64;

    let resp = client.send_cdp_wait(&json!({
        "id": msg_id,
        "method": "Page.enable",
        "params": {}
    }), msg_id)?;
    if let Some(err) = resp.get("error") {
        crate::log_to_temp(&format!("[steamcdp] Page.enable error: {}", err));
    }
    msg_id += 1;

    let resp = client.send_cdp_wait(&json!({
        "id": msg_id,
        "method": "Page.setBypassCSP",
        "params": {"enabled": true}
    }), msg_id)?;
    if let Some(err) = resp.get("error") {
        crate::log_to_temp(&format!("[steamcdp] setBypassCSP error: {}", err));
    }
    msg_id += 1;

    for plugin in plugins {
        let resp = client.send_cdp_wait(&json!({
            "id": msg_id,
            "method": "Page.addScriptToEvaluateOnNewDocument",
            "params": {
                "source": &plugin.code,
                "runImmediately": true
            }
        }), msg_id)?;

        let script_id = resp.get("result")
            .and_then(|r| r.get("identifier"))
            .and_then(|i| i.as_str())
            .unwrap_or("none");

        if let Some(err) = resp.get("error") {
            crate::log_to_temp(&format!(
                "[steamcdp] addScript error for '{}': {}",
                plugin.name, err
            ));
        } else {
            crate::log_to_temp(&format!(
                "[steamcdp] Registered '{}' script_id={}",
                plugin.name, script_id
            ));
        }
        msg_id += 1;

        let matches_url = match plugin.target_url {
            Some(ref filter) => target.url.to_lowercase().contains(&filter.to_lowercase()),
            None => true,
        };

        if matches_url {
            let resp = client.send_cdp_wait(&json!({
                "id": msg_id,
                "method": "Runtime.evaluate",
                "params": {
                    "expression": &plugin.code,
                    "awaitPromise": true,
                    "returnByValue": true
                }
            }), msg_id)?;

            if let Some(err) = resp.get("error") {
                crate::log_to_temp(&format!(
                    "[steamcdp] Runtime.evaluate error for '{}': {}",
                    plugin.name, err
                ));
            } else {
                crate::log_to_temp(&format!(
                    "[steamcdp] Executed '{}' in target #{}",
                    plugin.name, target_num
                ));
            }
            msg_id += 1;
        }

        crate::log_to_temp(&format!(
            "[steamcdp] Injected plugin '{}' into target #{} (url_match={}, registered=true)",
            plugin.name, target_num, matches_url
        ));
    }

    Ok(())
}
