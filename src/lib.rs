mod cdp;
mod discovery;
mod hook;

use std::ffi::c_void;
use windows_sys::Win32::System::LibraryLoader::GetModuleFileNameW;
use windows_sys::Win32::System::Threading::GetCurrentProcessId;

const DLL_PROCESS_ATTACH: u32 = 1;
const DLL_PROCESS_DETACH: u32 = 0;

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
