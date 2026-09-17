use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::net::TcpStream;
use std::time::Duration;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{connect, Message, WebSocket};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Target {
    pub id: String,
    pub url: String,
    #[serde(rename = "type")]
    pub target_type: String,
    #[serde(default)]
    pub title: String,
}

pub struct CdpClient {
    ws: WebSocket<MaybeTlsStream<TcpStream>>,
    target_id: Option<String>,
    port: u16,
}

fn reqwest_get(port: u16, path: &str) -> Result<String, String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(3))
        .connect_timeout(Duration::from_secs(3))
        .build()
        .map_err(|e| format!("Failed to build HTTP client: {}", e))?;
    let url = format!("http://127.0.0.1:{}{}", port, path);
    let response = client
        .get(&url)
        .send()
        .map_err(|e| format!("HTTP GET failed: {}", e))?;
    response
        .text()
        .map_err(|e| format!("Failed to read response: {}", e))
}

impl CdpClient {
    pub fn connect(port: u16) -> Result<Self, String> {
        let body = reqwest_get(port, "/json")?;
        let targets: Vec<Target> = serde_json::from_str(&body)
            .map_err(|e| format!("Failed to parse targets: {} body={}", e, &body[..body.len().min(200)]))?;

        if targets.is_empty() {
            return Err("No targets available".to_string());
        }

        let target = targets.first().unwrap();
        let ws_url = format!("ws://127.0.0.1:{}/devtools/page/{}", port, target.id);

        let (ws, _) =
            connect(&ws_url).map_err(|e| format!("WebSocket connection failed: {}", e))?;

        Ok(CdpClient {
            ws,
            target_id: Some(target.id.clone()),
            port,
        })
    }

    pub fn attach_to_target(&mut self, target_id: &str) -> Result<(), String> {
        if self.target_id.as_deref() == Some(target_id) {
            return Ok(());
        }
        let ws_url = format!("ws://127.0.0.1:{}/devtools/page/{}", self.port, target_id);
        let (ws, _) =
            connect(&ws_url).map_err(|e| format!("WebSocket connection failed: {}", e))?;
        self.ws = ws;
        self.target_id = Some(target_id.to_string());
        Ok(())
    }

    pub fn get_targets(&self) -> Result<Vec<Target>, String> {
        let body = reqwest_get(self.port, "/json")?;
        let targets: Vec<Target> = serde_json::from_str(&body)
            .map_err(|e| format!("Failed to parse targets: {}", e))?;
        Ok(targets)
    }

    pub fn send_cdp(&mut self, msg: &Value) -> Result<(), String> {
        self.ws
            .send(Message::Text(msg.to_string()))
            .map_err(|e| format!("Send error: {}", e))
    }

    pub fn send_cdp_wait(&mut self, msg: &Value, expected_id: u64) -> Result<Value, String> {
        self.send_cdp(msg)?;
        self.read_response(expected_id)
    }

    pub fn read_response(&mut self, expected_id: u64) -> Result<Value, String> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if std::time::Instant::now() > deadline {
                return Err(format!("Timeout waiting for response id={}", expected_id));
            }
            match self.ws.read() {
                Ok(Message::Text(text)) => {
                    if let Ok(msg) = serde_json::from_str::<Value>(&text) {
                        if msg.get("id").and_then(|i| i.as_u64()) == Some(expected_id) {
                            return Ok(msg);
                        }
                    }
                }
                Ok(_) => {}
                Err(tungstenite::Error::Io(ref e))
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    continue;
                }
                Err(e) => {
                    return Err(format!("Read error: {}", e));
                }
            }
        }
    }

    pub fn is_alive(&self) -> bool {
        reqwest_get(self.port, "/json/version").is_ok()
    }
}
