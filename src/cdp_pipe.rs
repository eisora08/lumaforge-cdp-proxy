use serde_json::{json, Value};
use std::collections::HashMap;
use std::os::unix::io::RawFd;

#[derive(Debug, Clone)]
pub struct Target {
    pub id: String,
    pub url: String,
    pub target_type: String,
    pub title: String,
}

/// CDP client that communicates over Unix pipe FDs (null-byte delimited JSON).
/// This is used inside pressure-vessel containers where TCP is unreliable.
pub struct CdpPipeClient {
    write_fd: RawFd,
    read_fd: RawFd,
    buffer: String,
    next_id: u64,
    /// Track session IDs for attached targets
    sessions: HashMap<String, String>,
}

impl CdpPipeClient {
    /// Create a client from pre-opened pipe file descriptors.
    /// write_fd: FD to send CDP commands (host -> child)
    /// read_fd: FD to receive CDP responses (child -> host)
    pub fn new(write_fd: RawFd, read_fd: RawFd) -> Self {
        CdpPipeClient {
            write_fd,
            read_fd,
            buffer: String::new(),
            next_id: 1,
            sessions: HashMap::new(),
        }
    }

    /// Send a raw CDP message over the pipe.
    fn pipe_send(&mut self, msg: &str) -> Result<(), String> {
        let payload = format!("{}\0", msg);
        let bytes = payload.as_bytes();
        let mut written = 0;
        while written < bytes.len() {
            let n = unsafe {
                libc::write(
                    self.write_fd,
                    bytes[written..].as_ptr() as *const libc::c_void,
                    bytes.len() - written,
                )
            };
            if n < 0 {
                return Err(format!("pipe write: {}", std::io::Error::last_os_error()));
            }
            written += n as usize;
        }
        Ok(())
    }

    /// Read a complete CDP message from the pipe (blocking with timeout).
    fn pipe_read_message(&mut self, timeout_ms: u64) -> Result<String, String> {
        let deadline = std::time::Instant::now()
            + std::time::Duration::from_millis(timeout_ms);

        let flags = unsafe { libc::fcntl(self.read_fd, libc::F_GETFL) };
        unsafe {
            libc::fcntl(self.read_fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }

        let mut chunk = [0u8; 4096];
        loop {
            if std::time::Instant::now() > deadline {
                unsafe { libc::fcntl(self.read_fd, libc::F_SETFL, flags); }
                return Err("timeout".to_string());
            }

            let mut pollfd = libc::pollfd {
                fd: self.read_fd,
                events: libc::POLLIN,
                revents: 0,
            };
            let n = unsafe { libc::poll(&mut pollfd, 1, 50) };
            if n <= 0 { continue; }

            let n = unsafe {
                libc::read(
                    self.read_fd,
                    chunk.as_mut_ptr() as *mut libc::c_void,
                    chunk.len(),
                )
            };
            if n < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::WouldBlock { continue; }
                unsafe { libc::fcntl(self.read_fd, libc::F_SETFL, flags); }
                return Err(format!("pipe read: {}", err));
            }
            if n == 0 {
                unsafe { libc::fcntl(self.read_fd, libc::F_SETFL, flags); }
                return Err("pipe closed".to_string());
            }

            self.buffer.push_str(&String::from_utf8_lossy(&chunk[..n as usize]));

            if let Some(pos) = self.buffer.find('\0') {
                let msg = self.buffer[..pos].to_string();
                self.buffer.drain(..=pos);
                if !msg.is_empty() {
                    unsafe { libc::fcntl(self.read_fd, libc::F_SETFL, flags); }
                    return Ok(msg);
                }
            }
        }
    }

    /// Send a CDP command and wait for the response with matching id.
    pub fn send_cdp_wait(&mut self, msg: &Value, expected_id: u64) -> Result<Value, String> {
        self.send_cdp(msg)?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if std::time::Instant::now() > deadline {
                return Err(format!("timeout waiting for id={}", expected_id));
            }
            let remaining = deadline
                .duration_since(std::time::Instant::now())
                .as_millis() as u64;
            match self.pipe_read_message(remaining.min(200)) {
                Ok(text) => {
                    if let Ok(msg) = serde_json::from_str::<Value>(&text) {
                        if msg.get("id").and_then(|i| i.as_u64()) == Some(expected_id) {
                            return Ok(msg);
                        }
                    }
                }
                Err(ref e) if e == "timeout" => continue,
                Err(e) => return Err(e),
            }
        }
    }

    pub fn send_cdp(&mut self, msg: &Value) -> Result<(), String> {
        self.pipe_send(&msg.to_string())
    }

    pub fn next_msg_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    // -----------------------------------------------------------------------
    // Target discovery via CDP events (replaces HTTP /json)
    // -----------------------------------------------------------------------

    /// Enable target discovery via CDP Target domain.
    pub fn enable_target_discovery(&mut self) -> Result<(), String> {
        let id = self.next_msg_id();
        self.send_cdp_wait(
            &json!({
                "id": id,
                "method": "Target.setDiscoverTargets",
                "params": { "discover": true }
            }),
            id,
        )?;
        Ok(())
    }

    /// Read all pending messages and collect target events.
    /// Returns list of newly discovered targets.
    pub fn poll_targets(&mut self) -> Vec<Target> {
        let mut targets = Vec::new();
        loop {
            match self.pipe_read_message(50) {
                Ok(text) => {
                    if let Ok(msg) = serde_json::from_str::<Value>(&text) {
                        if let Some(method) = msg.get("method").and_then(|m| m.as_str()) {
                            match method {
                                "Target.targetCreated" => {
                                    if let Some(info) = msg.get("params")
                                        .and_then(|p| p.get("targetInfo"))
                                    {
                                        let target = Target {
                                            id: info.get("targetId")
                                                .and_then(|v| v.as_str())
                                                .unwrap_or("")
                                                .to_string(),
                                            url: info.get("url")
                                                .and_then(|v| v.as_str())
                                                .unwrap_or("")
                                                .to_string(),
                                            target_type: info.get("type")
                                                .and_then(|v| v.as_str())
                                                .unwrap_or("")
                                                .to_string(),
                                            title: info.get("title")
                                                .and_then(|v| v.as_str())
                                                .unwrap_or("")
                                                .to_string(),
                                        };
                                        targets.push(target);
                                    }
                                }
                                "Target.targetInfoChanged" => {
                                    // Could track updates here
                                }
                                "Target.targetDestroyed" => {
                                    // Could track destruction here
                                }
                                _ => {}
                            }
                        }
                    }
                }
                Err(_) => break,
            }
        }
        targets
    }

    /// Attach to a target and get a session ID.
    pub fn attach_to_target(&mut self, target_id: &str) -> Result<String, String> {
        if let Some(session_id) = self.sessions.get(target_id) {
            return Ok(session_id.clone());
        }

        let id = self.next_msg_id();
        let resp = self.send_cdp_wait(
            &json!({
                "id": id,
                "method": "Target.attachToTarget",
                "params": {
                    "targetId": target_id,
                    "flatten": true
                }
            }),
            id,
        )?;

        // The session ID is in result.sessionId
        let session_id = resp.get("result")
            .and_then(|r| r.get("sessionId"))
            .and_then(|s| s.as_str())
            .ok_or("no sessionId in attach response")?
            .to_string();

        self.sessions.insert(target_id.to_string(), session_id.clone());
        Ok(session_id)
    }

    /// Get all targets by reading pending CDP events.
    /// You should call enable_target_discovery() first, then poll_targets().
    pub fn get_targets(&mut self) -> Result<Vec<Target>, String> {
        // Send Target.getTargets
        let id = self.next_msg_id();
        let resp = self.send_cdp_wait(
            &json!({
                "id": id,
                "method": "Target.getTargets"
            }),
            id,
        )?;

        let mut targets = Vec::new();
        if let Some(target_infos) = resp.get("result").and_then(|r| r.get("targetInfos")) {
            if let Some(arr) = target_infos.as_array() {
                for info in arr {
                    targets.push(Target {
                        id: info.get("targetId")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        url: info.get("url")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        target_type: info.get("type")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        title: info.get("title")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                    });
                }
            }
        }

        Ok(targets)
    }

    /// Send a CDP command to a specific session.
    pub fn send_session_wait(
        &mut self,
        session_id: &str,
        method: &str,
        params: Value,
        expected_id: u64,
    ) -> Result<Value, String> {
        let msg = json!({
            "id": expected_id,
            "method": method,
            "params": params,
            "sessionId": session_id
        });
        self.send_cdp_wait(&msg, expected_id)
    }
}
