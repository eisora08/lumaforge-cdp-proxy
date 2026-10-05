use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use windows_sys::Win32::Foundation::{GetLastError, HANDLE};
use windows_sys::Win32::Storage::FileSystem::{ReadFile, WriteFile};
use windows_sys::Win32::System::Pipes::PeekNamedPipe;

use crate::cdp::Target;
use crate::hook::CdpPipePair;

/// Events queued for the relay. Bounded so a long relay outage cannot grow
/// without limit; oldest are dropped first (logged) — responses are never
/// affected because they are routed straight to their waiter.
// Bursts during a store page load (hundreds of Fetch.requestPaused events)
// arrive faster than cef_hook can answer them; 512 overflowed within seconds
// of a navigation. 4096 (~a few hundred KB of JSON at worst) rides out bursts
// while the relay drains continuously.
const EVENT_QUEUE_CAP: usize = 4096;

type Pending = Arc<Mutex<HashMap<u64, SyncSender<Result<Value, String>>>>>;
type EventQueue = Arc<Mutex<VecDeque<Value>>>;

/// CDP client over the anonymous pipe pair handed to steamwebhelper via
/// `--remote-debugging-io-pipes`. Messages are null-byte delimited JSON,
/// matching Chromium's pipe transport.
///
/// Mirrors Millennium's `cdp_client` (`cdp_api.cc`):
/// - one shared monotonic id space (`next_id`) for every consumer — the watch
///   loop and the relay can no longer collide by sending overlapping ids;
/// - a dedicated reader thread always draining the pipe: responses are
///   routed to their waiter by id (registered *before* the write), events go
///   to a bounded queue nobody can make another consumer skip;
/// - waiting never holds a lock, so a slow page times out only its own call
///   instead of starving every other CDP user.
#[derive(Clone)]
pub struct CdpPipeClient {
    inner: Arc<Inner>,
}

// The write handle is a plain pointer value: it is only ever used under
// `write_lock`. The read handle is not in Inner at all — it lives solely in
// the reader thread.
unsafe impl Send for Inner {}
unsafe impl Sync for Inner {}

// Separate Arcs for the reader (it must not hold `Inner`, or the client could
// never drop and the thread would never stop).
struct Reader {
    parent_read: HANDLE,
    pending: Pending,
    events: EventQueue,
    stop: Arc<AtomicBool>,
}

// The read handle is owned exclusively by the reader thread for its whole
// lifetime; nothing else ever touches it.
unsafe impl Send for Reader {}

struct Inner {
    parent_write: HANDLE,
    generation: u64,
    child_pid: u32,
    next_id: AtomicU64,
    write_lock: Mutex<()>,
    pending: Pending,
    events: EventQueue,
    sessions: Mutex<HashMap<String, String>>,
    current_session: Mutex<Option<String>>,
    current_target: Mutex<Option<String>>,
    stop: Arc<AtomicBool>,
    reader_thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        // Stop the reader, but deliberately do NOT close the pipe handles:
        // the pair lives in the session registry and a re-probe (transient
        // failure, next watch round, next session) must reuse the same open
        // handles — closing them would poison every future probe. The OS
        // closes them when the process exits.
        self.stop.store(true, Ordering::SeqCst);
        let thread = self
            .reader_thread
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        if let Some(handle) = thread {
            let _ = handle.join();
        }
    }
}

impl CdpPipeClient {
    pub fn from_pair(pair: CdpPipePair) -> Self {
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let events: EventQueue = Arc::new(Mutex::new(VecDeque::new()));
        let stop = Arc::new(AtomicBool::new(false));

        let reader = Reader {
            parent_read: pair.parent_read,
            pending: pending.clone(),
            events: events.clone(),
            stop: stop.clone(),
        };
        let thread = std::thread::spawn(move || reader.run());

        CdpPipeClient {
            inner: Arc::new(Inner {
                parent_write: pair.parent_write,
                generation: pair.generation,
                child_pid: pair.child_pid,
                next_id: AtomicU64::new(1),
                write_lock: Mutex::new(()),
                pending,
                events,
                sessions: Mutex::new(HashMap::new()),
                current_session: Mutex::new(None),
                current_target: Mutex::new(None),
                stop,
                reader_thread: Mutex::new(Some(thread)),
            }),
        }
    }

    pub fn generation(&self) -> u64 {
        self.inner.generation
    }

    pub fn child_pid(&self) -> u32 {
        self.inner.child_pid
    }

    /// Register `msg`, write it, and return `(wire_id, response_rx)` without
    /// waiting. The caller polls the receiver later — this is what lets the
    /// relay pipeline commands (Millennium model) instead of blocking one at
    /// a time and starving the event queue.
    pub fn send_begin(
        &self,
        msg: &Value,
    ) -> Result<(u64, std::sync::mpsc::Receiver<Result<Value, String>>), String> {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let mut owned = msg.clone();
        owned["id"] = json!(id);

        let mut payload = owned.to_string().into_bytes();
        payload.push(0);

        let (tx, rx) = sync_channel::<Result<Value, String>>(1);
        self.inner
            .pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(id, tx);

        if let Err(e) = self.write_all(&payload) {
            self.inner
                .pending
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(&id);
            return Err(e);
        }
        Ok((id, rx))
    }

    /// Send `msg` and wait for the response with a fresh id from the shared
    /// counter. The caller's `id` field (if any) is ignored — the wire id is
    /// always allocated here so every producer shares one space.
    pub fn send(&self, msg: &Value, timeout: Duration) -> Result<Value, String> {
        let (id, rx) = self.send_begin(msg)?;
        match rx.recv_timeout(timeout) {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err(e),
            Err(_) => {
                self.inner
                    .pending
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .remove(&id);
                Err(format!("timeout waiting for id={}", id))
            }
        }
    }

    fn write_all(&self, bytes: &[u8]) -> Result<(), String> {
        // Serializes concurrent producers (watch loop + relay) so frames can
        // never interleave mid-write.
        let _guard = self
            .inner
            .write_lock
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let mut total = 0usize;
        while total < bytes.len() {
            let mut written: u32 = 0;
            let ok = unsafe {
                WriteFile(
                    self.inner.parent_write,
                    bytes[total..].as_ptr(),
                    (bytes.len() - total) as u32,
                    &mut written,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 || written == 0 {
                return Err(format!("pipe write failed ({})", unsafe { GetLastError() }));
            }
            total += written as usize;
        }
        Ok(())
    }

    /// Pop every queued event (Fetch.requestPaused and friends) for the relay.
    /// Events are never discarded anywhere else.
    pub fn pop_events(&self) -> Vec<Value> {
        let mut queue = self.inner.events.lock().unwrap_or_else(|p| p.into_inner());
        queue.drain(..).collect()
    }

    pub fn probe(&self, timeout: Duration) -> Result<Value, String> {
        self.send(&json!({"method": "Target.getTargets"}), timeout)
    }

    pub fn get_targets(&self) -> Result<Vec<Target>, String> {
        let resp = self.send(
            &json!({"method": "Target.getTargets"}),
            Duration::from_secs(5),
        )?;
        let mut targets = Vec::new();
        if let Some(infos) = resp
            .get("result")
            .and_then(|r| r.get("targetInfos"))
            .and_then(|a| a.as_array())
        {
            for info in infos {
                targets.push(Target {
                    id: info
                        .get("targetId")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    url: info
                        .get("url")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    target_type: info
                        .get("type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    title: info
                        .get("title")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                });
            }
        }
        Ok(targets)
    }

    pub fn attach_to_target(&self, target_id: &str) -> Result<String, String> {
        if let Some(session_id) = self
            .inner
            .sessions
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(target_id)
            .cloned()
        {
            return Ok(session_id);
        }
        let resp = self.send(
            &json!({
                "method": "Target.attachToTarget",
                "params": {"targetId": target_id, "flatten": true}
            }),
            Duration::from_secs(5),
        )?;
        let session_id = resp
            .get("result")
            .and_then(|r| r.get("sessionId"))
            .and_then(|s| s.as_str())
            .ok_or("no sessionId in attach response")?
            .to_string();
        self.inner
            .sessions
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(target_id.to_string(), session_id.clone());
        Ok(session_id)
    }

    /// Attach with the CdpClient parity contract: Ok(()) when attached to
    /// `target_id` (no-op if already the current target), remembering the
    /// session so session-level commands route correctly.
    pub fn attach_session(&self, target_id: &str) -> Result<(), String> {
        {
            let cur = self
                .inner
                .current_target
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            if cur.as_deref() == Some(target_id) {
                return Ok(());
            }
        }
        let session_id = self.attach_to_target(target_id)?;
        *self
            .inner
            .current_session
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = Some(session_id);
        *self
            .inner
            .current_target
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = Some(target_id.to_string());
        Ok(())
    }

    /// send_cdp_wait parity: injects the current sessionId into session-level
    /// commands (browser-level Target./Browser. commands stay bare), 5s
    /// response timeout like the TCP client. The message's `id` field is
    /// replaced by the shared counter.
    pub fn send_session(&self, msg: &Value) -> Result<Value, String> {
        let mut owned = msg.clone();
        let method = owned
            .get("method")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .to_string();
        let browser_level = method.starts_with("Target.") || method.starts_with("Browser.");
        if !browser_level && owned.get("sessionId").is_none() {
            let cur = self
                .inner
                .current_session
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            match cur.clone() {
                Some(s) => owned["sessionId"] = serde_json::Value::String(s),
                None => return Err("no CDP session attached".to_string()),
            }
        }
        self.send(&owned, Duration::from_secs(5))
    }

    pub fn session_for(&self, target_id: &str) -> Option<String> {
        self.inner
            .sessions
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(target_id)
            .cloned()
    }

    #[cfg(test)]
    fn pending_count(&self) -> usize {
        self.inner
            .pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .len()
    }
}

impl Reader {
    fn run(self) {
        let mut buffer: Vec<u8> = Vec::new();
        let mut dropped_events: u64 = 0;
        while !self.stop.load(Ordering::Relaxed) {
            let mut avail: u32 = 0;
            let ok = unsafe {
                PeekNamedPipe(
                    self.parent_read,
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null_mut(),
                    &mut avail,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                let err = unsafe { GetLastError() };
                crate::log_to_temp(&format!(
                    "[pipe] Reader pipe error ({}), failing in-flight requests",
                    err
                ));
                self.fail_all(&format!("pipe read failed ({})", err));
                return;
            }
            if avail == 0 {
                std::thread::sleep(Duration::from_millis(5));
                continue;
            }
            let mut chunk = vec![0u8; avail as usize];
            let mut read: u32 = 0;
            let ok = unsafe {
                ReadFile(
                    self.parent_read,
                    chunk.as_mut_ptr(),
                    avail,
                    &mut read,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                let err = unsafe { GetLastError() };
                crate::log_to_temp(&format!(
                    "[pipe] Reader read failed ({}), failing in-flight requests",
                    err
                ));
                self.fail_all(&format!("pipe read failed ({})", err));
                return;
            }
            buffer.extend_from_slice(&chunk[..read as usize]);

            while let Some(pos) = buffer.iter().position(|&b| b == 0) {
                let raw: Vec<u8> = buffer.drain(..=pos).collect();
                let text = String::from_utf8_lossy(&raw[..pos]);
                if text.trim().is_empty() {
                    continue;
                }
                let msg: Value = match serde_json::from_str(&text) {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                if let Some(id) = msg.get("id").and_then(|v| v.as_u64()) {
                    let waiter = self
                        .pending
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .remove(&id);
                    match waiter {
                        Some(tx) => {
                            let _ = tx.send(Ok(msg));
                        }
                        None => {
                            // Only reachable for a response that outlived its
                            // waiter's timeout (ids are never reused).
                            crate::log_to_temp(&format!(
                                "[pipe] Response for unknown/late id {}",
                                id
                            ));
                        }
                    }
                } else if msg.get("method").is_some() {
                    let mut queue = self.events.lock().unwrap_or_else(|p| p.into_inner());
                    if queue.len() >= EVENT_QUEUE_CAP {
                        queue.pop_front();
                        dropped_events += 1;
                        if dropped_events % 64 == 1 {
                            crate::log_to_temp(&format!(
                                "[pipe] Event queue full, dropping oldest ({} dropped this session)",
                                dropped_events
                            ));
                        }
                    }
                    queue.push_back(msg);
                }
            }
        }
    }

    fn fail_all(&self, err: &str) {
        let mut pending = self.pending.lock().unwrap_or_else(|p| p.into_inner());
        for (_, tx) in pending.drain() {
            let _ = tx.send(Err(err.to_string()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    use windows_sys::Win32::System::Pipes::CreatePipe;

    fn spawn_helper<F>(child_read: HANDLE, child_write: HANDLE, f: F) -> thread::JoinHandle<()>
    where
        F: FnOnce(HANDLE, HANDLE) + Send + 'static,
    {
        let r = child_read as usize;
        let w = child_write as usize;
        thread::spawn(move || unsafe { f(r as HANDLE, w as HANDLE) })
    }

    fn make_pair() -> (CdpPipePair, HANDLE, HANDLE) {
        unsafe {
            let sa = SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: std::ptr::null_mut(),
                bInheritHandle: 1,
            };
            let mut child_read: HANDLE = std::ptr::null_mut();
            let mut parent_write: HANDLE = std::ptr::null_mut();
            assert_ne!(CreatePipe(&mut child_read, &mut parent_write, &sa, 0), 0);
            let mut parent_read: HANDLE = std::ptr::null_mut();
            let mut child_write: HANDLE = std::ptr::null_mut();
            assert_ne!(CreatePipe(&mut parent_read, &mut child_write, &sa, 0), 0);
            let pair = CdpPipePair {
                parent_read,
                parent_write,
                child_pid: 42,
                generation: 7,
            };
            (pair, child_read, child_write)
        }
    }

    unsafe fn child_read_message(handle: HANDLE) -> String {
        let mut buf: Vec<u8> = Vec::new();
        loop {
            let mut chunk = [0u8; 4096];
            let mut read: u32 = 0;
            assert_ne!(
                ReadFile(
                    handle,
                    chunk.as_mut_ptr(),
                    chunk.len() as u32,
                    &mut read,
                    std::ptr::null_mut()
                ),
                0
            );
            assert!(read > 0, "pipe closed before full message");
            buf.extend_from_slice(&chunk[..read as usize]);
            if let Some(pos) = buf.iter().position(|&b| b == 0) {
                return String::from_utf8_lossy(&buf[..pos]).to_string();
            }
        }
    }

    unsafe fn child_write_bytes(handle: HANDLE, bytes: &[u8]) {
        let mut total = 0usize;
        while total < bytes.len() {
            let mut written: u32 = 0;
            assert_ne!(
                WriteFile(
                    handle,
                    bytes[total..].as_ptr(),
                    (bytes.len() - total) as u32,
                    &mut written,
                    std::ptr::null_mut(),
                ),
                0
            );
            total += written as usize;
        }
    }

    unsafe fn child_write_raw(handle: HANDLE, text: &str) {
        let mut bytes: Vec<u8> = text.as_bytes().to_vec();
        bytes.push(0);
        child_write_bytes(handle, &bytes);
    }

    #[test]
    fn roundtrip_request_response() {
        let (pair, mut child_read, child_write) = make_pair();
        let helper = spawn_helper(child_read, child_write, |child_read, child_write| unsafe {
            let cmd = child_read_message(child_read);
            let msg: Value = serde_json::from_str(&cmd).unwrap();
            let id = msg.get("id").and_then(|v| v.as_u64()).unwrap();
            child_write_raw(
                child_write,
                &json!({"id": id, "result": {"targetInfos": []}}).to_string(),
            );
            CloseHandle(child_read);
            CloseHandle(child_write);
        });

        let client = CdpPipeClient::from_pair(pair);
        let resp = client
            .send(
                &json!({"method": "Target.getTargets"}),
                Duration::from_secs(5),
            )
            .unwrap();
        assert_eq!(
            resp.get("result").and_then(|r| r.get("targetInfos")),
            Some(&json!([]))
        );
        helper.join().unwrap();
    }

    #[test]
    fn interleaved_event_is_queued_not_dropped() {
        let (pair, mut child_read, child_write) = make_pair();
        let helper = spawn_helper(child_read, child_write, |child_read, child_write| unsafe {
            let cmd = child_read_message(child_read);
            let msg: Value = serde_json::from_str(&cmd).unwrap();
            let id = msg.get("id").and_then(|v| v.as_u64()).unwrap();
            child_write_raw(child_write, &json!({"method": "Target.targetCreated", "params": {"targetInfo": {"targetId": "t1"}}}).to_string());
            child_write_raw(
                child_write,
                &json!({"id": id, "result": {"ok": true}}).to_string(),
            );
            CloseHandle(child_read);
            CloseHandle(child_write);
        });

        let client = CdpPipeClient::from_pair(pair);
        let resp = client
            .send(&json!({"method": "X"}), Duration::from_secs(5))
            .unwrap();
        assert_eq!(
            resp.get("result").and_then(|r| r.get("ok")),
            Some(&json!(true))
        );
        // The event must be waiting for the relay even though nothing asked
        // for it while the response was in flight.
        let mut events = client.pop_events();
        assert_eq!(events.len(), 1);
        assert_eq!(
            events.remove(0).get("method").and_then(|m| m.as_str()),
            Some("Target.targetCreated")
        );
        helper.join().unwrap();
    }

    #[test]
    fn partial_frames_are_reassembled() {
        let (pair, mut child_read, child_write) = make_pair();
        let helper = spawn_helper(child_read, child_write, |child_read, child_write| unsafe {
            let cmd = child_read_message(child_read);
            let msg: Value = serde_json::from_str(&cmd).unwrap();
            let id = msg.get("id").and_then(|v| v.as_u64()).unwrap();
            let full = json!({"id": id, "result": {"value": "hello"}}).to_string();
            let split = full.len() / 2;
            child_write_bytes(child_write, full[..split].as_bytes());
            std::thread::sleep(Duration::from_millis(50));
            child_write_raw(child_write, &full[split..]);
            CloseHandle(child_read);
            CloseHandle(child_write);
        });

        let client = CdpPipeClient::from_pair(pair);
        let resp = client
            .send(&json!({"method": "X"}), Duration::from_secs(5))
            .unwrap();
        assert_eq!(
            resp.get("result").and_then(|r| r.get("value")),
            Some(&json!("hello"))
        );
        helper.join().unwrap();
    }

    #[test]
    fn response_routes_to_its_own_waiter_not_first_available() {
        // The helper answers out of order: first the late/wrong id, then the
        // real one. The client must fulfill the wait with ITS response and
        // must never hand another request's answer to a waiting caller —
        // that cross-claiming was the root cause of "timeout waiting for
        // id=100" in the pipe-only session.
        let (pair, mut child_read, child_write) = make_pair();
        let helper = spawn_helper(child_read, child_write, |child_read, child_write| unsafe {
            let cmd = child_read_message(child_read);
            let msg: Value = serde_json::from_str(&cmd).unwrap();
            let id = msg.get("id").and_then(|v| v.as_u64()).unwrap();
            child_write_raw(
                child_write,
                &json!({"id": id + 5000, "result": {"wrong": true}}).to_string(),
            );
            child_write_raw(
                child_write,
                &json!({"id": id, "result": {"mine": true}}).to_string(),
            );
            CloseHandle(child_read);
            CloseHandle(child_write);
        });

        let client = CdpPipeClient::from_pair(pair);
        let resp = client
            .send(&json!({"method": "X"}), Duration::from_secs(5))
            .unwrap();
        assert_eq!(
            resp.get("result").and_then(|r| r.get("mine")),
            Some(&json!(true)),
            "waiter must receive its own response, got: {}",
            resp
        );
        helper.join().unwrap();
    }

    #[test]
    fn concurrent_senders_get_unique_ids() {
        let (pair, mut child_read, child_write) = make_pair();
        // Echo server that tolerates coalesced frames: concurrent writers can
        // land several messages in one ReadFile, so the buffer must persist
        // across messages (child_read_message would discard the tail).
        let helper = spawn_helper(child_read, child_write, |child_read, child_write| unsafe {
            let mut buf: Vec<u8> = Vec::new();
            let mut handled = 0usize;
            while handled < 8 {
                let mut chunk = [0u8; 4096];
                let mut read: u32 = 0;
                assert_ne!(
                    ReadFile(
                        child_read,
                        chunk.as_mut_ptr(),
                        chunk.len() as u32,
                        &mut read,
                        std::ptr::null_mut()
                    ),
                    0
                );
                assert!(read > 0, "pipe closed before full message");
                buf.extend_from_slice(&chunk[..read as usize]);
                while let Some(pos) = buf.iter().position(|&b| b == 0) {
                    let raw: Vec<u8> = buf.drain(..=pos).collect();
                    let text = String::from_utf8_lossy(&raw[..pos]).to_string();
                    if text.trim().is_empty() {
                        continue;
                    }
                    let msg: Value = serde_json::from_str(&text).unwrap();
                    let id = msg.get("id").and_then(|v| v.as_u64()).unwrap();
                    child_write_raw(
                        child_write,
                        &json!({"id": id, "result": {"ok": true}}).to_string(),
                    );
                    handled += 1;
                }
            }
            CloseHandle(child_read);
            CloseHandle(child_write);
        });

        let client = CdpPipeClient::from_pair(pair);
        let seen: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));
        let mut handles = Vec::new();
        for n in 0..4u32 {
            let c = client.clone();
            let seen = seen.clone();
            handles.push(thread::spawn(move || {
                for i in 0..2u32 {
                    let resp = c
                        .send(
                            &json!({"method": format!("M{}-{}", n, i)}),
                            Duration::from_secs(5),
                        )
                        .unwrap();
                    assert_eq!(
                        resp.get("result").and_then(|r| r.get("ok")),
                        Some(&json!(true))
                    );
                    seen.lock()
                        .unwrap()
                        .push(resp.get("id").and_then(|v| v.as_u64()).unwrap());
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let ids = seen.lock().unwrap();
        assert_eq!(ids.len(), 8);
        let unique: std::collections::HashSet<u64> = ids.iter().copied().collect();
        assert_eq!(unique.len(), 8, "wire ids must never repeat: {:?}", ids);
        helper.join().unwrap();
    }

    #[test]
    fn events_survive_without_a_consumer() {
        let (pair, mut child_read, child_write) = make_pair();
        let helper = spawn_helper(child_read, child_write, |child_read, child_write| unsafe {
            let cmd = child_read_message(child_read);
            let msg: Value = serde_json::from_str(&cmd).unwrap();
            let id = msg.get("id").and_then(|v| v.as_u64()).unwrap();
            // Three events arrive while nothing drains them, then the answer.
            for n in 0..3 {
                child_write_raw(
                    child_write,
                    &json!({"method": "Fetch.requestPaused", "params": {"n": n}}).to_string(),
                );
            }
            child_write_raw(
                child_write,
                &json!({"id": id, "result": {"ok": true}}).to_string(),
            );
            CloseHandle(child_read);
            CloseHandle(child_write);
        });

        let client = CdpPipeClient::from_pair(pair);
        client
            .send(&json!({"method": "X"}), Duration::from_secs(5))
            .unwrap();
        // No pop_events happened during the exchange: nothing was discarded.
        let events = client.pop_events();
        assert_eq!(events.len(), 3);
        helper.join().unwrap();
    }

    #[test]
    fn attach_caches_session() {
        let (pair, mut child_read, child_write) = make_pair();
        let helper = spawn_helper(child_read, child_write, |child_read, child_write| unsafe {
            let cmd = child_read_message(child_read);
            let msg: Value = serde_json::from_str(&cmd).unwrap();
            let id = msg.get("id").and_then(|v| v.as_u64()).unwrap();
            child_write_raw(
                child_write,
                &json!({"id": id, "result": {"sessionId": "S-1"}}).to_string(),
            );
            CloseHandle(child_read);
            CloseHandle(child_write);
        });

        let client = CdpPipeClient::from_pair(pair);
        let s1 = client.attach_to_target("target-1").unwrap();
        assert_eq!(s1, "S-1");
        let s2 = client.attach_to_target("target-1").unwrap();
        assert_eq!(s2, "S-1");
        assert_eq!(client.session_for("target-1"), Some("S-1".to_string()));
        helper.join().unwrap();
    }

    #[test]
    fn session_commands_get_session_id_injected() {
        let (pair, mut child_read, child_write) = make_pair();
        let helper = spawn_helper(child_read, child_write, |child_read, child_write| unsafe {
            let cmd = child_read_message(child_read);
            let msg: Value = serde_json::from_str(&cmd).unwrap();
            let id = msg.get("id").and_then(|v| v.as_u64()).unwrap();
            child_write_raw(
                child_write,
                &json!({"id": id, "result": {"sessionId": "S-9"}}).to_string(),
            );

            let cmd2 = child_read_message(child_read);
            let msg2: Value = serde_json::from_str(&cmd2).unwrap();
            let id2 = msg2.get("id").and_then(|v| v.as_u64()).unwrap();
            assert_eq!(msg2.get("sessionId").and_then(|s| s.as_str()), Some("S-9"));
            assert_eq!(
                msg2.get("method").and_then(|m| m.as_str()),
                Some("Runtime.evaluate")
            );
            child_write_raw(
                child_write,
                &json!({"id": id2, "result": {"result": {"value": 1}}}).to_string(),
            );

            let cmd3 = child_read_message(child_read);
            let msg3: Value = serde_json::from_str(&cmd3).unwrap();
            let id3 = msg3.get("id").and_then(|v| v.as_u64()).unwrap();
            assert_eq!(
                msg3.get("method").and_then(|m| m.as_str()),
                Some("Target.getTargets")
            );
            assert!(msg3.get("sessionId").is_none());
            child_write_raw(
                child_write,
                &json!({"id": id3, "result": {"targetInfos": []}}).to_string(),
            );
            CloseHandle(child_read);
            CloseHandle(child_write);
        });

        let client = CdpPipeClient::from_pair(pair);
        client.attach_session("t1").unwrap();
        let resp = client
            .send_session(&json!({"method": "Runtime.evaluate", "params": {"expression": "1"}}))
            .unwrap();
        assert_eq!(
            resp.get("result").and_then(|r| r.get("result")),
            Some(&json!({"value": 1}))
        );
        let resp2 = client
            .send_session(&json!({"method": "Target.getTargets"}))
            .unwrap();
        assert!(resp2.get("result").is_some());
        helper.join().unwrap();
    }

    #[test]
    fn timeout_reports_error() {
        let (pair, _child_read, _child_write) = make_pair();
        let client = CdpPipeClient::from_pair(pair);
        let err = client
            .send(&json!({"method": "X"}), Duration::from_millis(80))
            .unwrap_err();
        assert!(err.contains("timeout"), "unexpected error: {}", err);
        // The timed-out entry must be gone, otherwise a late response would
        // log as unknown and the map would grow forever.
        assert_eq!(client.pending_count(), 0);
    }
}
