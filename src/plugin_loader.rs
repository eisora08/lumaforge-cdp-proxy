use std::fs;
use std::path::PathBuf;
use crate::plugin::{PluginManifest, ExtensionConfig, LoadedPlugin};

pub fn get_plugins_dir() -> Result<PathBuf, String> {
    if let Ok(override_dir) = std::env::var("LUMA_FORGE_PLUGINS_DIR") {
        return Ok(PathBuf::from(override_dir));
    }

    let local_app_data = std::env::var("LOCALAPPDATA")
        .map_err(|_| "LOCALAPPDATA not set".to_string())?;
    Ok(PathBuf::from(local_app_data).join("LumaForge").join("plugins"))
}

pub fn load_all_plugins() -> Result<Vec<LoadedPlugin>, String> {
    let plugins_dir = get_plugins_dir()?;
    if !plugins_dir.exists() {
        fs::create_dir_all(&plugins_dir)
            .map_err(|e| format!("Failed to create plugins dir: {}", e))?;
        return Ok(Vec::new());
    }

    let mut loaded = Vec::new();
    for entry in fs::read_dir(&plugins_dir)
        .map_err(|e| format!("Failed to read plugins dir: {}", e))?
    {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let manifest_path = path.join("manifest.json");
        if !manifest_path.exists() {
            continue;
        }

        let manifest_content = match fs::read_to_string(&manifest_path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let manifest: PluginManifest = match serde_json::from_str(&manifest_content) {
            Ok(m) => m,
            Err(e) => {
                crate::log_to_temp(&format!(
                    "[steamcdp] Failed to parse manifest {}: {}",
                    manifest_path.display(), e
                ));
                continue;
            }
        };

        let config_path = path.join("extension-config.json");
        let ext_config: ExtensionConfig = if config_path.exists() {
            fs::read_to_string(&config_path)
                .ok()
                .and_then(|c| serde_json::from_str(&c).ok())
                .unwrap_or(ExtensionConfig {
                    enabled: true,
                    source: None,
                })
        } else {
            ExtensionConfig {
                enabled: true,
                source: None,
            }
        };

        if !ext_config.enabled {
            continue;
        }

        let activation = manifest.activation.as_ref();
        let cef_injection = activation.map_or(false, |a| a.cef_injection);
        if !cef_injection {
            continue;
        }

        let inject_script = activation
            .map(|a| a.inject_script.as_str())
            .unwrap_or("inject.js");

        let code_path = path.join(inject_script);
        if !code_path.exists() {
            crate::log_to_temp(&format!(
                "[steamcdp] Plugin {} missing inject script: {}",
                manifest.id.as_deref().unwrap_or("unknown"),
                code_path.display()
            ));
            continue;
        }

        let code = match fs::read_to_string(&code_path) {
            Ok(c) => c,
            Err(e) => {
                crate::log_to_temp(&format!(
                    "[steamcdp] Failed to read {}: {}",
                    code_path.display(), e
                ));
                continue;
            }
        };

        let plugin_id = manifest.id.clone().unwrap_or_else(|| {
            path.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string()
        });

        let plugin_name = manifest.name.clone().unwrap_or_else(|| plugin_id.clone());
        let target_url = activation.and_then(|a| a.target_url.clone());

        loaded.push(LoadedPlugin {
            _id: plugin_id,
            name: plugin_name,
            _version: manifest.version.clone(),
            code,
            target_url,
            _dir: path.clone(),
            backend_config: manifest.backend.clone(),
        });
    }

    Ok(loaded)
}

pub fn load_enabled_plugins() -> Result<Vec<LoadedPlugin>, String> {
    load_all_plugins()
}
