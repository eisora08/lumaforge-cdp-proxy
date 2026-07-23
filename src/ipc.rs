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
    match command.trim() {
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
        _ => r#"{"status":"error","message":"Unknown command"}"#.to_string(),
    }
}