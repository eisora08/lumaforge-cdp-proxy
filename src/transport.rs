#[cfg(target_os = "windows")]
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use crate::cdp::{CdpClient, Target};
use serde_json::Value;

#[cfg(target_os = "windows")]
use crate::cdp_pipe_win::CdpPipeClient;

#[cfg(target_os = "windows")]
pub const TRANSPORT_NONE: u8 = 0;
#[cfg(target_os = "windows")]
pub const TRANSPORT_TCP: u8 = 1;
#[cfg(target_os = "windows")]
pub const TRANSPORT_PIPE: u8 = 2;

#[cfg(target_os = "windows")]
static CLAIM: AtomicU8 = AtomicU8::new(TRANSPORT_NONE);

/// Read-only: may the TCP loop connect right now? Only when no pipe pairs are
/// registered (Steam adopted a session that never spawned with our pipes) —
/// a live pipe session never falls back to TCP mid-session.
#[cfg(target_os = "windows")]
pub fn tcp_allowed() -> bool {
    crate::hook::cdp_pipe_pairs().is_empty()
}

/// Claim (or re-affirm) the TCP transport. Returns false when the pipe
/// transport owns the session or another transport holds the claim.
/// Registered pipe pairs put the TCP path on standby for the whole session.
#[cfg(target_os = "windows")]
pub fn claim_tcp() -> bool {
    if !tcp_allowed() {
        return false;
    }
    match CLAIM.load(Ordering::SeqCst) {
        TRANSPORT_TCP => true,
        TRANSPORT_NONE => CLAIM
            .compare_exchange(
                TRANSPORT_NONE,
                TRANSPORT_TCP,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok(),
        _ => false,
    }
}

#[cfg(target_os = "windows")]
pub fn claim_pipe() -> bool {
    match CLAIM.load(Ordering::SeqCst) {
        TRANSPORT_PIPE => true,
        TRANSPORT_NONE => CLAIM
            .compare_exchange(
                TRANSPORT_NONE,
                TRANSPORT_PIPE,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok(),
        _ => false,
    }
}

#[cfg(target_os = "windows")]
pub fn release(who: u8) {
    let _ = CLAIM.compare_exchange(who, TRANSPORT_NONE, Ordering::SeqCst, Ordering::SeqCst);
}

#[cfg(target_os = "windows")]
pub fn current() -> u8 {
    CLAIM.load(Ordering::SeqCst)
}

/// The live pipe session, published by the pipe watch loop while it is
/// connected and cleared when the session ends. The relay (cef_hook traffic)
/// and the watch loop both go through this mutex, but only long enough to
/// clone the cheap `CdpPipeClient` handle — waits never happen under it, so
/// the two producers never block each other (Millennium parity: one shared
/// client, no single-flight).
#[cfg(target_os = "windows")]
pub static PIPE_SESSION: std::sync::Mutex<Option<CdpPipeClient>> = std::sync::Mutex::new(None);

/// True while a cef_hook WebSocket is attached to the relay (diagnostic).
#[cfg(target_os = "windows")]
pub static RELAY_WS_CONNECTED: AtomicBool = AtomicBool::new(false);

#[cfg(target_os = "windows")]
pub fn set_pipe_session(client: CdpPipeClient) {
    *PIPE_SESSION.lock().unwrap_or_else(|p| p.into_inner()) = Some(client);
}

#[cfg(target_os = "windows")]
pub fn clear_pipe_session() {
    *PIPE_SESSION.lock().unwrap_or_else(|p| p.into_inner()) = None;
}

/// Clone a handle to the shared pipe session (fails when no session is
/// published). The clone is an `Arc` bump — no CDP traffic happens under the
/// session lock, and queued events are never discarded here.
#[cfg(target_os = "windows")]
pub fn pipe_handle() -> Result<CdpPipeClient, String> {
    let guard = PIPE_SESSION.lock().unwrap_or_else(|p| p.into_inner());
    guard
        .as_ref()
        .cloned()
        .ok_or_else(|| "no pipe session".to_string())
}

/// Borrowed view over a CDP transport with the surface the injector and
/// watch loops need (attach / send / list targets). TCP is a WebSocket per
/// target; PipeShared addresses the shared pipe session.
pub enum Transport<'a> {
    Tcp(&'a mut CdpClient),
    #[cfg(target_os = "windows")]
    PipeShared,
}

impl Transport<'_> {
    pub fn get_targets(&mut self) -> Result<Vec<Target>, String> {
        match self {
            Transport::Tcp(c) => c.get_targets(),
            #[cfg(target_os = "windows")]
            Transport::PipeShared => pipe_handle()?.get_targets(),
        }
    }

    pub fn attach_to_target(&mut self, target_id: &str) -> Result<(), String> {
        match self {
            Transport::Tcp(c) => c.attach_to_target(target_id),
            #[cfg(target_os = "windows")]
            Transport::PipeShared => pipe_handle()?.attach_session(target_id),
        }
    }

    pub fn send_cdp_wait(&mut self, msg: &Value, expected_id: u64) -> Result<Value, String> {
        match self {
            Transport::Tcp(c) => c.send_cdp_wait(msg, expected_id),
            // The pipe client rewrites the id from its shared counter, so the
            // caller's `expected_id` (injector 100+, bridge 9000+) is only a
            // TCP-space value and is ignored here.
            #[cfg(target_os = "windows")]
            Transport::PipeShared => pipe_handle()?.send_session(msg),
        }
    }
}

#[cfg(all(test, target_os = "windows"))]
mod tests {
    use super::*;

    #[test]
    fn claim_state_machine() {
        assert_eq!(current(), TRANSPORT_NONE);
        assert!(claim_tcp());
        assert!(claim_tcp());
        assert!(!claim_pipe());
        release(TRANSPORT_TCP);
        assert_eq!(current(), TRANSPORT_NONE);
        assert!(claim_pipe());
        assert!(claim_pipe());
        assert!(!claim_tcp());
        release(TRANSPORT_PIPE);
        assert_eq!(current(), TRANSPORT_NONE);
        assert!(release_is_noop_when_not_owner());
    }

    fn release_is_noop_when_not_owner() -> bool {
        release(TRANSPORT_TCP);
        current() == TRANSPORT_NONE
    }
}
