mod bridge;
mod cdp;
mod discovery;
mod injector;
mod ipc;
mod lua_backend;
pub mod platform;
mod plugin;
pub mod theme;

#[cfg(target_os = "windows")]
mod hook;
#[cfg(target_os = "windows")]
mod package_installer;
#[cfg(target_os = "windows")]
mod plugin_loader;

#[cfg(target_os = "linux")]
mod hook_linux;
#[cfg(target_os = "linux")]
mod plugin_loader_linux;

use std::sync::Mutex;

// ---------------------------------------------------------------------------
// Buffered logging
// ---------------------------------------------------------------------------

static LOG_BUFFER: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn flush_log_buffer() {
    let Ok(mut buf) = LOG_BUFFER.lock() else {
        return;
    };
    if buf.is_empty() {
        return;
    }

    use std::fs::OpenOptions;
    use std::io::Write;

    let path = platform::log_file_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) {
        for line in buf.drain(..) {
            let _ = file.write_all(line.as_bytes());
        }
    }
}

pub(crate) fn log_to_temp(msg: &str) {
    let line = format!("{}\r\n", msg);
    let should_flush = {
        let Ok(mut buf) = LOG_BUFFER.lock() else {
            return;
        };
        buf.push(line);
        buf.len() >= 10
    };
    if should_flush {
        flush_log_buffer();
    }
}

// ---------------------------------------------------------------------------
// Stealth kill — polls for webhelpers, then kills all at once
// ---------------------------------------------------------------------------

#[cfg(target_os = "windows")]
unsafe fn stealth_kill_all_webhelpers() {
    use std::mem;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};

    for _ in 0..120 {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            std::thread::sleep(std::time::Duration::from_millis(500));
            continue;
        }

        let mut entry: PROCESSENTRY32W = mem::zeroed();
        entry.dwSize = mem::size_of::<PROCESSENTRY32W>() as u32;

        if Process32FirstW(snapshot, &mut entry) == 0 {
            CloseHandle(snapshot);
            std::thread::sleep(std::time::Duration::from_millis(500));
            continue;
        }

        let mut pids: Vec<u32> = Vec::new();
        loop {
            let len = entry
                .szExeFile
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(entry.szExeFile.len());
            let name = String::from_utf16_lossy(&entry.szExeFile[..len]);
            if name.eq_ignore_ascii_case("steamwebhelper.exe") {
                pids.push(entry.th32ProcessID);
            }
            if Process32NextW(snapshot, &mut entry) == 0 {
                break;
            }
        }
        CloseHandle(snapshot);

        if pids.is_empty() {
            std::thread::sleep(std::time::Duration::from_millis(500));
            continue;
        }

        for pid in &pids {
            let handle = OpenProcess(PROCESS_TERMINATE, 0, *pid);
            if !handle.is_null() {
                TerminateProcess(handle, 0);
                CloseHandle(handle);
            } else {
                let _ = std::process::Command::new("taskkill")
                    .args(["/F", "/PID", &pid.to_string()])
                    .output();
            }
        }
        return;
    }
}

#[cfg(target_os = "linux")]
fn stealth_kill_all_webhelpers() {
    use std::io::Read;

    for _ in 0..120 {
        let mut pids: Vec<u32> = Vec::new();

        // Scan /proc for steamwebhelper processes
        if let Ok(entries) = std::fs::read_dir("/proc") {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                if let Ok(pid) = name_str.parse::<u32>() {
                    let cmdline_path = entry.path().join("cmdline");
                    if let Ok(mut f) = std::fs::File::open(&cmdline_path) {
                        let mut buf = String::new();
                        let _ = f.read_to_string(&mut buf);
                        if buf.contains("steamwebhelper") {
                            pids.push(pid);
                        }
                    }
                }
            }
        }

        if pids.is_empty() {
            std::thread::sleep(std::time::Duration::from_millis(500));
            continue;
        }

        for pid in &pids {
            let _ = std::process::Command::new("kill")
                .args(["-9", &pid.to_string()])
                .output();
        }
        return;
    }
}

// ---------------------------------------------------------------------------
// DllMain (Windows) / #[ctor] (Linux)
// ---------------------------------------------------------------------------

#[cfg(target_os = "windows")]
#[no_mangle]
#[allow(non_snake_case)]
unsafe extern "system" fn DllMain(
    _hinst: *const std::ffi::c_void,
    fdw_reason: u32,
    _lpv_reserved: *const std::ffi::c_void,
) -> i32 {
    use windows_sys::Win32::System::LibraryLoader::GetModuleFileNameW;
    use windows_sys::Win32::System::Threading::GetCurrentProcessId;

    const DLL_PROCESS_ATTACH: u32 = 1;
    const DLL_PROCESS_DETACH: u32 = 0;

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

            // 1. Hooks — synchronous
            match hook::install_hook() {
                Ok(()) => {
                    log_to_temp(&format!("[steamcdp] Hooks installed in PID {}", pid));
                }
                Err(e) => {
                    log_to_temp(&format!("[steamcdp] Failed to install hooks: {}", e));
                }
            }

            // 2. Kill webhelpers — background thread, polls until found
            std::thread::spawn(|| {
                stealth_kill_all_webhelpers();
            });

            // 3. Theme + IPC — background thread
            std::thread::spawn(|| {
                if let Err(e) = theme::export_theme_for_cef_hook() {
                    log_to_temp(&format!("[steamcdp] Theme export failed: {}", e));
                }
                if let Err(e) = crate::ipc::start_ipc_server() {
                    log_to_temp(&format!("[steamcdp] IPC server error: {}", e));
                }
            });

            // 4. Lua backends — separate thread
            std::thread::spawn(|| match plugin_loader::load_all_plugins() {
                Ok(plugins) => {
                    for p in &plugins {
                        if let Some(ref bc) = p.backend_config {
                            if let Err(e) = lua_backend::load_lua_backend(&p._id, &p._dir, bc) {
                                log_to_temp(&format!(
                                    "[steamcdp] Lua backend error for {}: {}",
                                    p._id, e
                                ));
                            }
                        }
                    }
                }
                Err(e) => {
                    log_to_temp(&format!("[steamcdp] Plugin load error: {}", e));
                }
            });

            // 5. Bridge server — must not block DllMain
            std::thread::spawn(|| {
                crate::bridge::start_bridge_server();
            });
        }
        DLL_PROCESS_DETACH => {
            flush_log_buffer();
        }
        _ => {}
    }
    1
}

#[cfg(target_os = "linux")]
#[ctor::ctor]
fn init() {
    use std::io::Read;

    // Identify which process loaded us
    let mut cmdline = String::new();
    if let Ok(mut f) = std::fs::File::open("/proc/self/cmdline") {
        let _ = f.read_to_string(&mut cmdline);
    }
    let exe_name = cmdline.split('\0').next().unwrap_or("unknown");

    log_to_temp(&format!(
        "[steamcdp] .so loaded in PID {} exe=\"{}\"",
        std::process::id(),
        exe_name
    ));

    // 1. Kill existing webhelpers
    std::thread::spawn(|| {
        stealth_kill_all_webhelpers();
    });

    // 2. Theme export + IPC server
    std::thread::spawn(|| {
        if let Err(e) = theme::export_theme_for_cef_hook() {
            log_to_temp(&format!("[steamcdp] Theme export failed: {}", e));
        }
        if let Err(e) = crate::ipc::start_ipc_server() {
            log_to_temp(&format!("[steamcdp] IPC server error: {}", e));
        }
    });

    // 3. Lua backends
    std::thread::spawn(|| {
        // TODO: Load plugins from platform::plugins_dir()
        // For now, try the existing plugin_loader if available
        log_to_temp("[steamcdp] Linux: plugin loading not yet implemented");
    });

    // 4. Bridge server
    std::thread::spawn(|| {
        crate::bridge::start_bridge_server();
    });

    // 5. Start CDP injection loop
    std::thread::spawn(|| {
        crate::hook_linux::start_cdp_injection_loop();
    });

    log_to_temp("[steamcdp] Linux initialization complete");
}
