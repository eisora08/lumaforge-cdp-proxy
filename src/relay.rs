use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tungstenite::protocol::{Role, WebSocket};

use crate::transport;

pub const RELAY_PORT: u16 = 21778;

static CONN_GENERATION: AtomicUsize = AtomicUsize::new(0);

/// RFC 6455 §1.3 accept key: base64(sha1(key + magic GUID)).
fn accept_key(request_key: &str) -> String {
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    use sha1::{Digest, Sha1};
    let mut hasher = Sha1::new();
    hasher.update(request_key.as_bytes());
    hasher.update(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
    STANDARD.encode(hasher.finalize())
}

/// HTTP + WebSocket relay for cef_hook. Serves /json/version with a browser
/// WebSocket URL and bridges that socket to the shared pipe session, rewriting
/// ids so cef_hook's id space cannot collide with the watch loop's, and
/// forwarding pipe events (Fetch.requestPaused and friends) untouched.
pub fn start_relay_server() {
    std::thread::spawn(|| {
        // In pipe-only sessions the relay is cef_hook's ONLY CDP path, so a
        // transient bind failure (TIME_WAIT from a previous session, another
        // process still exiting) must not kill it for the whole session —
        // retry for ~10s before giving up.
        const BIND_ATTEMPTS: u32 = 5;
        const RETRY_DELAY: Duration = Duration::from_secs(2);
        let mut bound: Option<TcpListener> = None;
        for attempt in 1..=BIND_ATTEMPTS {
            match TcpListener::bind(("127.0.0.1", RELAY_PORT)) {
                Ok(l) => {
                    bound = Some(l);
                    break;
                }
                Err(e) => {
                    if attempt == BIND_ATTEMPTS {
                        crate::log_to_temp(&format!(
                            "[relay] Failed to bind 127.0.0.1:{} after {} attempts: {}",
                            RELAY_PORT, BIND_ATTEMPTS, e
                        ));
                    } else {
                        crate::log_to_temp(&format!(
                            "[relay] Bind 127.0.0.1:{} failed (attempt {}/{}): {} - retrying in {}s",
                            RELAY_PORT,
                            attempt,
                            BIND_ATTEMPTS,
                            e,
                            RETRY_DELAY.as_secs()
                        ));
                        std::thread::sleep(RETRY_DELAY);
                    }
                }
            }
        }
        let listener = match bound {
            Some(l) => l,
            None => return,
        };
        crate::log_to_temp(&format!("[relay] Listening on 127.0.0.1:{}", RELAY_PORT));
        for stream in listener.incoming() {
            match stream {
                Ok(s) => {
                    std::thread::spawn(|| handle_connection(s));
                }
                Err(_) => continue,
            }
        }
    });
}

fn read_request_head<R: BufRead>(reader: &mut R) -> Option<String> {
    let mut raw: Vec<u8> = Vec::new();
    loop {
        let mut line: Vec<u8> = Vec::new();
        let n = reader.read_until(b'\n', &mut line).ok()?;
        if n == 0 {
            return None;
        }
        raw.extend_from_slice(&line);
        if line == b"\r\n" || line == b"\n" {
            break;
        }
        if raw.len() > 16 * 1024 {
            return None;
        }
    }
    String::from_utf8(raw).ok()
}

fn parse_request_head(raw: &str) -> Option<(String, HashMap<String, String>)> {
    let mut lines = raw.lines();
    let request_line = lines.next()?;
    let path = request_line.split_whitespace().nth(1)?.to_string();
    let mut headers = HashMap::new();
    for line in lines {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            break;
        }
        if let Some(idx) = line.find(':') {
            headers.insert(
                line[..idx].trim().to_lowercase(),
                line[idx + 1..].trim().to_string(),
            );
        }
    }
    Some((path, headers))
}

fn version_body() -> String {
    json!({
        "Browser": "LumaForge-pipe-relay/1.0",
        "Protocol-Version": "1.3",
        "webSocketDebuggerUrl": format!("ws://127.0.0.1:{}/devtools/browser/luma-relay", RELAY_PORT),
    })
    .to_string()
}

fn handle_connection(stream: TcpStream) {
    let Ok(clone) = stream.try_clone() else {
        return;
    };
    let _ = stream.set_nodelay(true);
    let mut reader = BufReader::new(clone);
    let Some(head) = read_request_head(&mut reader) else {
        return;
    };
    let Some((path, headers)) = parse_request_head(&head) else {
        return;
    };

    if path.starts_with("/json") {
        let body = version_body();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = (&stream).write_all(response.as_bytes());
        return;
    }

    let is_ws = headers
        .get("upgrade")
        .map(|u| u.eq_ignore_ascii_case("websocket"))
        .unwrap_or(false);
    if !is_ws {
        let _ = (&stream)
            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        return;
    }

    // The BufReader may have buffered bytes past the handshake; a WebSocket
    // client never sends frames before the 101, so anything left is a bug.
    if !reader.buffer().is_empty() {
        crate::log_to_temp("[relay] Unexpected buffered bytes after WS handshake, dropping");
        return;
    }

    let Some(key) = headers.get("sec-websocket-key") else {
        return;
    };
    let accept = accept_key(key);
    let response = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {}\r\n\r\n",
        accept
    );
    if (&stream).write_all(response.as_bytes()).is_err() {
        return;
    }

    let ws = WebSocket::from_raw_socket(stream, Role::Server, None);
    handle_ws(ws);
}

fn is_transient_write(io: &std::io::Error) -> bool {
    matches!(
        io.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    )
}

/// Hand queued frames to the socket until it stalls or the queue empties.
///
/// On a transient write failure tungstenite retains the frame in its internal
/// out_buffer (it only drains what was fully written), so the frame must NOT
/// be requeued here — a later `flush()` retries it. Returns false only on a
/// fatal socket error (caller must drop the connection).
fn drain_outbox(
    ws: &mut WebSocket<TcpStream>,
    outbox: &mut VecDeque<String>,
    deferred: &mut bool,
    deferred_since: &mut Option<Instant>,
    last_attempt: &mut Instant,
) -> bool {
    while let Some(text) = outbox.pop_front() {
        match ws.send(tungstenite::Message::Text(text)) {
            Ok(()) => {}
            Err(tungstenite::Error::Io(ref io)) if is_transient_write(io) => {
                *deferred = true;
                if deferred_since.is_none() {
                    *deferred_since = Some(Instant::now());
                }
                *last_attempt = Instant::now();
                return true;
            }
            Err(e) => {
                crate::log_to_temp(&format!("[relay] WS write failed: {e}"));
                return false;
            }
        }
    }
    true
}

fn error_frame(cef_id: &Option<Value>, message: String) -> String {
    let mut err =
        json!({"id": cef_id.clone().unwrap_or(Value::Null), "error": {"message": message}});
    if cef_id.is_none() {
        err.as_object_mut().map(|o| o.remove("id"));
    }
    err.to_string()
}

/// One cef_hook command written to the pipe but not yet answered.
struct InFlight {
    cef_id: Option<Value>,
    wire_id: u64,
    rx: std::sync::mpsc::Receiver<Result<Value, String>>,
    sent_at: Instant,
}

fn handle_ws(mut ws: WebSocket<TcpStream>) {
    let generation = CONN_GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    transport::RELAY_WS_CONNECTED.store(true, Ordering::SeqCst);
    crate::log_to_temp("[relay] cef_hook connected over relay");

    let _ = ws
        .get_mut()
        .set_read_timeout(Some(Duration::from_millis(100)));
    // Short write timeout: this loop must never sit in send() for long. The
    // old 30s write timeout deadlocked the relay against cef_hook (both sides
    // blocked in send while neither drained its socket), froze every Fetch
    // pause — including the store HTML response — for the full timeout, and
    // ended in RST + reconnect. With 100ms a stalled write defers the frame
    // into the outbox and the loop keeps reading commands and events.
    let _ = ws
        .get_mut()
        .set_write_timeout(Some(Duration::from_millis(100)));

    // Pipelined (Millennium model): commands go to the pipe immediately and
    // their responses are forwarded as they arrive. The old design waited for
    // each response before reading the next message — a slow command then
    // starved both Fetch.requestPaused events (queue overflow drops) and
    // every other command (response timeouts during navigations).
    let mut in_flight: Vec<InFlight> = Vec::with_capacity(16);
    // Commands received while no pipe session exists (webhelper respawn gap).
    let mut waiting: Vec<(Option<Value>, Value)> = Vec::new();
    let mut no_session_since: Option<Instant> = None;
    // Frames queued for cef_hook that tungstenite has not accepted yet
    // (only non-empty while a previous write stalled).
    let mut outbox: VecDeque<String> = VecDeque::new();
    let mut write_deferred = false;
    let mut write_deferred_since: Option<Instant> = None;
    let mut last_write_attempt = Instant::now();

    'outer: loop {
        // 0. Retry a stalled write at most every 100ms. A stall that lasts
        //    the whole guard window means cef_hook is not draining — drop it
        //    so it reconnects with a clean socket instead of wedging here.
        if write_deferred {
            if let Some(since) = write_deferred_since {
                if since.elapsed() >= Duration::from_secs(30) {
                    crate::log_to_temp(
                        "[relay] WS write stalled for 30s, dropping cef_hook connection",
                    );
                    break 'outer;
                }
            }
            if last_write_attempt.elapsed() >= Duration::from_millis(100) {
                last_write_attempt = Instant::now();
                match ws.flush() {
                    Ok(()) => {
                        write_deferred = false;
                        write_deferred_since = None;
                    }
                    Err(tungstenite::Error::Io(ref io)) if is_transient_write(io) => {}
                    Err(e) => {
                        crate::log_to_temp(&format!("[relay] WS flush failed: {e}"));
                        break 'outer;
                    }
                }
            }
        }

        // 1. Forward completed command responses, restoring cef_hook's id.
        let mut keep: Vec<InFlight> = Vec::with_capacity(in_flight.len());
        for cmd in in_flight.drain(..) {
            match cmd.rx.try_recv() {
                Ok(Ok(mut r)) => {
                    if let Some(id) = &cmd.cef_id {
                        r["id"] = id.clone();
                    }
                    outbox.push_back(r.to_string());
                }
                Ok(Err(e)) => {
                    outbox.push_back(error_frame(&cmd.cef_id, e));
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    if cmd.sent_at.elapsed() >= Duration::from_secs(5) {
                        let msg = format!("timeout waiting for id={}", cmd.wire_id);
                        outbox.push_back(error_frame(&cmd.cef_id, msg));
                    } else {
                        keep.push(cmd);
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    let e = "pipe response channel closed".to_string();
                    outbox.push_back(error_frame(&cmd.cef_id, e));
                }
            }
        }
        in_flight = keep;

        // 2. Flush commands held while there was no pipe session.
        if !waiting.is_empty() {
            match transport::pipe_handle() {
                Ok(h) => {
                    no_session_since = None;
                    for (cef_id, msg) in waiting.drain(..) {
                        match h.send_begin(&msg) {
                            Ok((wire_id, rx)) => in_flight.push(InFlight {
                                cef_id,
                                wire_id,
                                rx,
                                sent_at: Instant::now(),
                            }),
                            Err(e) => {
                                outbox.push_back(error_frame(&cef_id, e));
                            }
                        }
                    }
                }
                Err(_) => {
                    let start = *no_session_since.get_or_insert_with(Instant::now);
                    if start.elapsed() >= Duration::from_secs(10) {
                        for (cef_id, _) in waiting.drain(..) {
                            outbox.push_back(error_frame(&cef_id, "no pipe session".to_string()));
                        }
                        let _ = drain_outbox(
                            &mut ws,
                            &mut outbox,
                            &mut write_deferred,
                            &mut write_deferred_since,
                            &mut last_write_attempt,
                        );
                        crate::log_to_temp(
                            "[relay] Pipe session gone, dropping cef_hook connection",
                        );
                        break 'outer;
                    }
                }
            }
        }

        // 3. Hand queued frames to the socket, then top up from the pipe.
        //    While a write is stalled we skip both: queued frames wait in the
        //    outbox, and transport events wait in their queue (bounded, drop
        //    oldest) instead of piling into ours.
        if !write_deferred {
            if !drain_outbox(
                &mut ws,
                &mut outbox,
                &mut write_deferred,
                &mut write_deferred_since,
                &mut last_write_attempt,
            ) {
                break 'outer;
            }
            if outbox.is_empty() && !write_deferred {
                if let Ok(h) = transport::pipe_handle() {
                    for event in h.pop_events() {
                        outbox.push_back(event.to_string());
                    }
                }
                if !drain_outbox(
                    &mut ws,
                    &mut outbox,
                    &mut write_deferred,
                    &mut write_deferred_since,
                    &mut last_write_attempt,
                ) {
                    break 'outer;
                }
            }
        }

        // 4. Read the next cef_hook command.
        match ws.read() {
            Ok(tungstenite::Message::Text(text)) => {
                let Ok(msg) = serde_json::from_str::<Value>(&text) else {
                    continue;
                };
                let cef_id = msg.get("id").cloned();
                match transport::pipe_handle() {
                    Ok(h) => {
                        no_session_since = None;
                        match h.send_begin(&msg) {
                            Ok((wire_id, rx)) => in_flight.push(InFlight {
                                cef_id,
                                wire_id,
                                rx,
                                sent_at: Instant::now(),
                            }),
                            Err(e) => {
                                outbox.push_back(error_frame(&cef_id, e));
                            }
                        }
                    }
                    Err(_) => {
                        no_session_since.get_or_insert_with(Instant::now);
                        waiting.push((cef_id, msg));
                    }
                }
            }
            Ok(tungstenite::Message::Close(_)) => break,
            Ok(_) => {}
            Err(tungstenite::Error::Io(ref io))
                if io.kind() == std::io::ErrorKind::WouldBlock
                    || io.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(_) => break,
        }
    }

    if CONN_GENERATION.load(Ordering::SeqCst) == generation {
        transport::RELAY_WS_CONNECTED.store(false, Ordering::SeqCst);
    }
    crate::log_to_temp("[relay] cef_hook disconnected");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_json_request() {
        let raw =
            "GET /json/version HTTP/1.1\r\nHost: 127.0.0.1:21778\r\nConnection: close\r\n\r\n";
        let (path, headers) = parse_request_head(raw).unwrap();
        assert_eq!(path, "/json/version");
        assert_eq!(
            headers.get("host").map(|s| s.as_str()),
            Some("127.0.0.1:21778")
        );
    }

    #[test]
    fn parses_websocket_request() {
        let raw = "GET /devtools/browser/luma-relay HTTP/1.1\r\nHost: 127.0.0.1:21778\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n";
        let (path, headers) = parse_request_head(raw).unwrap();
        assert_eq!(path, "/devtools/browser/luma-relay");
        assert_eq!(
            headers.get("upgrade").map(|s| s.as_str()),
            Some("websocket")
        );
        assert_eq!(
            headers.get("sec-websocket-key").map(|s| s.as_str()),
            Some("dGhlIHNhbXBsZSBub25jZQ==")
        );
    }

    #[test]
    fn accept_key_matches_rfc_example() {
        assert_eq!(
            accept_key("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn version_body_carries_ws_url() {
        let body: Value = serde_json::from_str(&version_body()).unwrap();
        let url = body
            .get("webSocketDebuggerUrl")
            .and_then(|v| v.as_str())
            .unwrap();
        assert!(url.starts_with("ws://127.0.0.1:21778/devtools/browser/"));
    }

    fn serve_one(listener: TcpListener) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            if let Ok((s, _)) = listener.accept() {
                handle_connection(s);
            }
        })
    }

    #[test]
    fn http_json_endpoint_serves_ws_url() {
        use std::io::Read;
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let server = serve_one(listener);

        let mut stream = TcpStream::connect(addr).unwrap();
        stream
            .write_all(b"GET /json/version HTTP/1.1\r\nHost: relay\r\nConnection: close\r\n\r\n")
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        server.join().unwrap();

        assert!(response.starts_with("HTTP/1.1 200 OK"), "got: {}", response);
        assert!(response.contains("webSocketDebuggerUrl"));
        assert!(response.contains("ws://127.0.0.1:21778/devtools/browser/luma-relay"));
    }

    #[test]
    fn websocket_handshake_returns_101() {
        use std::io::Read;
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let server = serve_one(listener);

        let mut stream = TcpStream::connect(addr).unwrap();
        stream
            .write_all(
                b"GET /devtools/browser/luma-relay HTTP/1.1\r\nHost: relay\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n",
            )
            .unwrap();
        let mut response = String::new();
        {
            let mut reader = BufReader::new(&stream);
            let mut line = String::new();
            loop {
                line.clear();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    break;
                }
                response.push_str(&line);
                if line == "\r\n" {
                    break;
                }
            }
        }

        assert!(response.starts_with("HTTP/1.1 101"), "got: {}", response);
        assert!(response.contains("Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo="));

        drop(stream);
        server.join().unwrap();
        assert!(!transport::RELAY_WS_CONNECTED.load(Ordering::SeqCst));
    }
}
