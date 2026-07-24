// src/ipc.rs
use std::sync::Once;
use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Pipes::{CreateNamedPipeA, ConnectNamedPipe};
use windows_sys::Win32::Storage::FileSystem::{ReadFile, WriteFile};

// Constantes de pipes (definidas localmente)
const PIPE_ACCESS_DUPLEX: u32 = 0x00000003;
const PIPE_TYPE_MESSAGE: u32 = 0x00000004;
const PIPE_READMODE_MESSAGE: u32 = 0x00000002;
const PIPE_WAIT: u32 = 0x00000000;

const PIPE_NAME: &str = r"\\.\pipe\lumalite_core\0";

pub fn start_ipc_server() -> Result<(), String> {
    static STARTED: Once = Once::new();
    let result = Ok(());
    STARTED.call_once(|| {
        std::thread::spawn(|| {
            loop {
                unsafe {
                    let pipe = CreateNamedPipeA(
                        PIPE_NAME.as_ptr(),
                        PIPE_ACCESS_DUPLEX,
                        PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT,
                        1,
                        1024,
                        1024,
                        0,
                        std::ptr::null_mut(),
                    );
                    if pipe == INVALID_HANDLE_VALUE {
                        std::thread::sleep(std::time::Duration::from_millis(100));
                        continue;
                    }

                    if ConnectNamedPipe(pipe, std::ptr::null_mut()) == 0 {
                        CloseHandle(pipe);
                        continue;
                    }

                    let mut buffer = [0u8; 1024];
                    let mut bytes_read = 0u32;
                    if ReadFile(
                        pipe,
                        buffer.as_mut_ptr(),
                        buffer.len() as u32,
                        &mut bytes_read,
                        std::ptr::null_mut(),
                    ) == 0 {
                        CloseHandle(pipe);
                        continue;
                    }

                    let command = String::from_utf8_lossy(&buffer[..bytes_read as usize]);
                    let response = handle_command(&command);
                    let response_bytes = response.as_bytes();
                    let mut bytes_written = 0u32;
                    let _ = WriteFile(
                        pipe,
                        response_bytes.as_ptr(),
                        response_bytes.len() as u32,
                        &mut bytes_written,
                        std::ptr::null_mut(),
                    );

                    CloseHandle(pipe);
                }
            }
        });
    });
    result
}

fn write_theme_reload_signal() -> Result<(), String> {
    let local_appdata = std::env::var("LOCALAPPDATA")
        .map_err(|_| "LOCALAPPDATA not set".to_string())?;
    let runtime_dir = std::path::PathBuf::from(&local_appdata)
        .join("LumaForge")
        .join("runtime");
    std::fs::create_dir_all(&runtime_dir)
        .map_err(|e| format!("Failed to create runtime dir: {}", e))?;
    let signal_path = runtime_dir.join("theme-reload");
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    std::fs::write(&signal_path, timestamp.to_string())
        .map_err(|e| format!("Failed to write signal file: {}", e))
}

fn handle_command(command: &str) -> String {
    let cmd = command.trim();
    match cmd {
        "reload" => {
            crate::log_to_temp("[steamcdp] IPC: reload command received");
            match crate::plugin_loader::load_enabled_plugins() {
                Ok(plugins) => {
                    let names: Vec<String> = plugins.iter().map(|p| p.name.clone()).collect();
                    format!(
                        r#"{{"status":"ok","message":"Plugins reloaded","count":{},"plugins":{:?}}}"#,
                        names.len(), names
                    )
                }
                Err(e) => format!(r#"{{"status":"error","message":"{}"}}"#, e),
            }
        }
        "reload-theme" => {
            crate::log_to_temp("[steamcdp] IPC: reload-theme command received");
            match write_theme_reload_signal() {
                Ok(()) => r#"{"status":"ok","message":"Theme reload signal sent"}"#.to_string(),
                Err(e) => format!(r#"{{"status":"error","message":"{}"}}"#, e),
            }
        }
        "status" => {
            match crate::plugin_loader::load_enabled_plugins() {
                Ok(plugins) => {
                    let names: Vec<String> = plugins.iter().map(|p| p.name.clone()).collect();
                    format!(r#"{{"status":"ok","plugins":{:?}}}"#, names)
                }
                Err(e) => format!(r#"{{"status":"error","message":"{}"}}"#, e),
            }
        }
        // ── Theme commands ──
        "list-themes" => {
            let themes = crate::theme::list_available_themes();
            let active = crate::theme::read_active_theme_name().unwrap_or_default();
            format!(
                r#"{{"status":"ok","active":"{}","themes":{}}}"#,
                active,
                serde_json::to_string(&themes).unwrap_or_else(|_| "[]".to_string())
            )
        }
        "active-theme" => {
            match crate::theme::load_active_theme() {
                Some(theme) => {
                    format!(
                        r#"{{"status":"ok","name":"{}","patches":{},"conditions":{},"webkit_css":{},"webkit_js":{},"root_colors":{}}}"#,
                        theme.name,
                        theme.manifest.patches.len(),
                        theme.manifest.conditions.len(),
                        theme.manifest.webkit_css.is_some(),
                        theme.manifest.webkit_js.is_some(),
                        theme.manifest.root_colors.is_some(),
                    )
                }
                None => r#"{"status":"error","message":"No active theme"}"#.to_string(),
            }
        }
        _ if cmd.starts_with("set-theme ") => {
            let theme_name = cmd.strip_prefix("set-theme ").unwrap_or("").trim();
            if theme_name.is_empty() {
                return r#"{"status":"error","message":"Theme name required"}"#.to_string();
            }
            match crate::theme::write_active_theme_name(theme_name) {
                Ok(()) => {
                    // Re-export manifest for cef_hook
                    if let Err(e) = crate::theme::export_theme_for_cef_hook() {
                        crate::log_to_temp(&format!("[steamcdp] Theme export failed after set: {}", e));
                    }
                    // Signal reload
                    let _ = write_theme_reload_signal();
                    format!(r#"{{"status":"ok","message":"Theme set to '{}'","theme":"{}"}}"#, theme_name, theme_name)
                }
                Err(e) => format!(r#"{{"status":"error","message":"{}"}}"#, e),
            }
        }
        _ if cmd.starts_with("set-condition ") => {
            // format: set-condition <condition_name> <value>
            let rest = cmd.strip_prefix("set-condition ").unwrap_or("").trim();
            let parts: Vec<&str> = rest.splitn(2, ' ').collect();
            if parts.len() < 2 {
                return r#"{"status":"error","message":"Usage: set-condition <name> <value>"}"#.to_string();
            }
            let cond_name = parts[0];
            let cond_value = parts[1];
            let theme_name = crate::theme::read_active_theme_name().unwrap_or_default();
            let mut config = crate::theme::ThemeConditionConfig::load();
            config.set_selection(&theme_name, cond_name, cond_value);
            match config.save() {
                Ok(()) => {
                    // Re-export manifest with new condition values
                    if let Err(e) = crate::theme::export_theme_for_cef_hook() {
                        crate::log_to_temp(&format!("[steamcdp] Theme export failed after condition: {}", e));
                    }
                    let _ = write_theme_reload_signal();
                    format!(
                        r#"{{"status":"ok","message":"Condition '{}' set to '{}'","theme":"{}"}}"#,
                        cond_name, cond_value, theme_name
                    )
                }
                Err(e) => format!(r#"{{"status":"error","message":"{}"}}"#, e),
            }
        }
        _ if cmd.starts_with("get-condition ") => {
            let cond_name = cmd.strip_prefix("get-condition ").unwrap_or("").trim();
            let theme_name = crate::theme::read_active_theme_name().unwrap_or_default();
            let config = crate::theme::ThemeConditionConfig::load();
            let value = config.selections
                .get(&theme_name)
                .and_then(|c| c.get(cond_name))
                .cloned()
                .unwrap_or_default();
            format!(
                r#"{{"status":"ok","condition":"{}","value":"{}","theme":"{}"}}"#,
                cond_name, value, theme_name
            )
        }
        "theme-info" => {
            match crate::theme::load_active_theme() {
                Some(theme) => {
                    let conditions: Vec<String> = theme.manifest.conditions.keys().cloned().collect();
                    let patches_info: Vec<String> = theme.manifest.patches.iter()
                        .map(|p| format!("regex='{}' css={:?} js={:?}", p.match_regex_string, p.target_css, p.target_js))
                        .collect();
                    format!(
                        r#"{{"status":"ok","name":"{}","author":"{}","description":"{}","patches":{},"conditions":{},"webkit_css":{},"webkit_js":{},"root_colors":{},"condition_names":{},"patch_details":{}}}"#,
                        theme.manifest.name,
                        theme.manifest.author,
                        theme.manifest.description,
                        theme.manifest.patches.len(),
                        theme.manifest.conditions.len(),
                        theme.manifest.webkit_css.is_some(),
                        theme.manifest.webkit_js.is_some(),
                        theme.manifest.root_colors.is_some(),
                        serde_json::to_string(&conditions).unwrap_or_default(),
                        serde_json::to_string(&patches_info).unwrap_or_default(),
                    )
                }
                None => r#"{"status":"error","message":"No active theme"}"#.to_string(),
            }
        }
        _ => r#"{"status":"error","message":"Unknown command"}"#.to_string(),
    }
}