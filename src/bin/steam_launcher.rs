#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("[LumaForge Launcher] This binary is only available on Windows.");
    eprintln!("[LumaForge Launcher] On Linux, use:");
    eprintln!("[LumaForge Launcher]   LD_PRELOAD=lumaforge_hook.so:liblumaforge.so steam");
    std::process::exit(1);
}

#[cfg(target_os = "windows")]
fn main() {
    use std::env;
    use std::path::PathBuf;

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
            RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY_LOCAL_MACHINE, KEY_READ, REG_SZ,
        };

        let key_path = encode_wide(r"SOFTWARE\Valve\Steam");
        let value_name = encode_wide("InstallPath");

        unsafe {
            let mut hkey: windows_sys::Win32::Foundation::HANDLE = std::ptr::null_mut();
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

    fn encode_wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

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
            eprintln!(
                "[LumaForge Launcher] Usage: {} [path\\to\\steam.exe]",
                exe_name_str
            );
            std::process::exit(1);
        }
    };
    eprintln!("[LumaForge Launcher] Found Steam: {}", steam_exe.display());

    let steam_dir = steam_exe
        .parent()
        .expect("steam.exe has no parent directory");

    let wsock32 = steam_dir.join("wsock32.dll");
    if !wsock32.exists() {
        eprintln!(
            "[LumaForge Launcher] ERROR: wsock32.dll not found at {}",
            wsock32.display()
        );
        eprintln!("[LumaForge Launcher] Place wsock32.dll in the Steam directory alongside steam.exe.");
        std::process::exit(1);
    }

    let lumaforge_dir = steam_dir.join("lumaforge");
    let main_dll = lumaforge_dir.join("lumaforge.dll");
    if !main_dll.exists() {
        eprintln!(
            "[LumaForge Launcher] ERROR: lumaforge.dll not found at {}",
            main_dll.display()
        );
        eprintln!("[LumaForge Launcher] Create a 'lumaforge' subfolder and place lumaforge.dll inside.");
        std::process::exit(1);
    }

    let cef_hook = steam_dir.join("lumaforge_cef_hook.dll");
    if !cef_hook.exists() {
        eprintln!(
            "[LumaForge Launcher] WARNING: lumaforge_cef_hook.dll not found at {}",
            cef_hook.display()
        );
    }

    eprintln!("[LumaForge Launcher] Bootstrap: {}", wsock32.display());
    eprintln!("[LumaForge Launcher] Main DLL: {}", main_dll.display());

    let forward_args: Vec<String> = env::args().skip(1).collect();
    let cmd_line = if forward_args.is_empty() {
        format!("\"{}\"", steam_exe.display())
    } else {
        format!("\"{}\" {}", steam_exe.display(), forward_args.join(" "))
    };

    eprintln!("[LumaForge Launcher] Command line: {}", cmd_line);
    eprintln!("[LumaForge Launcher] Launching Steam (wsock32.dll bootstrap will load automatically)...");

    match std::process::Command::new(&steam_exe)
        .args(forward_args.iter().map(|a| a.as_str()))
        .spawn()
    {
        Ok(child) => {
            eprintln!(
                "[LumaForge Launcher] Steam launched (PID: {})",
                child.id()
            );
        }
        Err(e) => {
            eprintln!("[LumaForge Launcher] ERROR: Failed to launch Steam: {}", e);
            std::process::exit(1);
        }
    }

    eprintln!("[LumaForge Launcher] Done. Steam is running with LumaForge CDP hooks.");
}
