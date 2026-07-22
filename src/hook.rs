use minhook::MinHook;
use std::ffi::c_void;
use std::mem;
use std::sync::OnceLock;
use windows_sys::Win32::System::Threading::{PROCESS_INFORMATION, STARTUPINFOW};

type FnCreateProcessW = unsafe extern "system" fn(
    *const u16,
    *mut u16,
    *const c_void,
    *const c_void,
    i32,
    u32,
    *const c_void,
    *const u16,
    *const STARTUPINFOW,
    *mut PROCESS_INFORMATION,
) -> i32;

pub(crate) static ORIGINAL_CREATE_PROCESS_W: OnceLock<FnCreateProcessW> = OnceLock::new();
pub(crate) static DEBUG_PORT: OnceLock<u16> = OnceLock::new();

const MAX_CMD_LINE_CHARS: usize = 32768;

unsafe fn read_utf16_bounded(ptr: *const u16, max_chars: usize) -> String {
    if ptr.is_null() {
        return String::new();
    }
    let mut len = 0usize;
    while len < max_chars && *ptr.add(len) != 0 {
        len += 1;
    }
    let slice = std::slice::from_raw_parts(ptr, len);
    String::from_utf16_lossy(slice)
}

unsafe extern "system" fn hook_create_process_w(
    lp_application_name: *const u16,
    lp_command_line: *mut u16,
    lp_process_attributes: *const c_void,
    lp_thread_attributes: *const c_void,
    b_inherit_handles: i32,
    dw_creation_flags: u32,
    lp_environment: *const c_void,
    lp_current_directory: *const u16,
    lp_startup_info: *const STARTUPINFOW,
    lp_process_information: *mut PROCESS_INFORMATION,
) -> i32 {
    let original_cmd = read_utf16_bounded(lp_command_line, MAX_CMD_LINE_CHARS);
    let lower = original_cmd.to_lowercase();

    let original = match ORIGINAL_CREATE_PROCESS_W.get() {
        Some(&f) => f,
        None => {
            crate::log_to_temp("[steamcdp] ERROR: trampoline not set, passthrough");
            let module_name: Vec<u16> = "kernel32.dll"
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            let proc_name = std::ffi::CString::new("CreateProcessW").unwrap();
            let m =
                windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(module_name.as_ptr());
            let p = windows_sys::Win32::System::LibraryLoader::GetProcAddress(
                m,
                proc_name.as_ptr() as *const u8,
            );
            match p {
                Some(f) => mem::transmute::<_, FnCreateProcessW>(f),
                None => return 0,
            }
        }
    };

    let new_cmd =
        if lower.contains("steamwebhelper.exe") && !lower.contains("--remote-debugging-port") {
            let port: u16 = match DEBUG_PORT.get() {
                Some(&p) => p,
                None => {
                    crate::log_to_temp("[steamcdp] ERROR: DEBUG_PORT not set");
                    return original(
                        lp_application_name,
                        lp_command_line,
                        lp_process_attributes,
                        lp_thread_attributes,
                        b_inherit_handles,
                        dw_creation_flags,
                        lp_environment,
                        lp_current_directory,
                        lp_startup_info,
                        lp_process_information,
                    );
                }
            };

            crate::log_to_temp(&format!(
                "[steamcdp] Injected debug port {} into steamwebhelper.exe",
                port
            ));

            Some(format!(
                "{} --remote-debugging-port={}",
                original_cmd.trim(),
                port
            ))
        } else {
            None
        };

    match new_cmd {
        Some(modified) => {
            let mut cmd_utf16: Vec<u16> =
                modified.encode_utf16().chain(std::iter::once(0)).collect();
            original(
                lp_application_name,
                cmd_utf16.as_mut_ptr(),
                lp_process_attributes,
                lp_thread_attributes,
                b_inherit_handles,
                dw_creation_flags,
                lp_environment,
                lp_current_directory,
                lp_startup_info,
                lp_process_information,
            )
        }
        None => original(
            lp_application_name,
            lp_command_line,
            lp_process_attributes,
            lp_thread_attributes,
            b_inherit_handles,
            dw_creation_flags,
            lp_environment,
            lp_current_directory,
            lp_startup_info,
            lp_process_information,
        ),
    }
}

pub fn install_hook() -> Result<(), String> {
    unsafe {
        let trampoline = MinHook::create_hook_api::<&str>(
            "kernel32.dll",
            "CreateProcessW",
            hook_create_process_w as *mut c_void,
        )
        .map_err(|e| format!("create_hook_api failed: {:?}", e))?;

        let original: FnCreateProcessW = mem::transmute(trampoline);
        ORIGINAL_CREATE_PROCESS_W
            .set(original)
            .map_err(|_| "OriginalCreateProcessW already initialized".to_string())?;

        MinHook::enable_all_hooks().map_err(|e| format!("enable_all_hooks failed: {:?}", e))?;
    }

    Ok(())
}

pub fn get_debug_port() -> Option<u16> {
    DEBUG_PORT.get().copied()
}
