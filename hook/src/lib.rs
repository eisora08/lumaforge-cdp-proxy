use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use std::sync::atomic::{AtomicBool, Ordering};

// ---------------------------------------------------------------------------
// Safe pointer wrapper for statics
// ---------------------------------------------------------------------------

struct SafePtr(std::sync::atomic::AtomicPtr<c_void>);
unsafe impl Send for SafePtr {}
unsafe impl Sync for SafePtr {}
impl SafePtr {
    const fn new() -> Self { SafePtr(std::sync::atomic::AtomicPtr::new(std::ptr::null_mut())) }
    fn get(&self) -> *mut c_void { self.0.load(Ordering::Relaxed) }
    fn set(&self, p: *mut c_void) { self.0.store(p, Ordering::Relaxed); }
}

// ---------------------------------------------------------------------------
// XTest function pointers
// ---------------------------------------------------------------------------

static REAL_XTST: SafePtr = SafePtr::new();
static FN_FAKE_BUTTON: SafePtr = SafePtr::new();
static FN_FAKE_KEY: SafePtr = SafePtr::new();
static FN_QUERY_EXT: SafePtr = SafePtr::new();
static FN_FAKE_REL_MOTION: SafePtr = SafePtr::new();
static FN_FAKE_MOTION: SafePtr = SafePtr::new();

static INIT_DONE: AtomicBool = AtomicBool::new(false);

fn debug_log(msg: &str) {
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("/tmp/steamcdp_proxy.log")
        .and_then(|mut f| std::io::Write::write_all(&mut f, format!("[xtst-proxy] {}\n", msg).as_bytes()));
}

// ---------------------------------------------------------------------------
// XTest pass-through
// ---------------------------------------------------------------------------

fn resolve_real_lib() -> *mut c_void {
    let existing = REAL_XTST.get();
    if !existing.is_null() { return existing; }
    unsafe {
        let home = std::env::var("HOME").unwrap_or_default();
        let rt_path = format!("{}/.steam/steam/ubuntu12_32/steam-runtime/usr/lib/i386-linux-gnu/libXtst.so.6", home);
        let h = libc::dlopen(CString::new(rt_path.as_str()).unwrap().as_ptr(), libc::RTLD_LAZY);
        if !h.is_null() { debug_log("loaded real libXtst from steam-runtime"); REAL_XTST.set(h); return h; }
        let h2 = libc::dlopen(b"libXtst.so.6\0".as_ptr() as *const c_char, libc::RTLD_LAZY | libc::RTLD_NOLOAD);
        if !h2.is_null() { REAL_XTST.set(h2); return h2; }
        let h3 = libc::dlopen(b"libXtst.so.6\0".as_ptr() as *const c_char, libc::RTLD_LAZY);
        REAL_XTST.set(h3);
        h3
    }
}

fn resolve_func(safe_ptr: &SafePtr, name: &str) -> *mut c_void {
    let ptr = safe_ptr.get();
    if !ptr.is_null() { return ptr; }
    let lib = resolve_real_lib();
    let sym = unsafe { libc::dlsym(lib, CString::new(name).unwrap().as_ptr() as *const c_char) };
    if !sym.is_null() { safe_ptr.set(sym); }
    sym
}

macro_rules! xtest_func {
    ($name:ident, $safe_ptr:ident, ($($arg:ident : $aty:ty),*)) => {
        #[no_mangle]
        pub unsafe extern "C" fn $name($($arg: $aty),*) -> c_int {
            let ptr = resolve_func(&$safe_ptr, stringify!($name));
            if ptr.is_null() { return 0; }
            let f: unsafe extern "C" fn($($aty),*) -> c_int = std::mem::transmute(ptr);
            f($($arg),*)
        }
    };
}

xtest_func!(XTestFakeButtonEvent, FN_FAKE_BUTTON, (d: *mut c_void, b: u32, p: c_int, t: u32));
xtest_func!(XTestFakeKeyEvent, FN_FAKE_KEY, (d: *mut c_void, k: u32, p: c_int, t: u32));
xtest_func!(XTestQueryExtension, FN_QUERY_EXT, (d: *mut c_void, a: *mut c_int, b: *mut c_int, c: *mut c_int, e: *mut c_int));
xtest_func!(XTestFakeRelativeMotionEvent, FN_FAKE_REL_MOTION, (d: *mut c_void, x: c_int, y: c_int, t: u32));
xtest_func!(XTestFakeMotionEvent, FN_FAKE_MOTION, (d: *mut c_void, s: c_int, x: c_int, y: c_int, t: u32));

// ---------------------------------------------------------------------------
// Process identification
// ---------------------------------------------------------------------------

fn get_process_path() -> String {
    unsafe {
        let mut buf = [0u8; 4096];
        let len = libc::readlink(b"/proc/self/exe\0".as_ptr() as *const c_char, buf.as_mut_ptr() as *mut c_char, buf.len());
        if len > 0 { String::from_utf8_lossy(&buf[..len as usize]).into_owned() } else { String::new() }
    }
}

fn is_steam_process() -> bool {
    let exe = get_process_path();
    let home = std::env::var("HOME").unwrap_or_default();
    let expected = format!("{}/.steam/steam/ubuntu12_32/steam", home);
    unsafe {
        let mut exe_buf = [0u8; 4096];
        let mut expected_buf = [0u8; 4096];
        libc::realpath(CString::new(exe.as_str()).unwrap().as_ptr(), exe_buf.as_mut_ptr() as *mut c_char);
        libc::realpath(CString::new(expected.as_str()).unwrap().as_ptr(), expected_buf.as_mut_ptr() as *mut c_char);
        CStr::from_ptr(exe_buf.as_ptr() as *const c_char).to_string_lossy() == CStr::from_ptr(expected_buf.as_ptr() as *const c_char).to_string_lossy()
    }
}

// ---------------------------------------------------------------------------
// Constructor
// ---------------------------------------------------------------------------

#[ctor::ctor]
fn xtst_proxy_init() {
    if INIT_DONE.swap(true, Ordering::Relaxed) { return; }
    if !is_steam_process() { return; }

    debug_log("proxy loaded in steam process, loading main library...");

    let _ = resolve_real_lib();

    let exe = get_process_path();
    let dir = std::path::Path::new(&exe).parent().unwrap_or(std::path::Path::new("."));
    let lib_path = dir.join("liblumaforge.so");

    unsafe {
        let handle = libc::dlopen(
            CString::new(lib_path.to_str().unwrap()).unwrap().as_ptr(),
            libc::RTLD_LAZY,
        );
        if handle.is_null() {
            let err = libc::dlerror();
            let err_str = if err.is_null() { "unknown error".to_string() } else { CStr::from_ptr(err).to_string_lossy().into_owned() };
            debug_log(&format!("failed to load liblumaforge.so: {}", err_str));
        } else {
            debug_log("liblumaforge.so loaded successfully");
        }
    }
}
