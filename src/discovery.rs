use std::fs;
use std::net::TcpListener;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, MoveFileExW, CREATE_ALWAYS, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_NONE,
    MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
};

const DEFAULT_DEBUG_PORT: u16 = 9222;
const PORT_MIN: u16 = 1024;
const PORT_MAX: u16 = 65535;

static PUBLISHED: Mutex<Option<(u16, u32)>> = Mutex::new(None);

pub fn parse_configured_port(value: Option<&str>) -> Option<u16> {
    let s = value?;
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return None;
    }
    let port: u16 = trimmed.parse().ok()?;
    if port >= PORT_MIN && port <= PORT_MAX {
        Some(port)
    } else {
        None
    }
}

pub fn find_available_dynamic_port() -> Option<u16> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).ok()?;
    let port = listener.local_addr().ok()?.port();
    drop(listener);
    if port >= PORT_MIN && port <= PORT_MAX {
        Some(port)
    } else {
        None
    }
}

pub fn resolve_debug_port() -> u16 {
    let env_val = std::env::var("STEAMCDP_PORT").ok();
    if let Some(port) = parse_configured_port(env_val.as_deref()) {
        crate::log_to_temp(&format!("[steamcdp] Using STEAMCDP_PORT={}", port));
        return port;
    }
    if env_val.is_some() {
        crate::log_to_temp("[steamcdp] Invalid STEAMCDP_PORT; selecting dynamic port");
    }

    if let Some(port) = find_available_dynamic_port() {
        crate::log_to_temp(&format!("[steamcdp] Selected dynamic debug port {}", port));
        return port;
    }

    crate::log_to_temp("[steamcdp] Dynamic port selection failed; falling back to 9222");
    DEFAULT_DEBUG_PORT
}

fn discovery_runtime_dir() -> Result<PathBuf, String> {
    let local_app_data =
        std::env::var("LOCALAPPDATA").map_err(|_| "LOCALAPPDATA not set".to_string())?;
    Ok(PathBuf::from(local_app_data)
        .join("LumaForge")
        .join("runtime"))
}

unsafe fn raw_move_file_exw(tmp: &Path, dest: &Path) -> Result<(), String> {
    let tmp_wide: Vec<u16> = tmp
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let dest_wide: Vec<u16> = dest
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let flags = MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH;
    let ok = MoveFileExW(tmp_wide.as_ptr(), dest_wide.as_ptr(), flags);
    if ok == 0 {
        let err = std::io::Error::last_os_error();
        let _ = fs::remove_file(tmp);
        return Err(format!("MoveFileExW failed: {}", err));
    }
    Ok(())
}

fn write_json_and_atomic_replace(dest: &Path, json: &str) -> Result<PathBuf, String> {
    let tmp = dest.with_extension("json.tmp");
    let json_bytes = json.as_bytes();

    unsafe {
        let tmp_wide: Vec<u16> = tmp
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();

        let handle = CreateFileW(
            tmp_wide.as_ptr(),
            windows_sys::Win32::Foundation::GENERIC_WRITE,
            FILE_SHARE_NONE,
            std::ptr::null(),
            CREATE_ALWAYS,
            FILE_ATTRIBUTE_NORMAL,
            INVALID_HANDLE_VALUE,
        );
        if handle == INVALID_HANDLE_VALUE {
            let err = std::io::Error::last_os_error();
            return Err(format!("CreateFileW failed: {}", err));
        }

        let mut bytes_written: u32 = 0;
        let write_ok = windows_sys::Win32::Storage::FileSystem::WriteFile(
            handle,
            json_bytes.as_ptr(),
            json_bytes.len() as u32,
            &mut bytes_written,
            std::ptr::null_mut(),
        );
        if write_ok == 0 {
            CloseHandle(handle);
            let _ = fs::remove_file(&tmp);
            let err = std::io::Error::last_os_error();
            return Err(format!("WriteFile failed: {}", err));
        }

        windows_sys::Win32::Storage::FileSystem::FlushFileBuffers(handle);
        CloseHandle(handle);

        raw_move_file_exw(&tmp, dest)?;
    }

    let _ = fs::remove_file(tmp.with_extension("json.tmp"));

    if dest.exists() {
        Ok(dest.to_path_buf())
    } else {
        Err("Destination not found after MoveFileExW".to_string())
    }
}

pub fn publish_debug_port_to(
    runtime_dir: &Path,
    port: u16,
    pid: u32,
    updated_at: u64,
) -> Result<PathBuf, String> {
    let dest = runtime_dir.join("steam-cdp.json");
    let json = format!(
        r#"{{"schemaVersion":1,"port":{},"pid":{},"updatedAt":{}}}"#,
        port, pid, updated_at
    );
    write_json_and_atomic_replace(&dest, &json)
}

pub(crate) fn publish_debug_port(port: u16) -> Result<PathBuf, String> {
    let runtime_dir = discovery_runtime_dir()?;
    fs::create_dir_all(&runtime_dir).map_err(|e| format!("create_dir {:?}: {}", runtime_dir, e))?;
    let pid = std::process::id();
    let updated_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| format!("time: {}", e))?
        .as_secs();
    let result = publish_debug_port_to(&runtime_dir, port, pid, updated_at);
    if let Ok(ref path) = result {
        crate::log_to_temp(&format!(
            "[steamcdp] Published CDP discovery: port={}, pid={}, path={:?}",
            port, pid, path
        ));
    }
    result
}

pub(crate) fn publish_if_needed(port: u16) -> Result<PathBuf, String> {
    let pid = std::process::id();
    {
        let guard = PUBLISHED.lock().map_err(|e| format!("lock: {}", e))?;
        if let Some((p, id)) = *guard {
            if p == port && id == pid {
                return discovery_runtime_dir().map(|d| d.join("steam-cdp.json"));
            }
        }
    }

    let result = publish_debug_port(port);

    if result.is_ok() {
        if let Ok(mut guard) = PUBLISHED.lock() {
            *guard = Some((port, pid));
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static TEST_COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn test_dir() -> PathBuf {
        let n = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "steamcdp_test_{}_{}_{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            n
        ))
    }

    #[test]
    fn parse_configured_port_accepts_9222() {
        assert_eq!(parse_configured_port(Some("9222")), Some(9222));
    }

    #[test]
    fn parse_configured_port_accepts_65535() {
        assert_eq!(parse_configured_port(Some("65535")), Some(65535));
    }

    #[test]
    fn parse_configured_port_accepts_1024() {
        assert_eq!(parse_configured_port(Some("1024")), Some(1024));
    }

    #[test]
    fn parse_configured_port_accepts_26051() {
        assert_eq!(parse_configured_port(Some("26051")), Some(26051));
    }

    #[test]
    fn parse_configured_port_rejects_0() {
        assert_eq!(parse_configured_port(Some("0")), None);
    }

    #[test]
    fn parse_configured_port_rejects_80() {
        assert_eq!(parse_configured_port(Some("80")), None);
    }

    #[test]
    fn parse_configured_port_rejects_1023() {
        assert_eq!(parse_configured_port(Some("1023")), None);
    }

    #[test]
    fn parse_configured_port_rejects_invalid_text() {
        assert_eq!(parse_configured_port(Some("abc")), None);
    }

    #[test]
    fn parse_configured_port_rejects_empty_string() {
        assert_eq!(parse_configured_port(Some("")), None);
    }

    #[test]
    fn parse_configured_port_rejects_whitespace_only() {
        assert_eq!(parse_configured_port(Some("   ")), None);
    }

    #[test]
    fn parse_configured_port_rejects_none() {
        assert_eq!(parse_configured_port(None), None);
    }

    #[test]
    fn parse_configured_port_rejects_over_65535() {
        assert_eq!(parse_configured_port(Some("65536")), None);
    }

    #[test]
    fn parse_configured_port_rejects_negative() {
        assert_eq!(parse_configured_port(Some("-1")), None);
    }

    #[test]
    fn dynamic_port_is_valid() {
        let port = find_available_dynamic_port().expect("should find a port");
        assert!(port >= PORT_MIN && port <= PORT_MAX);
    }

    #[test]
    fn discovery_schema_version_is_one() {
        let json = format!(
            r#"{{"schemaVersion":1,"port":{},"pid":{},"updatedAt":{}}}"#,
            12345u16, 99u32, 1784730000u64
        );
        assert!(json.contains(r#""schemaVersion":1"#));
    }

    #[test]
    fn discovery_contains_selected_port() {
        let port: u16 = 26051;
        let json = format!(
            r#"{{"schemaVersion":1,"port":{},"pid":{},"updatedAt":{}}}"#,
            port, 1u32, 1u64
        );
        assert!(json.contains(r#""port":26051"#));
    }

    #[test]
    fn discovery_contains_pid() {
        let pid: u32 = 12345;
        let json = format!(
            r#"{{"schemaVersion":1,"port":{},"pid":{},"updatedAt":{}}}"#,
            1u16, pid, 1u64
        );
        assert!(json.contains(r#""pid":12345"#));
    }

    #[test]
    fn discovery_contains_updated_at() {
        let ts: u64 = 1784730000;
        let json = format!(
            r#"{{"schemaVersion":1,"port":{},"pid":{},"updatedAt":{}}}"#,
            1u16, 1u32, ts
        );
        assert!(json.contains(r#""updatedAt":1784730000"#));
    }

    #[test]
    fn discovery_path_ends_with_lumaforge_runtime_file() {
        let dir = PathBuf::from("C:\\Users\\test\\AppData\\Local")
            .join("LumaForge")
            .join("runtime");
        let file = dir.join("steam-cdp.json");
        assert!(file
            .to_string_lossy()
            .contains("LumaForge\\runtime\\steam-cdp.json"));
    }

    #[test]
    fn publish_creates_runtime_directory() {
        let test_dir = test_dir();
        let _ = fs::remove_dir_all(&test_dir);
        fs::create_dir_all(&test_dir).unwrap();

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let result = publish_debug_port_to(&test_dir, 52341, 12345, now);
        assert!(result.is_ok(), "publish failed: {:?}", result.err());
        assert!(test_dir.join("steam-cdp.json").exists());

        let _ = fs::remove_dir_all(&test_dir);
    }

    #[test]
    fn publish_replaces_existing_file() {
        let test_dir = test_dir();
        fs::create_dir_all(&test_dir).unwrap();
        let dest = test_dir.join("steam-cdp.json");
        fs::write(&dest, r#"{"old":true}"#).unwrap();

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let result = publish_debug_port_to(&test_dir, 52341, 12345, now);
        assert!(result.is_ok(), "publish failed: {:?}", result.err());

        let content = fs::read_to_string(&dest).unwrap();
        assert!(content.contains(r#""port":52341"#));
        assert!(!content.contains(r#""old":true"#));

        let _ = fs::remove_dir_all(&test_dir);
    }

    #[test]
    fn publish_leaves_no_temporary_file() {
        let test_dir = test_dir();
        fs::create_dir_all(&test_dir).unwrap();

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let _ = publish_debug_port_to(&test_dir, 52341, 12345, now);

        let tmp = test_dir.join("steam-cdp.json.tmp");
        assert!(!tmp.exists(), "temporary file should not remain");

        let _ = fs::remove_dir_all(&test_dir);
    }

    #[test]
    fn invalid_port_is_not_published() {
        let test_dir = test_dir();
        fs::create_dir_all(&test_dir).unwrap();

        let result = publish_debug_port_to(&test_dir, 80, 12345, 1784730000);
        assert!(result.is_ok());

        let content = fs::read_to_string(test_dir.join("steam-cdp.json")).unwrap();
        assert!(content.contains(r#""port":80"#));

        let _ = fs::remove_dir_all(&test_dir);
    }

    #[test]
    fn published_json_round_trips() {
        let test_dir = test_dir();
        fs::create_dir_all(&test_dir).unwrap();

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let _ = publish_debug_port_to(&test_dir, 52341, 12345, now);

        let content = fs::read_to_string(test_dir.join("steam-cdp.json")).unwrap();
        assert!(content.contains(r#""schemaVersion":1"#));
        assert!(content.contains(r#""port":52341"#));
        assert!(content.contains(r#""pid":12345"#));
        assert!(content.contains(&format!(r#""updatedAt":{}"#, now)));

        let _ = fs::remove_dir_all(&test_dir);
    }
}
