use std::ffi::{CStr, CString};
use std::io::Write;
use std::os::raw::c_char;
use std::sync::OnceLock;

type ExecveFn = unsafe extern "C" fn(
    pathname: *const c_char,
    argv: *const *const c_char,
    envp: *const *const c_char,
) -> i32;

static ORIGINAL_EXECVE: OnceLock<ExecveFn> = OnceLock::new();
static PATCH_DONE: OnceLock<bool> = OnceLock::new();

fn debug_log(msg: &str) {
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("/tmp/lumaforge_hook.log")
    {
        let _ = writeln!(file, "[hook] {}", msg);
    }
}

fn resolve_port() -> u16 {
    if let Ok(val) = std::env::var("STEAMCDP_PORT") {
        if let Ok(port) = val.trim().parse::<u16>() {
            if port >= 1024 {
                debug_log(&format!("port from env: {}", port));
                return port;
            }
        }
    }

    let home = match std::env::var("HOME") {
        Ok(h) => h,
        Err(_) => return 9222,
    };
    let path = format!("{}/.local/share/LumaForge/runtime/steam-cdp.json", home);
    if let Ok(content) = std::fs::read_to_string(&path) {
        let marker = r#""port":"#;
        if let Some(idx) = content.find(marker) {
            let rest = &content[idx + marker.len()..];
            if let Some(end) = rest.find(|c: char| !c.is_ascii_digit()) {
                if let Ok(port) = rest[..end].parse::<u16>() {
                    if port >= 1024 {
                        debug_log(&format!("port from discovery file: {}", port));
                        return port;
                    }
                }
            }
        }
    }

    debug_log("port defaulted to 9222");
    9222
}

fn patch_steamwebhelper_script(port: u16) {
    let home = match std::env::var("HOME") {
        Ok(h) => h,
        Err(_) => {
            debug_log("patch: HOME not set");
            return;
        }
    };

    let script_path = format!(
        "{}/.local/share/Steam/ubuntu12_64/steamwebhelper_sniper_wrap.sh",
        home
    );

    let content = match std::fs::read_to_string(&script_path) {
        Ok(c) => c,
        Err(e) => {
            debug_log(&format!("patch: failed to read {}: {}", script_path, e));
            return;
        }
    };

    let flag = format!("--remote-debugging-port={}", port);

    let clean_line = "exec ./steamwebhelper \"$@\"";
    let patched_line = format!("{} {}", clean_line, flag);

    let new_content = content
        .lines()
        .map(|line| {
            if line.trim() == clean_line || line.starts_with(&format!("{} --remote-debugging-port=", clean_line)) {
                patched_line.clone()
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");

    if new_content == content {
        debug_log(&format!("patch: already correct with {}", flag));
        return;
    }

    if let Err(e) = std::fs::write(&script_path, new_content) {
        debug_log(&format!("patch: write failed: {}", e));
    } else {
        debug_log(&format!("patch: SUCCESS: {}", flag));
    }
}

fn ensure_patched() {
    PATCH_DONE.get_or_init(|| {
        debug_log("lazy patch: reading port");
        let port = resolve_port();
        patch_steamwebhelper_script(port);
        true
    });
}

fn get_original_execve() -> ExecveFn {
    *ORIGINAL_EXECVE.get_or_init(|| unsafe {
        let ptr = libc::dlsym(libc::RTLD_NEXT, b"execve\0".as_ptr() as *const c_char);
        assert!(!ptr.is_null(), "dlsym failed to find real execve");
        std::mem::transmute(ptr)
    })
}

fn argv_to_vec(argv: *const *const c_char) -> Vec<String> {
    let mut result = Vec::new();
    unsafe {
        let mut i = 0;
        loop {
            let ptr = *argv.add(i);
            if ptr.is_null() {
                return result;
            }
            let s = CStr::from_ptr(ptr).to_string_lossy().into_owned();
            result.push(s);
            i += 1;
        }
    }
}

fn vec_to_cstrings(strings: &[String]) -> Vec<CString> {
    strings
        .iter()
        .map(|s| CString::new(s.as_str()).unwrap())
        .collect()
}

fn should_inject(argv: &[String]) -> bool {
    let is_webhelper = argv.iter().any(|a| a.contains("steamwebhelper"));
    if !is_webhelper {
        return false;
    }
    if argv.iter().any(|a| a.starts_with("--type=")) {
        return false;
    }
    if argv.iter().any(|a| a.starts_with("--remote-debugging-port")) {
        return false;
    }
    true
}

#[no_mangle]
pub unsafe extern "C" fn execve(
    pathname: *const c_char,
    argv: *const *const c_char,
    envp: *const *const c_char,
) -> i32 {
    let original = get_original_execve();

    if argv.is_null() || pathname.is_null() {
        return original(pathname, argv, envp);
    }

    let args = argv_to_vec(argv);

    if should_inject(&args) {
        ensure_patched();
        let port = resolve_port();
        let flag = format!("--remote-debugging-port={}", port);
        debug_log(&format!("execve INJECT: {}", flag));
        let mut new_args = args;
        new_args.push(flag);

        let c_args = vec_to_cstrings(&new_args);
        let mut ptrs: Vec<*const c_char> = c_args.iter().map(|cs| cs.as_ptr()).collect();
        ptrs.push(std::ptr::null());

        return original(pathname, ptrs.as_ptr(), envp);
    }

    original(pathname, argv, envp)
}
