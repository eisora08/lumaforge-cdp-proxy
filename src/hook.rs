use minhook::MinHook;
use std::ffi::c_void;
use std::mem;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::sync::Once;
use std::sync::OnceLock;
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, SetHandleInformation, HANDLE, HANDLE_FLAG_INHERIT,
};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::{ReadFile, WriteFile};
use windows_sys::Win32::System::Diagnostics::Debug::WriteProcessMemory;
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
use windows_sys::Win32::System::Memory::{
    VirtualAllocEx, VirtualFreeEx, MEM_COMMIT, MEM_RELEASE, PAGE_READWRITE,
};
use windows_sys::Win32::System::Pipes::{CreatePipe, PeekNamedPipe};
use windows_sys::Win32::System::Threading::{
    CreateRemoteThread, DeleteProcThreadAttributeList, GetExitCodeProcess,
    InitializeProcThreadAttributeList, OpenProcess, UpdateProcThreadAttribute, WaitForSingleObject,
    EXTENDED_STARTUPINFO_PRESENT, LPPROC_THREAD_ATTRIBUTE_LIST, PROCESS_CREATE_THREAD,
    PROCESS_INFORMATION, PROCESS_QUERY_INFORMATION, PROCESS_VM_OPERATION, PROCESS_VM_READ,
    PROCESS_VM_WRITE, PROC_THREAD_ATTRIBUTE_HANDLE_LIST, STARTUPINFOEXW, STARTUPINFOW,
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

// --- FunciÃ³n auxiliar para modificar lÃ­nea de comandos ---
pub(crate) fn build_webhelper_command_line(
    application_name: &str,
    command_line: &str,
    port: Option<u16>,
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

    // Parity with hook_linux.rs: recent CEF rejects DevTools websocket
    // handshakes from browser contexts whose Origin is not allowed.
    let allow_origins = if lower_cmd.contains("--remote-allow-origins") {
        ""
    } else {
        " --remote-allow-origins=*"
    };

    // `port: None` (TCP CDP off) builds the base line without the debugging
    // port so pipe flags can still be appended to it.
    let port_flag = match port {
        Some(p) => format!(" --remote-debugging-port={}", p),
        None => String::new(),
    };

    if command_line.trim().is_empty() && app_is_webhelper {
        return Some(format!(
            "\"{}\"{}{}",
            application_name.trim(),
            port_flag,
            allow_origins
        ));
    }

    Some(format!(
        "{}{}{}",
        command_line.trim(),
        port_flag,
        allow_origins
    ))
}

/// Extract a `--remote-debugging-port=N` already present in the command line
/// (Steam's `.cef-enable-debugging` file or an explicit launch flag). Returns
/// None when the flag is absent or the value is not a usable port.
pub(crate) fn extract_existing_debug_port(command_line: &str) -> Option<u16> {
    let lower = command_line.to_ascii_lowercase();
    let pos = lower.find("--remote-debugging-port=")?;
    let rest = &command_line[pos + "--remote-debugging-port=".len()..];
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    crate::discovery::parse_configured_port(Some(&rest[..end]))
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

// --- CDP pipes: transporte DevTools sobre handles heredados ---
// Chromium acepta `--remote-debugging-pipe` + `--remote-debugging-io-pipes`
// y habla CDP por extremos de pipe heredados en vez de bindear un puerto TCP.

#[derive(Clone, Copy)]
pub(crate) struct CdpPipePair {
    pub parent_read: HANDLE,
    pub parent_write: HANDLE,
    pub child_pid: u32,
    pub generation: u64,
}

unsafe impl Send for CdpPipePair {}

static CDP_PIPE_PAIRS: OnceLock<Mutex<Vec<CdpPipePair>>> = OnceLock::new();
static NEXT_CDP_PIPE_GEN: AtomicU64 = AtomicU64::new(0);

#[allow(dead_code)]
pub(crate) fn cdp_pipe_pairs() -> Vec<CdpPipePair> {
    CDP_PIPE_PAIRS
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .map(|pairs| pairs.clone())
        .unwrap_or_default()
}

/// Webhelper pids spawned through our hook this session (with or without CDP
/// pipes). The stale-kill thread treats these — and their descendants — as
/// fresh and never touches them.
static SESSION_PIDS: OnceLock<Mutex<Vec<u32>>> = OnceLock::new();

pub(crate) fn note_spawned_webhelper(pid: u32) {
    let pids = SESSION_PIDS.get_or_init(|| Mutex::new(Vec::new()));
    if let Ok(mut guard) = pids.lock() {
        if !guard.contains(&pid) {
            guard.push(pid);
        }
    }
}

pub(crate) fn session_webhelper_pids() -> Vec<u32> {
    let mut pids: Vec<u32> = SESSION_PIDS
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .map(|guard| guard.clone())
        .unwrap_or_default();
    for pair in cdp_pipe_pairs() {
        if !pids.contains(&pair.child_pid) {
            pids.push(pair.child_pid);
        }
    }
    pids
}

pub(crate) fn is_session_webhelper(pid: u32) -> bool {
    if let Some(pids) = SESSION_PIDS.get() {
        if let Ok(guard) = pids.lock() {
            if guard.contains(&pid) {
                return true;
            }
        }
    }
    cdp_pipe_pairs().iter().any(|p| p.child_pid == pid)
}

fn register_cdp_pipe_pair(pair: CdpPipePair) {
    let pairs = CDP_PIPE_PAIRS.get_or_init(|| Mutex::new(Vec::new()));
    if let Ok(mut guard) = pairs.lock() {
        guard.push(pair);
    }
}

/// Drop superseded pipe pairs whose child process is gone: a respawn
/// registers a newer generation and probing dead handles every round is
/// pure noise. Pairs from the current (or a newer) generation are kept
/// unconditionally.
fn purge_dead_pairs_older_than(gen: u64) {
    let pairs = CDP_PIPE_PAIRS.get_or_init(|| Mutex::new(Vec::new()));
    if let Ok(mut guard) = pairs.lock() {
        guard.retain(|p| p.generation >= gen || pid_is_alive(p.child_pid));
    }
}

fn pid_is_alive(pid: u32) -> bool {
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_INFORMATION, 0, pid);
        if handle.is_null() {
            return false;
        }
        let mut code = 0u32;
        let ok = GetExitCodeProcess(handle, &mut code);
        CloseHandle(handle);
        ok != 0 && code == 259 // STILL_ACTIVE
    }
}

fn env_flag(name: &str) -> bool {
    match std::env::var(name) {
        Ok(v) => !v.is_empty() && v != "0",
        Err(_) => false,
    }
}

fn cdp_pipes_disabled() -> bool {
    env_flag("LUMAFORGE_NO_CDP_PIPES")
}

/// Master switch for the injected-TCP CDP path (`--remote-debugging-port`).
/// Default off while the pipe-only path is being validated: set
/// LUMAFORGE_CDP_TCP=1/0 to force either way, or flip DEFAULT once pipes
/// are proven. Steam's own adopted port is handled separately (always on).
fn cdp_tcp_enabled() -> bool {
    const DEFAULT: bool = false;
    match std::env::var("LUMAFORGE_CDP_TCP") {
        Ok(v) => !v.is_empty() && v != "0",
        Err(_) => DEFAULT,
    }
}

fn cdp_pipe_probe_enabled() -> bool {
    env_flag("LUMAFORGE_PIPE_PROBE")
}

fn pipe_flag_args(child_read: usize, child_write: usize) -> String {
    format!(
        "--remote-debugging-pipe --remote-debugging-io-pipes={},{}",
        child_read, child_write
    )
}

fn append_pipe_flags(command_line: &mut String, child_read: HANDLE, child_write: HANDLE) {
    if command_line
        .to_ascii_lowercase()
        .contains("--remote-debugging-pipe")
    {
        return;
    }
    command_line.push(' ');
    command_line.push_str(&pipe_flag_args(child_read as usize, child_write as usize));
}

struct PipeSpawn {
    child_read: HANDLE,
    child_write: HANDLE,
    parent_read: HANDLE,
    parent_write: HANDLE,
    generation: u64,
}

impl PipeSpawn {
    fn close_child_ends(&mut self) {
        unsafe {
            if !self.child_read.is_null() {
                CloseHandle(self.child_read);
                self.child_read = std::ptr::null_mut();
            }
            if !self.child_write.is_null() {
                CloseHandle(self.child_write);
                self.child_write = std::ptr::null_mut();
            }
        }
    }

    fn into_pair(mut self, child_pid: u32) -> CdpPipePair {
        self.close_child_ends();
        let pair = CdpPipePair {
            parent_read: self.parent_read,
            parent_write: self.parent_write,
            child_pid,
            generation: self.generation,
        };
        self.parent_read = std::ptr::null_mut();
        self.parent_write = std::ptr::null_mut();
        pair
    }
}

impl Drop for PipeSpawn {
    fn drop(&mut self) {
        unsafe {
            for handle in [
                self.child_read,
                self.child_write,
                self.parent_read,
                self.parent_write,
            ] {
                if !handle.is_null() {
                    CloseHandle(handle);
                }
            }
        }
    }
}

fn create_cdp_pipe_spawn() -> Result<PipeSpawn, String> {
    unsafe {
        let sa = SECURITY_ATTRIBUTES {
            nLength: mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: std::ptr::null_mut(),
            bInheritHandle: 1,
        };

        // Pipe de comandos: el padre escribe, el webhelper (Chromium) lee.
        let mut child_read: HANDLE = std::ptr::null_mut();
        let mut parent_write: HANDLE = std::ptr::null_mut();
        if CreatePipe(&mut child_read, &mut parent_write, &sa, 0) == 0 {
            return Err(format!("CreatePipe(command) failed: {}", GetLastError()));
        }

        // Pipe de respuestas: el webhelper escribe, el padre lee.
        let mut parent_read: HANDLE = std::ptr::null_mut();
        let mut child_write: HANDLE = std::ptr::null_mut();
        if CreatePipe(&mut parent_read, &mut child_write, &sa, 0) == 0 {
            let err = GetLastError();
            CloseHandle(child_read);
            CloseHandle(parent_write);
            return Err(format!("CreatePipe(response) failed: {}", err));
        }

        // Los extremos del padre no deben heredarse por el hijo.
        if SetHandleInformation(parent_write, HANDLE_FLAG_INHERIT, 0) == 0
            || SetHandleInformation(parent_read, HANDLE_FLAG_INHERIT, 0) == 0
        {
            let err = GetLastError();
            CloseHandle(child_read);
            CloseHandle(parent_write);
            CloseHandle(parent_read);
            CloseHandle(child_write);
            return Err(format!("SetHandleInformation failed: {}", err));
        }

        Ok(PipeSpawn {
            child_read,
            child_write,
            parent_read,
            parent_write,
            generation: NEXT_CDP_PIPE_GEN.fetch_add(1, Ordering::Relaxed) + 1,
        })
    }
}

struct AttributeListGuard {
    ptr: LPPROC_THREAD_ATTRIBUTE_LIST,
    #[allow(dead_code)]
    buf: Vec<u8>,
}

impl Drop for AttributeListGuard {
    fn drop(&mut self) {
        unsafe {
            DeleteProcThreadAttributeList(self.ptr);
        }
    }
}

fn build_handle_attribute_list(
    first: HANDLE,
    second: HANDLE,
) -> Result<AttributeListGuard, String> {
    unsafe {
        let mut size: usize = 0;
        InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut size);
        if size == 0 {
            return Err(format!(
                "attribute list size query failed: {}",
                GetLastError()
            ));
        }

        let mut buf = vec![0u8; size];
        let ptr = buf.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;
        if InitializeProcThreadAttributeList(ptr, 1, 0, &mut size) == 0 {
            return Err(format!(
                "InitializeProcThreadAttributeList failed: {}",
                GetLastError()
            ));
        }

        let handles = [first, second];
        if UpdateProcThreadAttribute(
            ptr,
            0,
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
            handles.as_ptr() as *const c_void,
            mem::size_of::<[HANDLE; 2]>(),
            std::ptr::null_mut(),
            std::ptr::null(),
        ) == 0
        {
            let err = GetLastError();
            DeleteProcThreadAttributeList(ptr);
            return Err(format!("UpdateProcThreadAttribute failed: {}", err));
        }

        Ok(AttributeListGuard { ptr, buf })
    }
}

fn spawn_pipe_probe(pair: CdpPipePair) {
    let _ = std::thread::Builder::new()
        .name("pipe-probe".into())
        .spawn(move || run_pipe_probe(pair));
}

fn run_pipe_probe(pair: CdpPipePair) {
    std::thread::sleep(std::time::Duration::from_secs(3));
    let payload = "{\"id\":1,\"method\":\"Target.getTargets\"}".to_string() + "\0";
    unsafe {
        let bytes = payload.as_bytes();
        let mut written_total = 0usize;
        while written_total < bytes.len() {
            let mut written: u32 = 0;
            let ok = WriteFile(
                pair.parent_write,
                bytes[written_total..].as_ptr(),
                (bytes.len() - written_total) as u32,
                &mut written,
                std::ptr::null_mut(),
            );
            if ok == 0 || written == 0 {
                crate::log_to_temp(&format!(
                    "[steamcdp] Pipe probe gen {}: write failed ({})",
                    pair.generation,
                    GetLastError()
                ));
                return;
            }
            written_total += written as usize;
        }

        for _ in 0..150 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            let mut avail: u32 = 0;
            if PeekNamedPipe(
                pair.parent_read,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                &mut avail,
                std::ptr::null_mut(),
            ) == 0
            {
                crate::log_to_temp(&format!(
                    "[steamcdp] Pipe probe gen {}: peek failed ({})",
                    pair.generation,
                    GetLastError()
                ));
                return;
            }
            if avail == 0 {
                continue;
            }
            let mut buf = vec![0u8; avail as usize];
            let mut read: u32 = 0;
            if ReadFile(
                pair.parent_read,
                buf.as_mut_ptr(),
                avail,
                &mut read,
                std::ptr::null_mut(),
            ) == 0
            {
                crate::log_to_temp(&format!(
                    "[steamcdp] Pipe probe gen {}: read failed ({})",
                    pair.generation,
                    GetLastError()
                ));
                return;
            }
            let text = String::from_utf8_lossy(&buf[..(read as usize).min(buf.len())]);
            let shown: String = text.chars().take(300).collect();
            crate::log_to_temp(&format!(
                "[steamcdp] Pipe probe gen {} (pid {}): CDP answered: {}",
                pair.generation, pair.child_pid, shown
            ));
            return;
        }
        crate::log_to_temp(&format!(
            "[steamcdp] Pipe probe gen {}: no CDP answer within 15s",
            pair.generation
        ));
    }
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

    let lower_app = app_name.to_ascii_lowercase();
    let lower_cmd = original_cmd.to_ascii_lowercase();
    let is_webhelper_spawn = (lower_app.contains("steamwebhelper.exe")
        || lower_cmd.contains("steamwebhelper.exe"))
        && !lower_cmd.contains("--type=");

    // Steam may already pass --remote-debugging-port itself (`.cef-enable-debugging`
    // file or explicit launch flags). Adopt that port instead of fighting it:
    // publish it, start the watch loop and inject cef_hook anyway, so the rest
    // of the chain (CDP, bridge, themes, Lua) keeps working.
    let adopted = if is_webhelper_spawn {
        extract_existing_debug_port(&original_cmd)
    } else {
        None
    };

    // Resolve the port only when a webhelper spawn actually needs it â€” this
    // also shrinks the window between dynamic selection and CEF binding it.
    let port: Option<u16> = if is_webhelper_spawn {
        Some(*DEBUG_PORT.get_or_init(|| match adopted {
            Some(p) => p,
            None => crate::discovery::resolve_debug_port(),
        }))
    } else {
        None
    };

    // TCP CDP master switch (default off while the pipe path is validated):
    // Steam's own adopted port always keeps working; LUMAFORGE_NO_CDP_PIPES=1
    // keeps its old meaning of forcing the TCP path; otherwise CDP rides the
    // inherited pipes + relay only.
    let tcp_on = adopted.is_some() || cdp_tcp_enabled() || cdp_pipes_disabled();

    let mut new_cmd = match port {
        Some(p) if adopted.is_none() => build_webhelper_command_line(
            &app_name,
            &original_cmd,
            if tcp_on { Some(p) } else { None },
        ),
        _ => None,
    };

    // CDP sobre pipes: solo cuando Steam NO trae ya un puerto propio (en ese
    // caso se adopta el TCP tal cual, camino ya probado). El attribute list
    // restringe la herencia de handles a exactamente nuestros extremos.
    let want_pipes = is_webhelper_spawn && adopted.is_none() && !cdp_pipes_disabled();
    let mut pipe_spawn: Option<PipeSpawn> = None;
    if want_pipes {
        if lp_startup_info.is_null()
            || (*lp_startup_info).cb as usize >= mem::size_of::<STARTUPINFOEXW>()
        {
            if !lp_startup_info.is_null() {
                crate::log_to_temp(
                    "[steamcdp] Steam already uses an extended STARTUPINFO, skipping CDP pipes",
                );
            }
        } else {
            match create_cdp_pipe_spawn() {
                Ok(spawn) => pipe_spawn = Some(spawn),
                Err(e) => {
                    crate::log_to_temp(&format!("[steamcdp] CDP pipe creation failed: {}", e))
                }
            }
        }
    }

    // Pipes ride on the command line we hand to CreateProcessW. When the
    // builder declined (e.g. Steam already passes a malformed
    // --remote-debugging-port, so it cannot be adopted or rewritten), fall
    // back to the original line as base — otherwise the pipe flags would be
    // silently dropped and pipe-only sessions would have no CDP at all.
    if pipe_spawn.is_some() && new_cmd.is_none() && !original_cmd.trim().is_empty() {
        new_cmd = Some(original_cmd.clone());
    }

    if pipe_spawn.is_some() && new_cmd.is_none() {
        pipe_spawn = None;
    }

    if let Some(spawn) = pipe_spawn.as_mut() {
        append_pipe_flags(
            new_cmd.as_mut().unwrap(),
            spawn.child_read,
            spawn.child_write,
        );
        crate::log_to_temp(&format!(
            "[steamcdp] Pipe CDP attached to spawn (gen {}): io-pipes={},{}",
            spawn.generation, spawn.child_read as usize, spawn.child_write as usize
        ));
    }

    if let Some(port) = port {
        // Only advertise a Steam-provided (adopted) port right away — an
        // injected port may never bind (pipe-only sessions), and the watch
        // loop publishes it after the first successful TCP connect instead.
        if adopted.is_some() {
            if let Err(e) = crate::discovery::publish_if_needed(port) {
                crate::log_to_temp(&format!(
                    "[steamcdp] Failed to publish CDP discovery: {}",
                    e
                ));
            }
        }
        // Full (truncated) command line so a missing/broken --remote-debugging-port
        // is visible in the log instead of requiring a repro on the machine.
        let shown: String = new_cmd
            .as_deref()
            .unwrap_or(&original_cmd)
            .chars()
            .take(1200)
            .collect();
        if adopted.is_some() {
            crate::log_to_temp(&format!(
                "[steamcdp] Adopted Steam-provided debug port {} (CPW): {}",
                port, shown
            ));
        } else if tcp_on {
            crate::log_to_temp(&format!(
                "[steamcdp] Injected debug port {} into steamwebhelper.exe (CPW): {}",
                port, shown
            ));
        } else {
            crate::log_to_temp(&format!(
                "[steamcdp] TCP CDP disabled, pipe-only spawn (CPW): {}",
                shown
            ));
        }
    }

    let is_webhelper = port.is_some();

    // Iniciar el thread de CDP solo una vez — only when a TCP CDP path
    // exists (injected or adopted); pipe-only sessions rely on the pipe
    // watch loop started at first pipe registration instead.
    static INJECTION_STARTED: Once = Once::new();
    if let Some(port) = port {
        if tcp_on {
            INJECTION_STARTED.call_once(|| {
                std::thread::spawn(move || {
                    start_cdp_watch_loop(port);
                });
            });
        }
    }

    let mut inherit_out = b_inherit_handles;
    let mut flags_out = dw_creation_flags;
    let mut startup_out = lp_startup_info;
    let mut siex: STARTUPINFOEXW = mem::zeroed();
    let mut attr_guard: Option<AttributeListGuard> = None;

    if let Some(spawn) = pipe_spawn.as_mut() {
        inherit_out = 1;
        match build_handle_attribute_list(spawn.child_read, spawn.child_write) {
            Ok(guard) => {
                siex.StartupInfo = *lp_startup_info;
                siex.StartupInfo.cb = mem::size_of::<STARTUPINFOEXW>() as u32;
                siex.lpAttributeList = guard.ptr;
                attr_guard = Some(guard);
                flags_out = dw_creation_flags | EXTENDED_STARTUPINFO_PRESENT;
                startup_out = &siex.StartupInfo as *const STARTUPINFOW;
            }
            Err(e) => {
                crate::log_to_temp(&format!(
                    "[steamcdp] Attribute list setup failed (gen {}): {} - full handle inherit",
                    spawn.generation, e
                ));
            }
        }
    }

    let mut cmd_storage: Option<Vec<u16>> =
        new_cmd.map(|m| m.encode_utf16().chain(Some(0)).collect());
    let cmd_ptr = match cmd_storage.as_mut() {
        Some(v) => v.as_mut_ptr(),
        None => lp_command_line,
    };

    let result = original(
        lp_application_name,
        cmd_ptr,
        lp_process_attributes,
        lp_thread_attributes,
        inherit_out,
        flags_out,
        lp_environment,
        lp_current_directory,
        startup_out,
        lp_process_information,
    );
    drop(attr_guard);

    // Record every webhelper this session spawns (pipes or not) before the
    // stale-kill thread can observe the new pid — these are fresh by
    // definition and must never be terminated.
    if result != 0 && is_webhelper_spawn {
        note_spawned_webhelper((*lp_process_information).dwProcessId);
    }

    if let Some(spawn) = pipe_spawn {
        if result != 0 {
            let pid = (*lp_process_information).dwProcessId;
            let pair = spawn.into_pair(pid);
            crate::log_to_temp(&format!(
                "[steamcdp] CDP pipes registered (gen {}): webhelper pid {}",
                pair.generation, pair.child_pid
            ));
            if cdp_pipe_probe_enabled() {
                spawn_pipe_probe(pair);
            }
            register_cdp_pipe_pair(pair);
            static PIPE_LOOP_STARTED: Once = Once::new();
            PIPE_LOOP_STARTED.call_once(|| {
                crate::relay::start_relay_server();
                std::thread::spawn(|| start_cdp_pipe_watch_loop());
            });
        } else {
            crate::log_to_temp(&format!(
                "[steamcdp] Webhelper spawn failed (gen {}), CDP pipes closed",
                spawn.generation
            ));
        }
    }

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
    client: &mut crate::transport::Transport<'_>,
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
/// Tries the proxy's own ports only â€” no luma-lite dependency.
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

        let http_method =
            reqwest::Method::from_bytes(method.as_bytes()).unwrap_or(reqwest::Method::GET);
        let mut req = client.request(http_method, &target_url);
        if let Some(b) = body {
            req = req
                .body(b.to_string())
                .header("content-type", "application/json");
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

/// Main CDP watch loop for Windows: connect with exponential backoff (retrying
/// until the port responds), inject, drain bridge queue, re-inject on URL
/// changes, reconnect on drop.
fn start_cdp_watch_loop(port: u16) {
    let mut port = port;
    // Discovery is published only after a real TCP connect succeeds below —
    // publishing an injected-but-dead port here would mislead consumers on
    // pipe-only sessions (lost1).

    crate::log_to_temp(&format!(
        "[steamcdp] Windows CDP injection loop started, port={}",
        port
    ));

    // Infinite retry with capped backoff: 100ms â†’ 200ms â†’ 400ms â†’ 800ms â†’
    // 1600ms â†’ 2s (cap). CEF can take a while to open the DevTools port on
    // slow machines (AV scanning, first boot), so never give up for good â€”
    // previously we stopped after ~14s and the port came up later unused.
    let mut attempt: u64 = 0;
    let mut standby_logged = false;
    loop {
        attempt += 1;
        let delay_ms = std::cmp::min(100u64 << ((attempt - 1).min(5)), 2000);
        std::thread::sleep(std::time::Duration::from_millis(delay_ms));

        // While pipe pairs are registered (and no fallback), do not even
        // probe the injected TCP port — the pipe session owns injection and
        // a probe connection per cycle is pure churn.
        if !crate::transport::tcp_allowed() {
            if !standby_logged {
                crate::log_to_temp("[steamcdp] TCP transport on standby (pipe session active)");
                standby_logged = true;
            }
            continue;
        }

        // Probe the resolved port first, then Steam's fixed 8080 fallback.
        // On a switch, republish discovery so external consumers follow.
        let mut connected: Option<crate::cdp::CdpClient> = None;
        let mut errs: Vec<String> = Vec::new();
        for &cand in &crate::discovery::debug_port_candidates(port) {
            match crate::cdp::CdpClient::connect(cand) {
                Ok(client) => {
                    if cand != port {
                        crate::log_to_temp(&format!(
                            "[steamcdp] Switching to debug port {} (was {})",
                            cand, port
                        ));
                        if let Err(e) = crate::discovery::publish_if_needed(cand) {
                            crate::log_to_temp(&format!(
                                "[steamcdp] Failed to publish CDP discovery: {}",
                                e
                            ));
                        }
                        port = cand;
                    }
                    connected = Some(client);
                    break;
                }
                Err(e) => errs.push(format!("{}: {}", cand, e)),
            }
        }

        match connected {
            Some(mut client) => {
                // A real TCP endpoint answered: publish discovery (even if the
                // pipe session currently owns injection, the port works for
                // direct consumers like cef_hook).
                if let Err(e) = crate::discovery::publish_if_needed(port) {
                    crate::log_to_temp(&format!(
                        "[steamcdp] Failed to publish CDP discovery: {}",
                        e
                    ));
                }
                if !crate::transport::claim_tcp() {
                    if !standby_logged {
                        crate::log_to_temp(
                            "[steamcdp] TCP transport on standby (pipe session active)",
                        );
                        standby_logged = true;
                    }
                    continue;
                }
                standby_logged = false;
                crate::log_to_temp(&format!(
                    "[steamcdp] Connected to CDP (attempt {})",
                    attempt
                ));
                match crate::injector::inject_all(&mut crate::transport::Transport::Tcp(
                    &mut client,
                )) {
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
                let theme_bundle = crate::injector::load_theme_patches();
                let mut recheck_counter = 0u32;
                let mut reconnect_fails = 0u32;
                let mut known_urls: std::collections::HashMap<String, String> =
                    std::collections::HashMap::new();
                if let Ok(targets) = client.get_targets() {
                    for t in &targets {
                        if t.target_type == "page" && injected_targets.contains(&t.id) {
                            known_urls.insert(t.id.clone(), t.url.clone());
                        }
                    }
                }

                'session: loop {
                    std::thread::sleep(std::time::Duration::from_secs(1));
                    recheck_counter += 1;

                    // Drain bridge proxy queue every cycle (~1s) so extension fetch
                    // requests are fulfilled within the JS timeout. Only real Steam
                    // pages carry BRIDGE_PROXY_JS â€” context menus/supernavs/shared
                    // contexts always return an empty queue, and evaluating them
                    // every second used to dominate the cycle time.
                    let drain_ids: std::collections::HashSet<String> = match client.get_targets() {
                        Ok(ts) => ts
                            .iter()
                            .filter(|t| {
                                t.target_type == "page"
                                    && injected_targets.contains(&t.id)
                                    && crate::injector::is_real_steam_page(&t.url)
                            })
                            .map(|t| t.id.clone())
                            .collect(),
                        Err(_) => std::collections::HashSet::new(),
                    };
                    drain_bridge_queue(
                        &mut crate::transport::Transport::Tcp(&mut client),
                        &drain_ids,
                    );

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
                                        &mut crate::transport::Transport::Tcp(&mut client),
                                        t,
                                        &plugins,
                                        &theme_bundle,
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
                                    // Skip shutdown/close targets â€” injecting into these
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
                                        &mut crate::transport::Transport::Tcp(&mut client),
                                        t,
                                        &plugins,
                                        &theme_bundle,
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
                                    // Detect URL change on existing store target â†’ re-inject
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
                                                    &mut crate::transport::Transport::Tcp(
                                                        &mut client,
                                                    ),
                                                    t,
                                                    &plugins,
                                                    &theme_bundle,
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
                                    reconnect_fails = 0;
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
                                    reconnect_fails += 1;
                                    if reconnect_fails >= 3 {
                                        crate::log_to_temp(
                                            "[steamcdp] TCP transport released after repeated reconnect failures",
                                        );
                                        crate::transport::release(crate::transport::TRANSPORT_TCP);
                                        break 'session;
                                    }
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
            None => {
                // Throttle: first 10 failures, then one every 30 attempts
                // (~1 min at the 2s cap) so the log stays readable.
                if attempt <= 10 || attempt % 30 == 0 {
                    crate::log_to_temp(&format!(
                        "[steamcdp] CDP connect attempt {} failed: {}",
                        attempt,
                        errs.join("; ")
                    ));
                }
            }
        }
    }
}

// --- Pipe CDP watch loop (Windows: CDP over inherited pipes, no TCP port) ---

fn start_cdp_pipe_watch_loop() {
    crate::log_to_temp("[steamcdp] Pipe CDP watch loop started");
    let mut attempt: u64 = 0;
    let mut standby_logged = false;
    loop {
        attempt += 1;
        let delay_ms = std::cmp::min(300u64 << ((attempt - 1).min(3)), 2000);
        std::thread::sleep(std::time::Duration::from_millis(delay_ms));

        if crate::transport::current() == crate::transport::TRANSPORT_TCP {
            if !standby_logged {
                crate::log_to_temp("[steamcdp] Pipe loop on standby (TCP transport active)");
                standby_logged = true;
            }
            continue;
        }
        standby_logged = false;

        let pairs = cdp_pipe_pairs();
        let mut connected: Option<crate::cdp_pipe_win::CdpPipeClient> = None;
        let mut last_err = format!("no pipe pairs registered ({} pairs)", pairs.len());
        for pair in pairs.iter().rev() {
            let client = crate::cdp_pipe_win::CdpPipeClient::from_pair(pair.clone());
            match client.probe(std::time::Duration::from_secs(3)) {
                Ok(_) => {
                    connected = Some(client);
                    break;
                }
                Err(e) => {
                    last_err = format!("gen {}: {}", pair.generation, e);
                }
            }
        }
        let client = match connected {
            Some(c) => c,
            None => {
                // No TCP fallback: the session stays pipe-only and keeps
                // waiting for the (re)spawned helper with fresh pipes — the
                // same contract Millennium has (pipe replaced → reconnect).
                if attempt <= 10 || attempt % 30 == 0 {
                    crate::log_to_temp(&format!(
                        "[steamcdp] Pipe probe attempt {} failed: {}",
                        attempt, last_err
                    ));
                }
                continue;
            }
        };
        if !crate::transport::claim_pipe() {
            continue;
        }
        attempt = 0;
        crate::log_to_temp(&format!(
            "[steamcdp] Pipe CDP connected (gen {}, webhelper pid {})",
            client.generation(),
            client.child_pid()
        ));
        let connected_gen = client.generation();
        crate::transport::set_pipe_session(client);
        purge_dead_pairs_older_than(connected_gen);

        match crate::injector::inject_all(&mut crate::transport::Transport::PipeShared) {
            Ok(()) => {
                crate::log_to_temp("[steamcdp] Injection complete");
            }
            Err(e) => {
                crate::log_to_temp(&format!("[steamcdp] Injection error: {}", e));
            }
        }

        let mut injected_targets: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        if let Ok(targets) = crate::transport::Transport::PipeShared.get_targets() {
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

        crate::log_to_temp("[steamcdp] Watching for new targets (pipe)...");
        let theme_bundle = crate::injector::load_theme_patches();
        let mut recheck_counter = 0u32;
        let mut known_urls: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        if let Ok(targets) = crate::transport::Transport::PipeShared.get_targets() {
            for t in &targets {
                if t.target_type == "page" && injected_targets.contains(&t.id) {
                    known_urls.insert(t.id.clone(), t.url.clone());
                }
            }
        }

        'session: loop {
            std::thread::sleep(std::time::Duration::from_secs(1));
            recheck_counter += 1;

            let drain_ids: std::collections::HashSet<String> =
                match crate::transport::Transport::PipeShared.get_targets() {
                    Ok(ts) => ts
                        .iter()
                        .filter(|t| {
                            t.target_type == "page"
                                && injected_targets.contains(&t.id)
                                && crate::injector::is_real_steam_page(&t.url)
                        })
                        .map(|t| t.id.clone())
                        .collect(),
                    Err(_) => std::collections::HashSet::new(),
                };
            drain_bridge_queue(&mut crate::transport::Transport::PipeShared, &drain_ids);

            if recheck_counter % 30 == 0 {
                if let Ok(all_targets) = crate::transport::Transport::PipeShared.get_targets() {
                    let page_count = all_targets
                        .iter()
                        .filter(|t| t.target_type == "page")
                        .count();
                    crate::log_to_temp(&format!(
                        "[steamcdp] Recheck (pipe): {} total targets ({} pages), {} injected",
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
                        if crate::transport::Transport::PipeShared
                            .attach_to_target(&t.id)
                            .is_err()
                        {
                            continue;
                        }
                        let diag_expr = r#"JSON.stringify({url:window.location.href.substring(0,200),title:document.title.substring(0,80),bodyLen:document.body?document.body.innerHTML.length:-1,hasLuma:!!window.__lumaforge_ssh__,lumaActive:window.__lumaforge_ssh__&&window.__lumaforge_ssh__.active,lumaAppId:window.__lumaforge_ssh__&&window.__lumaforge_ssh__.currentAppId,btnExists:!!document.getElementById('luma-action-btn'),appLinks:document.querySelectorAll('a[href*="/app/"]').length})"#;
                        let msg_id = 9000 + checked as u64;
                        let mut diag_val = String::new();
                        if let Ok(resp) = crate::transport::Transport::PipeShared.send_cdp_wait(
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
                            let plugins =
                                crate::plugin_loader::load_enabled_plugins().unwrap_or_default();
                            if let Err(e) = crate::injector::inject_into_target(
                                &mut crate::transport::Transport::PipeShared,
                                t,
                                &plugins,
                                &theme_bundle,
                                checked as usize,
                            ) {
                                crate::log_to_temp(&format!("[steamcdp] Re-inject failed: {}", e));
                            }
                        }
                    }
                }
            }

            match crate::transport::Transport::PipeShared.get_targets() {
                Ok(new_targets) => {
                    for t in &new_targets {
                        if t.target_type == "page"
                            && !injected_targets.contains(&t.id)
                            && crate::injector::is_real_steam_page(&t.url)
                        {
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
                            let plugins =
                                crate::plugin_loader::load_enabled_plugins().unwrap_or_default();
                            if let Err(e) = crate::injector::inject_into_target(
                                &mut crate::transport::Transport::PipeShared,
                                t,
                                &plugins,
                                &theme_bundle,
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
                        } else if t.target_type == "page" && injected_targets.contains(&t.id) {
                            if t.url.contains("store.steampowered.com") {
                                if let Some(prev_url) = known_urls.get(&t.id) {
                                    if prev_url != &t.url {
                                        crate::log_to_temp(&format!(
                                            "[steamcdp] Store URL changed: id={}, prev={}, new={}",
                                            t.id,
                                            &prev_url[..prev_url.len().min(80)],
                                            &t.url[..t.url.len().min(80)]
                                        ));
                                        let plugins = crate::plugin_loader::load_enabled_plugins()
                                            .unwrap_or_default();
                                        if let Err(e) = crate::injector::inject_into_target(
                                            &mut crate::transport::Transport::PipeShared,
                                            t,
                                            &plugins,
                                            &theme_bundle,
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
                        "[steamcdp] Pipe get_targets error: {}, re-probing...",
                        e
                    ));
                    crate::transport::release(crate::transport::TRANSPORT_PIPE);
                    crate::transport::clear_pipe_session();
                    break 'session;
                }
            }
        }
        crate::log_to_temp("[steamcdp] Pipe CDP session ended, re-probing...");
    }
}

// --- InstalaciÃ³n de hooks (solo CreateProcessW) ---
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

// --- InyecciÃ³n de DLL en proceso webhelper ---
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
        let r = build_webhelper_command_line("", "steamwebhelper.exe --some-arg", Some(9222));
        assert_eq!(
            r,
            Some(
                "steamwebhelper.exe --some-arg --remote-debugging-port=9222 --remote-allow-origins=*"
                    .into()
            )
        );
    }

    #[test]
    fn primary_from_application_name() {
        let r = build_webhelper_command_line(
            "C:\\Program Files\\Steam\\steamwebhelper.exe",
            "",
            Some(9222),
        );
        assert_eq!(
            r,
            Some(
                "\"C:\\Program Files\\Steam\\steamwebhelper.exe\" --remote-debugging-port=9222 \
                 --remote-allow-origins=*"
                    .into()
            )
        );
    }

    #[test]
    fn reject_type_flag() {
        let r = build_webhelper_command_line(
            "",
            "steamwebhelper.exe --type=renderer --some-arg",
            Some(9222),
        );
        assert_eq!(r, None);
    }

    #[test]
    fn do_not_duplicate_debug_port() {
        let r = build_webhelper_command_line(
            "",
            "steamwebhelper.exe --remote-debugging-port=9222",
            Some(9222),
        );
        assert_eq!(r, None);
    }

    #[test]
    fn tcp_off_builds_base_without_port() {
        let r = build_webhelper_command_line("", "steamwebhelper.exe --some-arg", None);
        assert_eq!(
            r,
            Some("steamwebhelper.exe --some-arg --remote-allow-origins=*".into())
        );
        let r2 = build_webhelper_command_line("C:\\Steam\\steamwebhelper.exe", "", None);
        assert_eq!(
            r2,
            Some("\"C:\\Steam\\steamwebhelper.exe\" --remote-allow-origins=*".into())
        );
    }

    #[test]
    fn do_not_duplicate_allow_origins() {
        let r = build_webhelper_command_line(
            "",
            "steamwebhelper.exe --remote-allow-origins=*",
            Some(9222),
        );
        assert_eq!(
            r,
            Some("steamwebhelper.exe --remote-allow-origins=* --remote-debugging-port=9222".into())
        );
    }

    #[test]
    fn pipe_flags_appended() {
        let mut cmd = "steamwebhelper.exe --some-arg".to_string();
        append_pipe_flags(&mut cmd, 0x123usize as HANDLE, 0x456usize as HANDLE);
        assert_eq!(
            cmd,
            "steamwebhelper.exe --some-arg --remote-debugging-pipe --remote-debugging-io-pipes=291,1110"
        );
    }

    #[test]
    fn pipe_flags_not_duplicated() {
        let mut cmd = "steamwebhelper.exe --remote-debugging-pipe --remote-debugging-io-pipes=3,4"
            .to_string();
        append_pipe_flags(&mut cmd, 1usize as HANDLE, 2usize as HANDLE);
        assert_eq!(
            cmd,
            "steamwebhelper.exe --remote-debugging-pipe --remote-debugging-io-pipes=3,4"
        );
    }

    #[test]
    fn pipe_flag_args_format() {
        assert_eq!(
            pipe_flag_args(4, 8),
            "--remote-debugging-pipe --remote-debugging-io-pipes=4,8"
        );
    }

    #[test]
    fn extract_existing_port_from_tail() {
        let r = extract_existing_debug_port(
            "\"C:\\Steam\\steamwebhelper.exe\" -dev --remote-debugging-port=8080",
        );
        assert_eq!(r, Some(8080));
    }

    #[test]
    fn extract_existing_port_in_middle() {
        let r =
            extract_existing_debug_port("steamwebhelper.exe --remote-debugging-port=9222 --other");
        assert_eq!(r, Some(9222));
    }

    #[test]
    fn extract_existing_port_case_insensitive() {
        let r = extract_existing_debug_port("steamwebhelper.exe --REMOTE-DEBUGGING-PORT=8080");
        assert_eq!(r, Some(8080));
    }

    #[test]
    fn extract_missing_flag_is_none() {
        assert_eq!(extract_existing_debug_port("steamwebhelper.exe -dev"), None);
    }

    #[test]
    fn extract_malformed_flag_is_none() {
        assert_eq!(
            extract_existing_debug_port("steamwebhelper.exe --remote-debugging-port="),
            None
        );
        assert_eq!(
            extract_existing_debug_port("steamwebhelper.exe --remote-debugging-port=abc"),
            None
        );
        assert_eq!(
            extract_existing_debug_port("steamwebhelper.exe --remote-debugging-port=80"),
            None
        );
    }
}
