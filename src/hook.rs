use minhook::MinHook;
use std::ffi::c_void;
use std::mem;
use std::sync::Once;
use std::sync::OnceLock;
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE};
use windows_sys::Win32::System::Diagnostics::Debug::WriteProcessMemory;
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
use windows_sys::Win32::System::Memory::{
    VirtualAllocEx, VirtualFreeEx, MEM_COMMIT, MEM_RELEASE, PAGE_READWRITE,
};
use windows_sys::Win32::System::Threading::{
    CreateRemoteThread, OpenProcess, WaitForSingleObject, PROCESS_CREATE_THREAD,
    PROCESS_INFORMATION, PROCESS_QUERY_INFORMATION, PROCESS_VM_OPERATION, PROCESS_VM_READ,
    PROCESS_VM_WRITE, STARTUPINFOW,
};

// --- Type definitions (solo CreateProcessW) ---
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

// --- Almacenamiento de original (solo CreateProcessW) ---
pub(crate) static ORIGINAL_CREATE_PROCESS_W: OnceLock<FnCreateProcessW> = OnceLock::new();

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

// --- Hook para CreateProcessW ---
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
    let app_name = read_utf16_bounded(lp_application_name, 512);
    let original_cmd = read_utf16_bounded(lp_command_line, MAX_CMD_LINE_CHARS);

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
            crate::log_to_temp(&format!(
                "[steamcdp] Failed to publish CDP discovery: {}",
                e
            ));
        }
        crate::log_to_temp(&format!(
            "[steamcdp] Injected debug port {} into steamwebhelper.exe (CPW)",
            port
        ));
    }

    let is_webhelper = new_cmd.is_some();

    // Iniciar el thread de CDP solo una vez
    static INJECTION_STARTED: Once = Once::new();
    if is_webhelper {
        INJECTION_STARTED.call_once(|| {
            let port = port;
            std::thread::spawn(move || {
                start_cdp_watch_loop(port);
            });
        });
    }

    let result = match new_cmd {
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
    };

    if result != 0 && is_webhelper {
        let desired_access = PROCESS_CREATE_THREAD
            | PROCESS_VM_OPERATION
            | PROCESS_VM_READ
            | PROCESS_VM_WRITE
            | PROCESS_QUERY_INFORMATION;
        let inject_handle = OpenProcess(desired_access, 0, (*lp_process_information).dwProcessId);

        if !inject_handle.is_null() {
            if let Some(dll_path) = get_cef_hook_dll_path() {
                inject_dll_into_process(inject_handle, &dll_path);
            }
            CloseHandle(inject_handle);
        }
    }

    result
}

// --- CDP watch loop (parity with hook_linux.rs) ---

/// Drain the bridge proxy queue from all injected targets and fulfill
/// pending HTTP requests from Rust (bypasses mixed-content blocking).
fn drain_bridge_queue(
    client: &mut crate::cdp::CdpClient,
    injected: &std::collections::HashSet<String>,
) {
    for target_id in injected {
        if target_id.is_empty() {
            continue;
        }
        if client.attach_to_target(target_id).is_err() {
            continue;
        }
        let drain_expr = r#"(function(){
            if (!window.__lumaBridgeDrain) return '[]';
            return window.__lumaBridgeDrain();
        })()"#;
        let resp = match client.send_cdp_wait(
            &serde_json::json!({
                "id": 7700,
                "method": "Runtime.evaluate",
                "params": { "expression": drain_expr, "returnByValue": true }
            }),
            7700,
        ) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let queue_str = resp
            .get("result")
            .and_then(|r| r.get("result"))
            .and_then(|r| r.get("value"))
            .and_then(|v| v.as_str())
            .unwrap_or("[]");
        let queue: Vec<serde_json::Value> = serde_json::from_str(queue_str).unwrap_or_default();
        if queue.is_empty() {
            continue;
        }

        for req in &queue {
            let id = req.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let url = req.get("url").and_then(|v| v.as_str()).unwrap_or("");
            let method = req.get("method").and_then(|v| v.as_str()).unwrap_or("GET");
            let body = req.get("body").and_then(|v| v.as_str());

            let result = make_bridge_request(method, url, body);
            let inject_expr = format!(
                "window.__lumaBridgeResults['{}'] = {};",
                id.replace('\'', "\\'"),
                serde_json::to_string(&result).unwrap_or_default()
            );
            let _ = client.send_cdp_wait(
                &serde_json::json!({
                    "id": 7701,
                    "method": "Runtime.evaluate",
                    "params": { "expression": &inject_expr, "returnByValue": true }
                }),
                7701,
            );
        }
    }
}

/// Make an HTTP request to the local bridge (called from Rust, not CEF).
/// Tries the proxy's own ports only — no luma-lite dependency.
fn make_bridge_request(method: &str, url: &str, body: Option<&str>) -> serde_json::Value {
    let client = match reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return serde_json::json!({"status": 0, "body": "", "headers": {}, "statusText": format!("Client build error: {}", e)});
        }
    };

    // Our own bridge ports: primary 21775, fallback 21776
    let ports = [21775u16, 21776];
    for port in &ports {
        let target_url = if url.contains("127.0.0.1:") || url.contains("localhost:") {
            let host_part = if url.contains("127.0.0.1:") {
                "127.0.0.1:"
            } else {
                "localhost:"
            };
            let prefix = url.split(host_part).next().unwrap_or("");
            let suffix = url.splitn(2, host_part).nth(1).unwrap_or("");
            let path = suffix.splitn(2, '/').nth(1).unwrap_or("");
            if path.is_empty() {
                format!("{}127.0.0.1:{}/", prefix, port)
            } else {
                format!("{}127.0.0.1:{}/{}", prefix, port, path)
            }
        } else {
            url.to_string()
        };

        let http_method = reqwest::Method::from_bytes(method.as_bytes()).unwrap_or(reqwest::Method::GET);
        let mut req = client.request(http_method, &target_url);
        if let Some(b) = body {
            req = req.body(b.to_string()).header("content-type", "application/json");
        }

        match req.send() {
            Ok(resp) => {
                let status = resp.status().as_u16();
                let headers: serde_json::Map<String, serde_json::Value> = resp
                    .headers()
                    .iter()
                    .map(|(k, v)| {
                        (
                            k.as_str().to_string(),
                            serde_json::Value::String(v.to_str().unwrap_or("").to_string()),
                        )
                    })
                    .collect();
                let body = resp.text().unwrap_or_default();
                return serde_json::json!({
                    "status": status,
                    "body": body,
                    "headers": headers,
                    "statusText": ""
                });
            }
            Err(_) => continue,
        }
    }

    serde_json::json!({"status": 0, "body": "", "headers": {}, "statusText": "Bridge not available on any port"})
}

/// Main CDP watch loop for Windows: connect with exponential backoff,
/// inject, drain bridge queue, re-inject on URL changes, reconnect on drop.
fn start_cdp_watch_loop(port: u16) {
    if let Err(e) = crate::discovery::publish_if_needed(port) {
        crate::log_to_temp(&format!(
            "[steamcdp] Failed to publish CDP discovery: {}",
            e
        ));
    }

    crate::log_to_temp(&format!(
        "[steamcdp] Windows CDP injection loop started, port={}",
        port
    ));

    // Exponential backoff: 100ms → 200ms → 400ms → 500ms
    let max_attempts = 30;
    for attempt in 1..=max_attempts {
        let delay_ms = std::cmp::min(100 * (1u64 << ((attempt - 1).min(2))), 500);
        std::thread::sleep(std::time::Duration::from_millis(delay_ms));

        match crate::cdp::CdpClient::connect(port) {
            Ok(mut client) => {
                crate::log_to_temp(&format!(
                    "[steamcdp] Connected to CDP (attempt {})",
                    attempt
                ));
                match crate::injector::inject_all(&mut client) {
                    Ok(()) => {
                        crate::log_to_temp("[steamcdp] Injection complete");
                    }
                    Err(e) => {
                        crate::log_to_temp(&format!("[steamcdp] Injection error: {}", e));
                    }
                }

                // Mark existing page targets as seen (inject_all already filtered
                // to real Steam pages). Skip shutdown/internal pages entirely.
                let mut injected_targets: std::collections::HashSet<String> =
                    std::collections::HashSet::new();
                if let Ok(targets) = client.get_targets() {
                    for t in &targets {
                        if t.target_type == "page"
                            && t.title != "Shutdown"
                            && !t.url.contains("createflags=2")
                            && !t.url.contains("centerOnBrowserID")
                        {
                            injected_targets.insert(t.id.clone());
                        }
                    }
                }

                crate::log_to_temp("[steamcdp] Watching for new targets...");
                let (theme_dir, theme_patches) = crate::injector::load_theme_patches();
                let mut recheck_counter = 0u32;
                let mut known_urls: std::collections::HashMap<String, String> =
                    std::collections::HashMap::new();
                if let Ok(targets) = client.get_targets() {
                    for t in &targets {
                        if t.target_type == "page" && injected_targets.contains(&t.id) {
                            known_urls.insert(t.id.clone(), t.url.clone());
                        }
                    }
                }

                loop {
                    std::thread::sleep(std::time::Duration::from_secs(1));
                    recheck_counter += 1;

                    // Drain bridge proxy queue every cycle (~1s) so extension fetch
                    // requests are fulfilled within the 15s JS timeout
                    drain_bridge_queue(&mut client, &injected_targets);

                    // Every 30s, re-evaluate diagnostic on ALL injected targets to
                    // catch SPA navigations. Store pages are checked first.
                    if recheck_counter % 30 == 0 {
                        if let Ok(all_targets) = client.get_targets() {
                            let page_count = all_targets
                                .iter()
                                .filter(|t| t.target_type == "page")
                                .count();
                            crate::log_to_temp(&format!(
                                "[steamcdp] Recheck: {} total targets ({} pages), {} injected",
                                all_targets.len(),
                                page_count,
                                injected_targets.len()
                            ));

                            let mut pages: Vec<_> = all_targets
                                .iter()
                                .filter(|t| {
                                    t.target_type == "page"
                                        && injected_targets.contains(&t.id)
                                        && crate::injector::is_real_steam_page(&t.url)
                                })
                                .collect();
                            pages.sort_by(|a, b| {
                                let a_store = a.url.contains("store.steampowered.com");
                                let b_store = b.url.contains("store.steampowered.com");
                                b_store.cmp(&a_store)
                            });

                            let mut checked = 0u32;
                            for t in pages {
                                checked += 1;
                                if client.attach_to_target(&t.id).is_err() {
                                    continue;
                                }
                                let diag_expr = r#"JSON.stringify({url:window.location.href.substring(0,200),title:document.title.substring(0,80),bodyLen:document.body?document.body.innerHTML.length:-1,hasLuma:!!window.__lumaforge_ssh__,lumaActive:window.__lumaforge_ssh__&&window.__lumaforge_ssh__.active,lumaAppId:window.__lumaforge_ssh__&&window.__lumaforge_ssh__.currentAppId,btnExists:!!document.getElementById('luma-action-btn'),appLinks:document.querySelectorAll('a[href*="/app/"]').length})"#;
                                let msg_id = 9000 + checked as u64;
                                let mut diag_val = String::new();
                                if let Ok(resp) = client.send_cdp_wait(
                                    &serde_json::json!({
                                        "id": msg_id,
                                        "method": "Runtime.evaluate",
                                        "params": { "expression": diag_expr, "returnByValue": true }
                                    }),
                                    msg_id,
                                ) {
                                    if let Some(v) = resp
                                        .get("result")
                                        .and_then(|r| r.get("result"))
                                        .and_then(|r| r.get("value"))
                                        .and_then(|v| v.as_str())
                                    {
                                        diag_val = v.to_string();
                                        crate::log_to_temp(&format!(
                                            "[steamcdp] Recheck p#{}: {}",
                                            checked, v
                                        ));
                                    }
                                } else {
                                    crate::log_to_temp(&format!(
                                        "[steamcdp] Recheck p#{}: CDP eval failed",
                                        checked
                                    ));
                                }

                                let has_luma = diag_val.contains("\"hasLuma\":true");
                                if !has_luma {
                                    crate::log_to_temp(&format!(
                                        "[steamcdp] Re-injecting target (hasLuma=false): id={}, title=\"{}\", url={}",
                                        t.id,
                                        t.title,
                                        &t.url[..t.url.len().min(100)]
                                    ));
                                    let plugins = crate::plugin_loader::load_enabled_plugins()
                                        .unwrap_or_default();
                                    if let Err(e) = crate::injector::inject_into_target(
                                        &mut client,
                                        t,
                                        &plugins,
                                        &theme_dir,
                                        &theme_patches,
                                        checked as usize,
                                    ) {
                                        crate::log_to_temp(&format!(
                                            "[steamcdp] Re-inject failed: {}",
                                            e
                                        ));
                                    }
                                }
                            }
                        }
                    }

                    match client.get_targets() {
                        Ok(new_targets) => {
                            for t in &new_targets {
                                if t.target_type == "page"
                                    && !injected_targets.contains(&t.id)
                                    && crate::injector::is_real_steam_page(&t.url)
                                {
                                    // Skip shutdown/close targets — injecting into these
                                    // can destabilize Steam
                                    if t.title == "Shutdown" || t.url.contains("createflags=2") {
                                        injected_targets.insert(t.id.clone());
                                        continue;
                                    }
                                    crate::log_to_temp(&format!(
                                        "[steamcdp] New target: id={}, title=\"{}\", url={}",
                                        t.id,
                                        t.title,
                                        &t.url[..t.url.len().min(100)]
                                    ));
                                    let plugins = crate::plugin_loader::load_enabled_plugins()
                                        .unwrap_or_default();
                                    if let Err(e) = crate::injector::inject_into_target(
                                        &mut client,
                                        t,
                                        &plugins,
                                        &theme_dir,
                                        &theme_patches,
                                        injected_targets.len() + 1,
                                    ) {
                                        crate::log_to_temp(&format!(
                                            "[steamcdp] New target injection failed: {}",
                                            e
                                        ));
                                    } else {
                                        injected_targets.insert(t.id.clone());
                                        known_urls.insert(t.id.clone(), t.url.clone());
                                    }
                                } else if t.target_type == "page"
                                    && injected_targets.contains(&t.id)
                                {
                                    // Detect URL change on existing store target → re-inject
                                    if t.url.contains("store.steampowered.com") {
                                        if let Some(prev_url) = known_urls.get(&t.id) {
                                            if prev_url != &t.url {
                                                crate::log_to_temp(&format!(
                                                    "[steamcdp] Store URL changed: id={}, prev={}, new={}",
                                                    t.id,
                                                    &prev_url[..prev_url.len().min(80)],
                                                    &t.url[..t.url.len().min(80)]
                                                ));
                                                let plugins =
                                                    crate::plugin_loader::load_enabled_plugins()
                                                        .unwrap_or_default();
                                                if let Err(e) = crate::injector::inject_into_target(
                                                    &mut client,
                                                    t,
                                                    &plugins,
                                                    &theme_dir,
                                                    &theme_patches,
                                                    1,
                                                ) {
                                                    crate::log_to_temp(&format!(
                                                        "[steamcdp] URL-change re-inject failed: {}",
                                                        e
                                                    ));
                                                }
                                            }
                                        }
                                        known_urls.insert(t.id.clone(), t.url.clone());
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            crate::log_to_temp(&format!(
                                "[steamcdp] get_targets error: {}, trying reconnect...",
                                e
                            ));
                            match crate::cdp::CdpClient::connect(port) {
                                Ok(new_client) => {
                                    crate::log_to_temp("[steamcdp] Reconnected to CDP");
                                    client = new_client;
                                    injected_targets.clear();
                                    known_urls.clear();
                                    if let Ok(targets) = client.get_targets() {
                                        for t in &targets {
                                            if t.target_type == "page" {
                                                injected_targets.insert(t.id.clone());
                                                known_urls.insert(t.id.clone(), t.url.clone());
                                            }
                                        }
                                    }
                                }
                                Err(e2) => {
                                    crate::log_to_temp(&format!(
                                        "[steamcdp] Reconnect failed: {}",
                                        e2
                                    ));
                                }
                            }
                        }
                    }
                }
                // Unreachable: the watch loop only exits via reconnect path above.
                #[allow(unreachable_code)]
                {
                    crate::log_to_temp("[steamcdp] CDP connection lost, reconnecting...");
                }
            }
            Err(e) => {
                crate::log_to_temp(&format!(
                    "[steamcdp] CDP connect attempt {} failed: {}",
                    attempt, e
                ));
            }
        }
    }
    crate::log_to_temp("[steamcdp] Max attempts reached, giving up");
}

// --- Instalación de hooks (solo CreateProcessW) ---
pub fn install_hook() -> Result<(), String> {
    unsafe {
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

        MinHook::enable_all_hooks().map_err(|e| format!("enable_all_hooks failed: {:?}", e))?;
    }

    Ok(())
}

// --- Inyección de DLL en proceso webhelper ---
unsafe fn inject_dll_into_process(process_handle: HANDLE, dll_path: &str) -> bool {
    let dll_path_wide: Vec<u16> = dll_path.encode_utf16().chain(Some(0)).collect();
    let size = dll_path_wide.len() * 2;

    let remote_mem = VirtualAllocEx(
        process_handle,
        std::ptr::null_mut(),
        size,
        MEM_COMMIT,
        PAGE_READWRITE,
    );

    if remote_mem.is_null() {
        crate::log_to_temp(&format!(
            "[steamcdp] Failed to allocate memory in webhelper process: {}",
            GetLastError()
        ));
        return false;
    }

    if WriteProcessMemory(
        process_handle,
        remote_mem,
        dll_path_wide.as_ptr() as *const c_void,
        size,
        std::ptr::null_mut(),
    ) == 0
    {
        crate::log_to_temp(&format!(
            "[steamcdp] Failed to write DLL path to webhelper process: {}",
            GetLastError()
        ));
        VirtualFreeEx(process_handle, remote_mem, 0, MEM_RELEASE);
        return false;
    }

    let kernel32 = GetModuleHandleA(b"kernel32.dll\0".as_ptr());
    if kernel32.is_null() {
        crate::log_to_temp("[steamcdp] Failed to get kernel32.dll handle");
        VirtualFreeEx(process_handle, remote_mem, 0, MEM_RELEASE);
        return false;
    }

    let load_library_w = GetProcAddress(kernel32, b"LoadLibraryW\0".as_ptr());
    let load_library_w_fn = match load_library_w {
        Some(f) => f,
        None => {
            crate::log_to_temp("[steamcdp] Failed to get LoadLibraryW address");
            VirtualFreeEx(process_handle, remote_mem, 0, MEM_RELEASE);
            return false;
        }
    };

    let mut thread_id = 0u32;
    let thread_handle = CreateRemoteThread(
        process_handle,
        std::ptr::null(),
        0,
        Some(mem::transmute(load_library_w_fn)),
        remote_mem,
        0,
        &mut thread_id,
    );

    if thread_handle.is_null() {
        crate::log_to_temp(&format!(
            "[steamcdp] Failed to create remote thread in webhelper: {}",
            GetLastError()
        ));
        VirtualFreeEx(process_handle, remote_mem, 0, MEM_RELEASE);
        return false;
    }

    WaitForSingleObject(thread_handle, 5000);
    CloseHandle(thread_handle);
    VirtualFreeEx(process_handle, remote_mem, 0, MEM_RELEASE);

    crate::log_to_temp(&format!(
        "[steamcdp] Successfully injected DLL into webhelper: {}",
        dll_path
    ));
    true
}

fn get_cef_hook_dll_path() -> Option<String> {
    let exe_path = std::env::current_exe().ok()?;
    let exe_dir = exe_path.parent()?;

    // Primero buscar en subfolder lumaforge/ (DLL organizada)
    let dll_path = exe_dir.join("lumaforge").join("lumaforge_cef_hook.dll");
    if dll_path.exists() {
        return Some(dll_path.to_string_lossy().to_string());
    }

    // Fallback: root de Steam (legacy)
    let dll_path = exe_dir.join("lumaforge_cef_hook.dll");
    if dll_path.exists() {
        return Some(dll_path.to_string_lossy().to_string());
    }

    None
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
        let r =
            build_webhelper_command_line("", "steamwebhelper.exe --type=renderer --some-arg", 9222);
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
