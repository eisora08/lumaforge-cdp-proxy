mod bridge;
mod cdp;
mod discovery;
mod hook;
mod injector;
mod ipc;
mod lua_backend;
mod plugin;
mod plugin_loader;

use std::ffi::c_void;
use std::mem;
use windows_sys::Win32::System::LibraryLoader::GetModuleFileNameW;
use windows_sys::Win32::System::Threading::GetCurrentProcessId;
use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW,
    TH32CS_SNAPPROCESS, PROCESSENTRY32W,
};
use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};

const DLL_PROCESS_ATTACH: u32 = 1;
const DLL_PROCESS_DETACH: u32 = 0;

// Función para terminar procesos steamwebhelper.exe existentes
unsafe fn kill_existing_webhelpers() {
    let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
    if snapshot == INVALID_HANDLE_VALUE {
        log_to_temp("[steamcdp] Failed to create process snapshot");
        return;
    }

    let mut entry: PROCESSENTRY32W = mem::zeroed();
    entry.dwSize = mem::size_of::<PROCESSENTRY32W>() as u32;

    if Process32FirstW(snapshot, &mut entry) == 0 {
        CloseHandle(snapshot);
        return;
    }

    let mut killed_count = 0;
    loop {
        let name = String::from_utf16_lossy(&entry.szExeFile);
        if name.to_lowercase().contains("steamwebhelper.exe") {
            let handle = OpenProcess(PROCESS_TERMINATE, 0, entry.th32ProcessID);
            if !handle.is_null() {
                if TerminateProcess(handle, 0) != 0 {
                    killed_count += 1;
                }
                CloseHandle(handle);
            }
        }
        if Process32NextW(snapshot, &mut entry) == 0 {
            break;
        }
    }
    CloseHandle(snapshot);

    if killed_count > 0 {
        log_to_temp(&format!(
            "[steamcdp] Terminated {} existing steamwebhelper processes",
            killed_count
        ));
    }
}

#[no_mangle]
#[allow(non_snake_case)]
unsafe extern "system" fn DllMain(
    _hinst: *const c_void,
    fdw_reason: u32,
    _lpv_reserved: *const c_void,
) -> i32 {
    match fdw_reason {
        DLL_PROCESS_ATTACH => {
            let pid = GetCurrentProcessId();

            let mut exe_buf = [0u16; 260];
            let mut exe_len = GetModuleFileNameW(
                std::ptr::null_mut(),
                exe_buf.as_mut_ptr(),
                exe_buf.len() as u32,
            );
            if exe_len > 0 {
                exe_len = exe_len.min(259);
            }
            let exe_path = String::from_utf16_lossy(&exe_buf[..exe_len as usize]);

            log_to_temp(&format!(
                "[steamcdp] DLL loaded in PID {} exe=\"{}\"",
                pid, exe_path
            ));

            // Matar webhelpers existentes para forzar reinicio
            kill_existing_webhelpers();

            // Instalar hooks
            match hook::install_hook() {
                Ok(()) => {
                    log_to_temp(&format!(
                        "[steamcdp] Hooks installed synchronously in PID {}",
                        pid
                    ));
                }
                Err(e) => {
                    log_to_temp(&format!("[steamcdp] Failed to install hooks: {}", e));
                }
            }

            // 🔥 Iniciar el servidor IPC y bridge en threads separados
            std::thread::spawn(|| {
                if let Err(e) = crate::ipc::start_ipc_server() {
                    log_to_temp(&format!("[steamcdp] IPC server error: {}", e));
                }
            });

            // Initialize Lua backends for plugins that have them
            match plugin_loader::load_all_plugins() {
                Ok(plugins) => {
                    for p in &plugins {
                        if let Some(ref bc) = p.backend_config {
                            match lua_backend::load_lua_backend(&p._id, &p._dir, bc) {
                                Ok(()) => {
                                    log_to_temp(&format!("[steamcdp] Lua backend loaded for {}", p._id));
                                }
                                Err(e) => {
                                    log_to_temp(&format!("[steamcdp] Lua backend error for {}: {}", p._id, e));
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    log_to_temp(&format!("[steamcdp] Plugin load error: {}", e));
                }
            }

            crate::bridge::start_bridge_server();
        }
        DLL_PROCESS_DETACH => {}
        _ => {}
    }
    1
}

pub(crate) fn log_to_temp(msg: &str) {
    use std::fs::OpenOptions;
    use std::io::Write;
    use windows_sys::Win32::System::Environment::GetEnvironmentVariableW;

    unsafe {
        let mut buf = [0u16; 260];
        let temp_key: Vec<u16> = "TEMP".encode_utf16().chain(std::iter::once(0)).collect();
        let len = GetEnvironmentVariableW(temp_key.as_ptr(), buf.as_mut_ptr(), buf.len() as u32);

        let temp_dir = if len > 0 && (len as usize) < buf.len() {
            String::from_utf16_lossy(&buf[..len as usize])
        } else {
            "C:\\Windows\\Temp".to_string()
        };

        let path = format!("{}\\steamcdp_proxy.log", temp_dir);
        let mut file = match OpenOptions::new().create(true).append(true).open(&path) {
            Ok(f) => f,
            Err(_) => return,
        };

        let line = format!("{}\r\n", msg);
        let _ = file.write_all(line.as_bytes());
    }
}