use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_ulong, c_void};
use std::sync::atomic::{AtomicPtr, Ordering};

// ---------------------------------------------------------------------------
// Safe wrapper for raw pointers in statics
// ---------------------------------------------------------------------------

struct SafePtr(AtomicPtr<c_void>);

unsafe impl Send for SafePtr {}
unsafe impl Sync for SafePtr {}

impl SafePtr {
    const fn new() -> Self {
        SafePtr(AtomicPtr::new(std::ptr::null_mut()))
    }
    fn get(&self) -> *mut c_void {
        self.0.load(Ordering::Relaxed)
    }
    fn set(&self, ptr: *mut c_void) {
        self.0.store(ptr, Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------------------
// XTest function pointers — resolved from the real libXtst.so.6
// ---------------------------------------------------------------------------

type XTestFakeButtonEventFn = unsafe extern "C" fn(*mut c_void, c_ulong, c_int, c_ulong) -> c_int;
type XTestFakeKeyEventFn = unsafe extern "C" fn(*mut c_void, c_ulong, c_int, c_ulong) -> c_int;
type XTestQueryExtensionFn =
    unsafe extern "C" fn(*mut c_void, *mut c_int, *mut c_int, *mut c_int, *mut c_int) -> c_int;
type XTestFakeRelativeMotionEventFn =
    unsafe extern "C" fn(*mut c_void, c_int, c_int, c_ulong) -> c_int;
type XTestFakeMotionEventFn =
    unsafe extern "C" fn(*mut c_void, c_int, c_int, c_int, c_ulong) -> c_int;

static REAL_XTST: SafePtr = SafePtr::new();
static REAL_XTEST_FAKE_BUTTON_EVENT: SafePtr = SafePtr::new();
static REAL_XTEST_FAKE_KEY_EVENT: SafePtr = SafePtr::new();
static REAL_XTEST_QUERY_EXTENSION: SafePtr = SafePtr::new();
static REAL_XTEST_FAKE_RELATIVE_MOTION_EVENT: SafePtr = SafePtr::new();
static REAL_XTEST_FAKE_MOTION_EVENT: SafePtr = SafePtr::new();

fn debug_log(msg: &str) {
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("/tmp/steamcdp_proxy.log")
        .and_then(|mut f| std::io::Write::write_all(&mut f, format!("[xtst-proxy] {}\n", msg).as_bytes()));
}

fn resolve_real_lib() -> *mut c_void {
    let existing = REAL_XTST.get();
    if !existing.is_null() {
        return existing;
    }

    let handle = unsafe {
        // Try steam-runtime path first
        let home = std::env::var("HOME").unwrap_or_default();
        let rt_path = format!(
            "{}/.steam/steam/ubuntu12_32/steam-runtime/usr/lib/i386-linux-gnu/libXtst.so.6",
            home
        );
        let h = libc::dlopen(
            CString::new(rt_path.as_str()).unwrap().as_ptr(),
            libc::RTLD_LAZY,
        );
        if !h.is_null() {
            debug_log("loaded real libXtst from steam-runtime");
            REAL_XTST.set(h);
            return h;
        }

        // Fallback
        let h2 = libc::dlopen(
            b"libXtst.so.6\0".as_ptr() as *const c_char,
            libc::RTLD_LAZY | libc::RTLD_NOLOAD,
        );
        if !h2.is_null() {
            debug_log("loaded real libXtst via RTLD_NOLOAD");
            REAL_XTST.set(h2);
            return h2;
        }

        let h3 = libc::dlopen(
            b"libXtst.so.6\0".as_ptr() as *const c_char,
            libc::RTLD_LAZY,
        );
        debug_log(&format!("loaded real libXtst via system: {}", !h3.is_null()));
        REAL_XTST.set(h3);
        h3
    };
    handle
}

fn resolve_func<T: Copy>(safe_ptr: &SafePtr, name: &str) -> T {
    let ptr = safe_ptr.get();
    if !ptr.is_null() {
        return unsafe { std::mem::transmute_copy(&ptr) };
    }
    let lib = resolve_real_lib();
    let sym = unsafe {
        libc::dlsym(lib, CString::new(name).unwrap().as_ptr() as *const c_char)
    };
    if sym.is_null() {
        panic!("dlsym failed for {}", name);
    }
    safe_ptr.set(sym);
    unsafe { std::mem::transmute_copy(&sym) }
}

// ---------------------------------------------------------------------------
// XTest pass-through functions
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn XTestFakeButtonEvent(
    display: *mut c_void,
    button: c_ulong,
    is_press: c_int,
    relative_time: c_ulong,
) -> c_int {
    let f: XTestFakeButtonEventFn = resolve_func(&REAL_XTEST_FAKE_BUTTON_EVENT, "XTestFakeButtonEvent");
    f(display, button, is_press, relative_time)
}

#[no_mangle]
pub unsafe extern "C" fn XTestFakeKeyEvent(
    display: *mut c_void,
    keycode: c_ulong,
    is_press: c_int,
    relative_time: c_ulong,
) -> c_int {
    let f: XTestFakeKeyEventFn = resolve_func(&REAL_XTEST_FAKE_KEY_EVENT, "XTestFakeKeyEvent");
    f(display, keycode, is_press, relative_time)
}

#[no_mangle]
pub unsafe extern "C" fn XTestQueryExtension(
    display: *mut c_void,
    major: *mut c_int,
    minor: *mut c_int,
    first: *mut c_int,
    count: *mut c_int,
) -> c_int {
    let f: XTestQueryExtensionFn = resolve_func(&REAL_XTEST_QUERY_EXTENSION, "XTestQueryExtension");
    f(display, major, minor, first, count)
}

#[no_mangle]
pub unsafe extern "C" fn XTestFakeRelativeMotionEvent(
    display: *mut c_void,
    dx: c_int,
    dy: c_int,
    relative_time: c_ulong,
) -> c_int {
    let f: XTestFakeRelativeMotionEventFn = resolve_func(&REAL_XTEST_FAKE_RELATIVE_MOTION_EVENT, "XTestFakeRelativeMotionEvent");
    f(display, dx, dy, relative_time)
}

#[no_mangle]
pub unsafe extern "C" fn XTestFakeMotionEvent(
    display: *mut c_void,
    screen: c_int,
    x: c_int,
    y: c_int,
    relative_time: c_ulong,
) -> c_int {
    let f: XTestFakeMotionEventFn = resolve_func(&REAL_XTEST_FAKE_MOTION_EVENT, "XTestFakeMotionEvent");
    f(display, screen, x, y, relative_time)
}

// ---------------------------------------------------------------------------
// Process identification
// ---------------------------------------------------------------------------

fn get_process_path() -> String {
    unsafe {
        let mut buf = [0u8; 4096];
        let len = libc::readlink(
            b"/proc/self/exe\0".as_ptr() as *const c_char,
            buf.as_mut_ptr() as *mut c_char,
            buf.len(),
        );
        if len > 0 {
            String::from_utf8_lossy(&buf[..len as usize]).into_owned()
        } else {
            String::new()
        }
    }
}

fn is_steam_process() -> bool {
    let exe = get_process_path();
    let home = std::env::var("HOME").unwrap_or_default();
    let expected = format!("{}/.steam/steam/ubuntu12_32/steam", home);

    unsafe {
        let mut exe_buf = [0u8; 4096];
        let mut expected_buf = [0u8; 4096];
        libc::realpath(
            CString::new(exe.as_str()).unwrap().as_ptr(),
            exe_buf.as_mut_ptr() as *mut c_char,
        );
        libc::realpath(
            CString::new(expected.as_str()).unwrap().as_ptr(),
            expected_buf.as_mut_ptr() as *mut c_char,
        );
        let exe_str = CStr::from_ptr(exe_buf.as_ptr() as *const c_char)
            .to_string_lossy()
            .into_owned();
        let expected_str = CStr::from_ptr(expected_buf.as_ptr() as *const c_char)
            .to_string_lossy()
            .into_owned();
        exe_str == expected_str
    }
}

// ---------------------------------------------------------------------------
// Constructor — loads the main LumaForge library
// ---------------------------------------------------------------------------

#[ctor::ctor]
fn xtst_proxy_init() {
    if !is_steam_process() {
        return;
    }

    debug_log("proxy loaded in steam process, loading main library...");

    // Resolve the real libXtst.so.6 first
    let _ = resolve_real_lib();

    // Load the main liblumaforge.so from the same directory
    let exe = get_process_path();
    let dir = std::path::Path::new(&exe)
        .parent()
        .unwrap_or(std::path::Path::new("."));
    let lib_path = dir.join("liblumaforge.so");

    unsafe {
        let handle = libc::dlopen(
            CString::new(lib_path.to_str().unwrap()).unwrap().as_ptr(),
            libc::RTLD_LAZY,
        );
        if handle.is_null() {
            let err = libc::dlerror();
            let err_str = if err.is_null() {
                "unknown error".to_string()
            } else {
                CStr::from_ptr(err).to_string_lossy().into_owned()
            };
            debug_log(&format!("failed to load liblumaforge.so: {}", err_str));
        } else {
            debug_log("liblumaforge.so loaded successfully");
        }
    }
}
