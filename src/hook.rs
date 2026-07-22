use minhook::MinHook;
use rand::Rng;
use std::ffi::c_void;
use std::mem;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::OnceLock;
use windows_sys::Win32::System::Threading::{PROCESS_INFORMATION, STARTUPINFOW};

// --- Contadores para logs ---
static CREATE_PROCESS_W_CALL_COUNT: AtomicU32 = AtomicU32::new(0);
static CREATE_PROCESS_AS_USER_W_CALL_COUNT: AtomicU32 = AtomicU32::new(0);
static CREATE_PROCESS_A_CALL_COUNT: AtomicU32 = AtomicU32::new(0);
static CREATE_PROCESS_WITH_TOKEN_W_CALL_COUNT: AtomicU32 = AtomicU32::new(0);
static CREATE_PROCESS_WITH_LOGON_W_CALL_COUNT: AtomicU32 = AtomicU32::new(0);
static CREATE_PROCESS_INTERNAL_W_CALL_COUNT: AtomicU32 = AtomicU32::new(0);
static OUTPUT_DEBUG_STRING_W_CALL_COUNT: AtomicU32 = AtomicU32::new(0);

// --- Type definitions ---
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

type FnCreateProcessAsUserW = unsafe extern "system" fn(
    *mut c_void,
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

type FnCreateProcessA = unsafe extern "system" fn(
    *const u8,
    *mut u8,
    *const c_void,
    *const c_void,
    i32,
    u32,
    *const c_void,
    *const u8,
    *const STARTUPINFOW,
    *mut PROCESS_INFORMATION,
) -> i32;

type FnCreateProcessWithTokenW = unsafe extern "system" fn(
    *mut c_void,
    u32,
    *const u16,
    *mut u16,
    u32,
    *const c_void,
    *const u16,
    *const STARTUPINFOW,
    *mut PROCESS_INFORMATION,
) -> i32;

type FnCreateProcessWithLogonW = unsafe extern "system" fn(
    *const u16,
    *const u16,
    *const u16,
    u32,
    *const u16,
    *mut u16,
    u32,
    *const c_void,
    *const u16,
    *const STARTUPINFOW,
    *mut PROCESS_INFORMATION,
) -> i32;

// Firma para CreateProcessInternalW (kernelbase.dll)
type FnCreateProcessInternalW = unsafe extern "system" fn(
    *mut c_void, // HANDLE hToken (opcional)
    *const u16,  // LPCWSTR lpApplicationName
    *mut u16,    // LPWSTR lpCommandLine
    *const c_void, // LPSECURITY_ATTRIBUTES lpProcessAttributes
    *const c_void, // LPSECURITY_ATTRIBUTES lpThreadAttributes
    i32,         // BOOL bInheritHandles
    u32,         // DWORD dwCreationFlags
    *const c_void, // LPVOID lpEnvironment
    *const u16,  // LPCWSTR lpCurrentDirectory
    *const STARTUPINFOW,
    *mut PROCESS_INFORMATION,
    *mut u32,    // LPDWORD lpProcessInformation (extra)
) -> i32;

type FnOutputDebugStringW = unsafe extern "system" fn(*const u16);

// --- Almacenamiento de originales ---
pub(crate) static ORIGINAL_CREATE_PROCESS_W: OnceLock<FnCreateProcessW> = OnceLock::new();
pub(crate) static ORIGINAL_CREATE_PROCESS_AS_USER_W: OnceLock<FnCreateProcessAsUserW> = OnceLock::new();
pub(crate) static ORIGINAL_CREATE_PROCESS_A: OnceLock<FnCreateProcessA> = OnceLock::new();
pub(crate) static ORIGINAL_CREATE_PROCESS_WITH_TOKEN_W: OnceLock<FnCreateProcessWithTokenW> = OnceLock::new();
pub(crate) static ORIGINAL_CREATE_PROCESS_WITH_LOGON_W: OnceLock<FnCreateProcessWithLogonW> = OnceLock::new();
pub(crate) static ORIGINAL_CREATE_PROCESS_INTERNAL_W: OnceLock<FnCreateProcessInternalW> = OnceLock::new();
pub(crate) static ORIGINAL_OUTPUT_DEBUG_STRING_W: OnceLock<FnOutputDebugStringW> = OnceLock::new();

pub(crate) static DEBUG_PORT: OnceLock<u16> = OnceLock::new();

const MAX_CMD_LINE_CHARS: usize = 32768;

// --- Función auxiliar para modificar línea de comandos ---
pub(crate) fn build_webhelper_command_line(
    application_name: &str,
    command_line: &str,
    port: u16,
) -> Option<String> {
    let lower_app = application_name.to_lowercase();
    let lower_cmd = command_line.to_lowercase();

    let app_is_webhelper = lower_app.contains("steamwebhelper.exe");
    let cmd_is_webhelper = lower_cmd.contains("steamwebhelper.exe");

    if !app_is_webhelper && !cmd_is_webhelper {
        return None;
    }

    if lower_cmd.contains("--type=") {
        return None;
    }

    if lower_cmd.contains("--remote-debugging-port") {
        return None;
    }

    if command_line.trim().is_empty() && app_is_webhelper {
        return Some(format!(
            "\"{}\" --remote-debugging-port={}",
            application_name.trim(),
            port
        ));
    }

    Some(format!(
        "{} --remote-debugging-port={}",
        command_line.trim(),
        port
    ))
}

// --- Lectura segura de cadenas ---
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

unsafe fn read_ansi_bounded(ptr: *const u8, max_bytes: usize) -> String {
    if ptr.is_null() {
        return String::new();
    }
    let mut len = 0usize;
    while len < max_bytes && *ptr.add(len) != 0 {
        len += 1;
    }
    let slice = std::slice::from_raw_parts(ptr, len);
    String::from_utf8_lossy(slice).into_owned()
}

// --- Hooks ---

// Hook para CreateProcessW
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
    let call_num = CREATE_PROCESS_W_CALL_COUNT.fetch_add(1, Ordering::Relaxed) + 1;

    let app_name = read_utf16_bounded(lp_application_name, 512);
    let original_cmd = read_utf16_bounded(lp_command_line, MAX_CMD_LINE_CHARS);

    // Log incondicional para las primeras 20 llamadas
    if call_num <= 20 {
        crate::log_to_temp(&format!(
            "[steamcdp] CPW #{}: app=\"{}\" cmd=\"{}\"",
            call_num,
            &app_name[..app_name.len().min(200)],
            &original_cmd[..original_cmd.len().min(200)],
        ));
    }

    let original = match ORIGINAL_CREATE_PROCESS_W.get() {
        Some(&f) => f,
        None => {
            crate::log_to_temp("[steamcdp] ERROR: CPW trampoline not set, passthrough");
            return 0;
        }
    };

    let port: u16 = *DEBUG_PORT.get_or_init(crate::discovery::resolve_debug_port);
    let new_cmd = build_webhelper_command_line(&app_name, &original_cmd, port);

    if new_cmd.is_some() {
        if let Err(e) = crate::discovery::publish_if_needed(port) {
            crate::log_to_temp(&format!("[steamcdp] Failed to publish CDP discovery: {}", e));
        }
        crate::log_to_temp(&format!(
            "[steamcdp] Injected debug port {} into steamwebhelper.exe (CPW)",
            port
        ));
    }

    match new_cmd {
        Some(modified) => {
            let mut cmd_utf16: Vec<u16> = modified.encode_utf16().chain(Some(0)).collect();
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

// Hook para CreateProcessAsUserW
unsafe extern "system" fn hook_create_process_as_user_w(
    h_token: *mut c_void,
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
    let call_num = CREATE_PROCESS_AS_USER_W_CALL_COUNT.fetch_add(1, Ordering::Relaxed) + 1;

    let app_name = read_utf16_bounded(lp_application_name, 512);
    let original_cmd = read_utf16_bounded(lp_command_line, MAX_CMD_LINE_CHARS);

    if call_num <= 20 {
        crate::log_to_temp(&format!(
            "[steamcdp] CPAU #{}: app=\"{}\" cmd=\"{}\"",
            call_num,
            &app_name[..app_name.len().min(200)],
            &original_cmd[..original_cmd.len().min(200)],
        ));
    }

    let original = match ORIGINAL_CREATE_PROCESS_AS_USER_W.get() {
        Some(&f) => f,
        None => {
            crate::log_to_temp("[steamcdp] ERROR: CPAU trampoline not set, passthrough");
            return 0;
        }
    };

    let port: u16 = *DEBUG_PORT.get_or_init(crate::discovery::resolve_debug_port);
    let new_cmd = build_webhelper_command_line(&app_name, &original_cmd, port);

    if new_cmd.is_some() {
        if let Err(e) = crate::discovery::publish_if_needed(port) {
            crate::log_to_temp(&format!("[steamcdp] Failed to publish CDP discovery: {}", e));
        }
        crate::log_to_temp(&format!(
            "[steamcdp] Injected debug port {} into steamwebhelper.exe (CPAU)",
            port
        ));
    }

    match new_cmd {
        Some(modified) => {
            let mut cmd_utf16: Vec<u16> = modified.encode_utf16().chain(Some(0)).collect();
            original(
                h_token,
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
            h_token,
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

// Hook para CreateProcessA
unsafe extern "system" fn hook_create_process_a(
    lp_application_name: *const u8,
    lp_command_line: *mut u8,
    lp_process_attributes: *const c_void,
    lp_thread_attributes: *const c_void,
    b_inherit_handles: i32,
    dw_creation_flags: u32,
    lp_environment: *const c_void,
    lp_current_directory: *const u8,
    lp_startup_info: *const STARTUPINFOW,
    lp_process_information: *mut PROCESS_INFORMATION,
) -> i32 {
    let call_num = CREATE_PROCESS_A_CALL_COUNT.fetch_add(1, Ordering::Relaxed) + 1;

    let app_name = read_ansi_bounded(lp_application_name, 512);
    let original_cmd = read_ansi_bounded(lp_command_line, 32768);

    if call_num <= 20 || original_cmd.to_lowercase().contains("steamwebhelper") {
        crate::log_to_temp(&format!(
            "[steamcdp] CPA #{}: app=\"{}\" cmd=\"{}\" is_webhelper={}",
            call_num,
            &app_name[..app_name.len().min(200)],
            &original_cmd[..original_cmd.len().min(200)],
            original_cmd.to_lowercase().contains("steamwebhelper")
        ));
    }

    let original = match ORIGINAL_CREATE_PROCESS_A.get() {
        Some(&f) => f,
        None => {
            crate::log_to_temp("[steamcdp] ERROR: CPA trampoline not set, passthrough");
            return 0;
        }
    };

    let port: u16 = *DEBUG_PORT.get_or_init(crate::discovery::resolve_debug_port);
    let new_cmd = build_webhelper_command_line(&app_name, &original_cmd, port);

    if new_cmd.is_some() {
        if let Err(e) = crate::discovery::publish_if_needed(port) {
            crate::log_to_temp(&format!("[steamcdp] Failed to publish CDP discovery: {}", e));
        }
        crate::log_to_temp(&format!(
            "[steamcdp] Injected debug port {} into steamwebhelper.exe (CPA)",
            port
        ));
    }

    match new_cmd {
        Some(modified) => {
            let mut cmd_cstr: Vec<u8> = modified.into_bytes();
            cmd_cstr.push(0);
            original(
                lp_application_name,
                cmd_cstr.as_mut_ptr(),
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

// Hook para CreateProcessWithTokenW
unsafe extern "system" fn hook_create_process_with_token_w(
    h_token: *mut c_void,
    dw_logon_flags: u32,
    lp_application_name: *const u16,
    lp_command_line: *mut u16,
    dw_creation_flags: u32,
    lp_environment: *const c_void,
    lp_current_directory: *const u16,
    lp_startup_info: *const STARTUPINFOW,
    lp_process_information: *mut PROCESS_INFORMATION,
) -> i32 {
    let call_num = CREATE_PROCESS_WITH_TOKEN_W_CALL_COUNT.fetch_add(1, Ordering::Relaxed) + 1;

    let app_name = read_utf16_bounded(lp_application_name, 512);
    let original_cmd = read_utf16_bounded(lp_command_line, MAX_CMD_LINE_CHARS);

    if call_num <= 20 {
        crate::log_to_temp(&format!(
            "[steamcdp] CPTW #{}: app=\"{}\" cmd=\"{}\"",
            call_num,
            &app_name[..app_name.len().min(200)],
            &original_cmd[..original_cmd.len().min(200)],
        ));
    }

    let original = match ORIGINAL_CREATE_PROCESS_WITH_TOKEN_W.get() {
        Some(&f) => f,
        None => {
            crate::log_to_temp("[steamcdp] ERROR: CPTW trampoline not set, passthrough");
            return 0;
        }
    };

    let port: u16 = *DEBUG_PORT.get_or_init(crate::discovery::resolve_debug_port);
    let new_cmd = build_webhelper_command_line(&app_name, &original_cmd, port);

    if new_cmd.is_some() {
        if let Err(e) = crate::discovery::publish_if_needed(port) {
            crate::log_to_temp(&format!("[steamcdp] Failed to publish CDP discovery: {}", e));
        }
        crate::log_to_temp(&format!(
            "[steamcdp] Injected debug port {} into steamwebhelper.exe (CPTW)",
            port
        ));
    }

    match new_cmd {
        Some(modified) => {
            let mut cmd_utf16: Vec<u16> = modified.encode_utf16().chain(Some(0)).collect();
            original(
                h_token,
                dw_logon_flags,
                lp_application_name,
                cmd_utf16.as_mut_ptr(),
                dw_creation_flags,
                lp_environment,
                lp_current_directory,
                lp_startup_info,
                lp_process_information,
            )
        }
        None => original(
            h_token,
            dw_logon_flags,
            lp_application_name,
            lp_command_line,
            dw_creation_flags,
            lp_environment,
            lp_current_directory,
            lp_startup_info,
            lp_process_information,
        ),
    }
}

// Hook para CreateProcessWithLogonW
unsafe extern "system" fn hook_create_process_with_logon_w(
    lp_username: *const u16,
    lp_domain: *const u16,
    lp_password: *const u16,
    dw_logon_flags: u32,
    lp_application_name: *const u16,
    lp_command_line: *mut u16,
    dw_creation_flags: u32,
    lp_environment: *const c_void,
    lp_current_directory: *const u16,
    lp_startup_info: *const STARTUPINFOW,
    lp_process_information: *mut PROCESS_INFORMATION,
) -> i32 {
    let call_num = CREATE_PROCESS_WITH_LOGON_W_CALL_COUNT.fetch_add(1, Ordering::Relaxed) + 1;

    let app_name = read_utf16_bounded(lp_application_name, 512);
    let original_cmd = read_utf16_bounded(lp_command_line, MAX_CMD_LINE_CHARS);

    if call_num <= 20 {
        crate::log_to_temp(&format!(
            "[steamcdp] CPLW #{}: app=\"{}\" cmd=\"{}\"",
            call_num,
            &app_name[..app_name.len().min(200)],
            &original_cmd[..original_cmd.len().min(200)],
        ));
    }

    let original = match ORIGINAL_CREATE_PROCESS_WITH_LOGON_W.get() {
        Some(&f) => f,
        None => {
            crate::log_to_temp("[steamcdp] ERROR: CPLW trampoline not set, passthrough");
            return 0;
        }
    };

    let port: u16 = *DEBUG_PORT.get_or_init(crate::discovery::resolve_debug_port);
    let new_cmd = build_webhelper_command_line(&app_name, &original_cmd, port);

    if new_cmd.is_some() {
        if let Err(e) = crate::discovery::publish_if_needed(port) {
            crate::log_to_temp(&format!("[steamcdp] Failed to publish CDP discovery: {}", e));
        }
        crate::log_to_temp(&format!(
            "[steamcdp] Injected debug port {} into steamwebhelper.exe (CPLW)",
            port
        ));
    }

    match new_cmd {
        Some(modified) => {
            let mut cmd_utf16: Vec<u16> = modified.encode_utf16().chain(Some(0)).collect();
            original(
                lp_username,
                lp_domain,
                lp_password,
                dw_logon_flags,
                lp_application_name,
                cmd_utf16.as_mut_ptr(),
                dw_creation_flags,
                lp_environment,
                lp_current_directory,
                lp_startup_info,
                lp_process_information,
            )
        }
        None => original(
            lp_username,
            lp_domain,
            lp_password,
            dw_logon_flags,
            lp_application_name,
            lp_command_line,
            dw_creation_flags,
            lp_environment,
            lp_current_directory,
            lp_startup_info,
            lp_process_information,
        ),
    }
}

// Hook para CreateProcessInternalW (kernelbase.dll)
unsafe extern "system" fn hook_create_process_internal_w(
    h_token: *mut c_void,
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
    lp_process_information_extra: *mut u32,
) -> i32 {
    let call_num = CREATE_PROCESS_INTERNAL_W_CALL_COUNT.fetch_add(1, Ordering::Relaxed) + 1;

    let app_name = read_utf16_bounded(lp_application_name, 512);
    let original_cmd = read_utf16_bounded(lp_command_line, MAX_CMD_LINE_CHARS);

    // Log SIEMPRE si contiene "steamwebhelper"
    let lower_cmd = original_cmd.to_lowercase();
    let lower_app = app_name.to_lowercase();
    if lower_cmd.contains("steamwebhelper") || lower_app.contains("steamwebhelper") {
        crate::log_to_temp(&format!(
            "[steamcdp] CPIW #{} FOUND: app=\"{}\" cmd=\"{}\"",
            call_num, app_name, original_cmd
        ));
    }

    // Log primeras 20 como antes (para ver otras llamadas)
    if call_num <= 20 {
        crate::log_to_temp(&format!(
            "[steamcdp] CPIW #{}: app=\"{}\" cmd=\"{}\"",
            call_num,
            &app_name[..app_name.len().min(200)],
            &original_cmd[..original_cmd.len().min(200)],
        ));
    }

    let original = match ORIGINAL_CREATE_PROCESS_INTERNAL_W.get() {
        Some(&f) => f,
        None => {
            crate::log_to_temp("[steamcdp] ERROR: CPIW trampoline not set, passthrough");
            return 0;
        }
    };

    let port: u16 = *DEBUG_PORT.get_or_init(crate::discovery::resolve_debug_port);
    let new_cmd = build_webhelper_command_line(&app_name, &original_cmd, port);

    if new_cmd.is_some() {
        if let Err(e) = crate::discovery::publish_if_needed(port) {
            crate::log_to_temp(&format!("[steamcdp] Failed to publish CDP discovery: {}", e));
        }
        crate::log_to_temp(&format!(
            "[steamcdp] Injected debug port {} into steamwebhelper.exe (CPIW)",
            port
        ));
    }

    match new_cmd {
        Some(modified) => {
            let mut cmd_utf16: Vec<u16> = modified.encode_utf16().chain(Some(0)).collect();
            original(
                h_token,
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
                lp_process_information_extra,
            )
        }
        None => original(
            h_token,
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
            lp_process_information_extra,
        ),
    }
}

// Hook para OutputDebugStringW (opcional)
unsafe extern "system" fn hook_output_debug_string_w(lp_output_string: *const u16) {
    let count = OUTPUT_DEBUG_STRING_W_CALL_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
    if count == 1 || count % 10000 == 0 {
        let msg = read_utf16_bounded(lp_output_string, 200);
        crate::log_to_temp(&format!(
            "[steamcdp] ODSW heartbeat #{}: \"{}\"",
            count, &msg
        ));
    }
    let original = match ORIGINAL_OUTPUT_DEBUG_STRING_W.get() {
        Some(&f) => f,
        None => return,
    };
    original(lp_output_string)
}

// --- Instalación de hooks ---
pub fn install_hook() -> Result<(), String> {
    unsafe {
        // Crear los hooks (MinHook se inicializa automáticamente)

        // 1. CreateProcessW
        let trampoline_w = MinHook::create_hook_api::<&str>(
            "kernel32.dll",
            "CreateProcessW",
            hook_create_process_w as *mut c_void,
        )
        .map_err(|e| format!("create_hook_api(CreateProcessW) failed: {:?}", e))?;
        let original_w: FnCreateProcessW = mem::transmute(trampoline_w);
        ORIGINAL_CREATE_PROCESS_W
            .set(original_w)
            .map_err(|_| "OriginalCreateProcessW already initialized".to_string())?;

        // 2. CreateProcessAsUserW
        let trampoline_cau = MinHook::create_hook_api::<&str>(
            "advapi32.dll",
            "CreateProcessAsUserW",
            hook_create_process_as_user_w as *mut c_void,
        )
        .map_err(|e| format!("create_hook_api(CreateProcessAsUserW) failed: {:?}", e))?;
        let original_cau: FnCreateProcessAsUserW = mem::transmute(trampoline_cau);
        ORIGINAL_CREATE_PROCESS_AS_USER_W
            .set(original_cau)
            .map_err(|_| "OriginalCreateProcessAsUserW already initialized".to_string())?;

        // 3. CreateProcessA
        let trampoline_a = MinHook::create_hook_api::<&str>(
            "kernel32.dll",
            "CreateProcessA",
            hook_create_process_a as *mut c_void,
        )
        .map_err(|e| format!("create_hook_api(CreateProcessA) failed: {:?}", e))?;
        let original_a: FnCreateProcessA = mem::transmute(trampoline_a);
        ORIGINAL_CREATE_PROCESS_A
            .set(original_a)
            .map_err(|_| "OriginalCreateProcessA already initialized".to_string())?;

        // 4. CreateProcessWithTokenW
        let trampoline_cptw = MinHook::create_hook_api::<&str>(
            "advapi32.dll",
            "CreateProcessWithTokenW",
            hook_create_process_with_token_w as *mut c_void,
        )
        .map_err(|e| format!("create_hook_api(CreateProcessWithTokenW) failed: {:?}", e))?;
        let original_cptw: FnCreateProcessWithTokenW = mem::transmute(trampoline_cptw);
        ORIGINAL_CREATE_PROCESS_WITH_TOKEN_W
            .set(original_cptw)
            .map_err(|_| "OriginalCreateProcessWithTokenW already initialized".to_string())?;

        // 5. CreateProcessWithLogonW
        let trampoline_cplw = MinHook::create_hook_api::<&str>(
            "advapi32.dll",
            "CreateProcessWithLogonW",
            hook_create_process_with_logon_w as *mut c_void,
        )
        .map_err(|e| format!("create_hook_api(CreateProcessWithLogonW) failed: {:?}", e))?;
        let original_cplw: FnCreateProcessWithLogonW = mem::transmute(trampoline_cplw);
        ORIGINAL_CREATE_PROCESS_WITH_LOGON_W
            .set(original_cplw)
            .map_err(|_| "OriginalCreateProcessWithLogonW already initialized".to_string())?;

        // 6. CreateProcessInternalW (kernelbase.dll) - NUEVO
        let trampoline_cpiw = MinHook::create_hook_api::<&str>(
            "kernelbase.dll",
            "CreateProcessInternalW",
            hook_create_process_internal_w as *mut c_void,
        )
        .map_err(|e| format!("create_hook_api(CreateProcessInternalW) failed: {:?}", e))?;
        let original_cpiw: FnCreateProcessInternalW = mem::transmute(trampoline_cpiw);
        ORIGINAL_CREATE_PROCESS_INTERNAL_W
            .set(original_cpiw)
            .map_err(|_| "OriginalCreateProcessInternalW already initialized".to_string())?;

        // 7. OutputDebugStringW (opcional)
        let trampoline_odsw = MinHook::create_hook_api::<&str>(
            "kernel32.dll",
            "OutputDebugStringW",
            hook_output_debug_string_w as *mut c_void,
        )
        .map_err(|e| format!("create_hook_api(OutputDebugStringW) failed: {:?}", e))?;
        let original_odsw: FnOutputDebugStringW = mem::transmute(trampoline_odsw);
        ORIGINAL_OUTPUT_DEBUG_STRING_W
            .set(original_odsw)
            .map_err(|_| "OriginalOutputDebugStringW already initialized".to_string())?;

        // Habilitar todos los hooks
        MinHook::enable_all_hooks()
            .map_err(|e| format!("enable_all_hooks failed: {:?}", e))?;
    }

    Ok(())
}

// --- Función pública para obtener el puerto ---
pub fn get_debug_port() -> Option<u16> {
    DEBUG_PORT.get().copied()
}

// --- Pruebas unitarias ---
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primary_from_command_line() {
        let r = build_webhelper_command_line("", "steamwebhelper.exe --some-arg", 9222);
        assert_eq!(
            r,
            Some("steamwebhelper.exe --some-arg --remote-debugging-port=9222".into())
        );
    }

    #[test]
    fn primary_from_application_name() {
        let r =
            build_webhelper_command_line("C:\\Program Files\\Steam\\steamwebhelper.exe", "", 9222);
        assert_eq!(
            r,
            Some(
                "\"C:\\Program Files\\Steam\\steamwebhelper.exe\" --remote-debugging-port=9222"
                    .into()
            )
        );
    }

    #[test]
    fn reject_type_flag() {
        let r = build_webhelper_command_line(
            "",
            "steamwebhelper.exe --type=renderer --some-arg",
            9222,
        );
        assert_eq!(r, None);
    }

    #[test]
    fn do_not_duplicate_debug_port() {
        let r = build_webhelper_command_line(
            "",
            "steamwebhelper.exe --remote-debugging-port=9222",
            9222,
        );
        assert_eq!(r, None);
    }
}