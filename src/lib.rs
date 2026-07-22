mod cdp;
mod discovery;
mod hook;

use std::ffi::c_void;
use windows_sys::Win32::System::Environment::GetEnvironmentVariableW;
use windows_sys::Win32::System::Threading::CreateThread;

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
            CreateThread(
                std::ptr::null(),
                0,
                Some(proxy_thread),
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
            );
        }
        DLL_PROCESS_DETACH => {}
        _ => {}
    }
    1
}

unsafe extern "system" fn proxy_thread(_param: *mut c_void) -> u32 {
    let port = discovery::resolve_debug_port();
    let _ = hook::DEBUG_PORT.set(port);
    discovery::publish_port(port);

    match hook::install_hook() {
        Ok(()) => {
            log_to_temp(&format!(
                "[steamcdp] CreateProcessW hook installed, CDP port: {}",
                port
            ));
        }
        Err(e) => {
            log_to_temp(&format!("[steamcdp] Failed to install hook: {}", e));
        }
    }

    // TODO: Initialize CDP connection (cdp.rs)

    0
}

pub(crate) fn log_to_temp(msg: &str) {
    use std::fs::OpenOptions;
    use std::io::Write;

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
