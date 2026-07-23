use std::env;
use std::ffi::c_void;
use std::mem;
use std::path::{Path, PathBuf};
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE};
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows_sys::Win32::System::Memory::{VirtualAllocEx, VirtualFreeEx, MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE};
use windows_sys::Win32::System::Diagnostics::Debug::WriteProcessMemory;
use windows_sys::Win32::System::Threading::{
    CreateProcessW, CreateRemoteThread, GetExitCodeThread, ResumeThread,
    WaitForSingleObject, PROCESS_INFORMATION, STARTUPINFOW, CREATE_SUSPENDED,
};

const INFINITE: u32 = 0xFFFFFFFF;

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn find_steam_exe() -> Option<PathBuf> {
    let args: Vec<String> = env::args().skip(1).collect();
    if let Some(path) = args.first() {
        let p = PathBuf::from(path);
        if p.exists() {
            return Some(p);
        }
    }

    let candidates = [
        r"C:\Program Files (x86)\Steam\steam.exe",
        r"C:\Program Files\Steam\steam.exe",
    ];
    for c in &candidates {
        let p = PathBuf::from(c);
        if p.exists() {
            return Some(p);
        }
    }

    if let Some(p) = find_steam_via_registry() {
        return Some(p);
    }

    if let Ok(appdata) = env::var("PROGRAMFILES(X86)") {
        let p = PathBuf::from(appdata).join("Steam").join("steam.exe");
        if p.exists() {
            return Some(p);
        }
    }

    None
}

fn find_steam_via_registry() -> Option<PathBuf> {
    use windows_sys::Win32::System::Registry::{
        RegOpenKeyExW, RegQueryValueExW, RegCloseKey,
        HKEY_LOCAL_MACHINE, KEY_READ, REG_SZ,
    };

    let key_path = to_wide(r"SOFTWARE\Valve\Steam");
    let value_name = to_wide("InstallPath");

    unsafe {
        let mut hkey: HANDLE = std::ptr::null_mut();
        let res = RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            key_path.as_ptr(),
            0,
            KEY_READ,
            &mut hkey,
        );
        if res != 0 {
            return None;
        }

        let mut buf = [0u16; 260];
        let mut buf_len = (buf.len() * 2) as u32;
        let mut reg_type = 0u32;

        let res = RegQueryValueExW(
            hkey,
            value_name.as_ptr(),
            std::ptr::null_mut(),
            &mut reg_type,
            buf.as_mut_ptr() as *mut u8,
            &mut buf_len,
        );
        RegCloseKey(hkey);

        if res != 0 || reg_type != REG_SZ {
            return None;
        }

        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        let install_path = String::from_utf16_lossy(&buf[..len]);
        let exe = PathBuf::from(install_path).join("steam.exe");
        if exe.exists() {
            return Some(exe);
        }
    }

    None
}

unsafe fn inject_dll(process_handle: HANDLE, dll_path: &Path) -> Result<(), String> {
    let dll_path_wide = to_wide(dll_path.to_str().ok_or("Invalid DLL path")?);
    let dll_path_bytes = dll_path_wide.len() * 2;

    let kernel32 = GetModuleHandleW(to_wide("kernel32.dll").as_ptr());
    if kernel32.is_null() {
        return Err("Failed to get kernel32.dll handle".to_string());
    }

    let load_library_addr = GetProcAddress(kernel32, b"LoadLibraryW\0".as_ptr() as _);
    if load_library_addr.is_none() {
        return Err("Failed to find LoadLibraryW".to_string());
    }
    let load_library_fn: unsafe extern "system" fn(*const u16) -> u32 =
        mem::transmute(load_library_addr.unwrap());

    let remote_mem = VirtualAllocEx(
        process_handle,
        std::ptr::null_mut(),
        dll_path_bytes,
        MEM_COMMIT | MEM_RESERVE,
        PAGE_READWRITE,
    );
    if remote_mem.is_null() {
        return Err(format!("VirtualAllocEx failed: {}", GetLastError()));
    }

    let write_ok = WriteProcessMemory(
        process_handle,
        remote_mem,
        dll_path_wide.as_ptr() as *const c_void,
        dll_path_bytes,
        std::ptr::null_mut(),
    );
    if write_ok == 0 {
        VirtualFreeEx(process_handle, remote_mem, 0, 0);
        return Err(format!("WriteProcessMemory failed: {}", GetLastError()));
    }

    let mut thread_id = 0u32;
    let thread_handle = CreateRemoteThread(
        process_handle,
        std::ptr::null(),
        0,
        Some(mem::transmute(load_library_fn)),
        remote_mem,
        0,
        &mut thread_id,
    );

    if thread_handle.is_null() {
        VirtualFreeEx(process_handle, remote_mem, 0, 0);
        return Err(format!("CreateRemoteThread failed: {}", GetLastError()));
    }

    WaitForSingleObject(thread_handle, INFINITE);

    let mut exit_code = 0u32;
    GetExitCodeThread(thread_handle, &mut exit_code);
    CloseHandle(thread_handle);

    VirtualFreeEx(process_handle, remote_mem, 0, 0);

    if exit_code == 0 {
        return Err("LoadLibraryW returned NULL (DLL failed to load)".to_string());
    }

    Ok(())
}

fn main() {
    let exe_name = env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_os_string()))
        .unwrap_or_default();
    let exe_name_str = exe_name.to_string_lossy();

    eprintln!("[LumaForge Launcher] Starting...");

    let steam_exe = match find_steam_exe() {
        Some(p) => p,
        None => {
            eprintln!("[LumaForge Launcher] ERROR: Could not find steam.exe");
            eprintln!("[LumaForge Launcher] Usage: {} [path\\to\\steam.exe]", exe_name_str);
            std::process::exit(1);
        }
    };
    eprintln!("[LumaForge Launcher] Found Steam: {}", steam_exe.display());

    let steam_dir = steam_exe.parent().expect("steam.exe has no parent directory");
    let user32_proxy = steam_dir.join("user32.dll");
    if !user32_proxy.exists() {
        eprintln!(
            "[LumaForge Launcher] ERROR: user32.dll proxy not found at {}",
            user32_proxy.display()
        );
        eprintln!("[LumaForge Launcher] Place user32.dll in the Steam directory alongside steam.exe.");
        std::process::exit(1);
    }

    let real_user32 = steam_dir.join("REAL_USER32.dll");
    if !real_user32.exists() {
        eprintln!(
            "[LumaForge Launcher] WARNING: REAL_USER32.dll not found. Export forwarding may fail.",
        );
    }

    eprintln!("[LumaForge Launcher] DLL proxy: {}", user32_proxy.display());

    let forward_args: Vec<String> = env::args().skip(1).collect();
    let cmd_line = if forward_args.is_empty() {
        format!("\"{}\"", steam_exe.display())
    } else {
        format!("\"{}\" {}", steam_exe.display(), forward_args.join(" "))
    };

    eprintln!("[LumaForge Launcher] Command line: {}", cmd_line);

    let mut cmd_wide = to_wide(&cmd_line);

    let (created, pi) = unsafe {
        let mut si: STARTUPINFOW = mem::zeroed();
        si.cb = mem::size_of::<STARTUPINFOW>() as u32;
        let mut pi: PROCESS_INFORMATION = mem::zeroed();

        let created = CreateProcessW(
            std::ptr::null(),
            cmd_wide.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            CREATE_SUSPENDED,
            std::ptr::null(),
            std::ptr::null(),
            &mut si,
            &mut pi,
        );

        (created, pi)
    };

    if created == 0 {
        eprintln!(
            "[LumaForge Launcher] ERROR: CreateProcessW failed: {}",
            unsafe { GetLastError() }
        );
        std::process::exit(1);
    }

    eprintln!(
        "[LumaForge Launcher] Steam.exe created SUSPENDED (PID: {}, TID: {})",
        pi.dwProcessId, pi.dwThreadId
    );

    match unsafe { inject_dll(pi.hProcess, &user32_proxy) } {
        Ok(()) => {
            eprintln!("[LumaForge Launcher] DLL injected. DllMain ran, hooks installed.");
        }
        Err(e) => {
            eprintln!("[LumaForge Launcher] ERROR: DLL injection failed: {}", e);
            unsafe {
                windows_sys::Win32::System::Threading::TerminateProcess(pi.hProcess, 1);
                CloseHandle(pi.hProcess);
                CloseHandle(pi.hThread);
            }
            std::process::exit(1);
        }
    }

    let resumed = unsafe { ResumeThread(pi.hThread) };
    if resumed == u32::MAX {
        eprintln!(
            "[LumaForge Launcher] WARNING: ResumeThread failed: {}",
            unsafe { GetLastError() }
        );
    } else {
        eprintln!("[LumaForge Launcher] Steam.exe RESUMED. Hooks active, CDP injection ready.");
    }

    unsafe {
        CloseHandle(pi.hProcess);
        CloseHandle(pi.hThread);
    }

    eprintln!("[LumaForge Launcher] Done. Steam is running with CDP hooks.");
}
