//! Snapcast: per-client volume through snapserver's JSON-RPC control port.

use std::time::Duration;

use async_trait::async_trait;
use axum::http::StatusCode;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

use crate::speakers::{Speaker, SpeakerChange, SpeakerControl, SpeakerError};

/// Port of snapserver's JSON-RPC control interface.
const DEFAULT_CONTROL_PORT: u16 = 1705;

const UNREACHABLE: &str = "Can't reach snapserver";

/// Longest client id accepted (snapserver uses MAC addresses or host ids).
const ID_MAX: usize = 128;

/// snapserver's control address: `explicit` if given, else the host of
/// `address` on port 1705.
pub fn snapcast_control(address: &str, explicit: Option<&str>) -> String {
    if let Some(control) = explicit {
        return control.to_owned();
    }
    let host = address.rsplit_once(':').map_or(address, |(host, _)| host);
    format!("{host}:{DEFAULT_CONTROL_PORT}")
}

/// Speaker control through snapserver's JSON-RPC port; one connection per call.
pub struct SnapcastSpeakers {
    control: String,
    timeout: Duration,
}

impl SnapcastSpeakers {
    pub fn new(control: String, timeout: Duration) -> Self {
        Self { control, timeout }
    }

    async fn connect(&self) -> Result<Conn, SpeakerError> {
        let stream = TcpStream::connect(&self.control).await.map_err(io_error)?;
        let (read, write) = stream.into_split();
        Ok(Conn {
            reader: BufReader::new(read),
            writer: write,
        })
    }

    async fn list_inner(&self) -> Result<Vec<Speaker>, SpeakerError> {
        let mut conn = self.connect().await?;
        let status = conn.call(1, "Server.GetStatus", json!({})).await?;
        Ok(clients(&status))
    }

    async fn set_inner(&self, id: &str, change: &SpeakerChange) -> Result<(), SpeakerError> {
        let mut conn = self.connect().await?;
        let status = conn.call(1, "Server.GetStatus", json!({})).await?;
        let current = clients(&status)
            .into_iter()
            .find(|s| s.id == id)
            .ok_or_else(unknown_speaker)?;
        let params = json!({
            "id": id,
            "volume": {
                "percent": change.volume.unwrap_or(current.volume),
                "muted": change.muted.unwrap_or(current.muted),
            },
        });
        conn.call(2, "Client.SetVolume", params).await?;
        Ok(())
    }
}

#[async_trait]
impl SpeakerControl for SnapcastSpeakers {
    async fn list(&self) -> Result<Vec<Speaker>, SpeakerError> {
        tokio::time::timeout(self.timeout, self.list_inner())
            .await
            .unwrap_or_else(|_| Err(timed_out()))
    }

    async fn set(&self, id: &str, change: &SpeakerChange) -> Result<(), SpeakerError> {
        if id.is_empty() || id.len() > ID_MAX {
            return Err(unknown_speaker());
        }
        if change.volume.is_some_and(|v| v > 100) {
            return Err(SpeakerError::Rejected(
                StatusCode::BAD_REQUEST,
                "volume must be 0 to 100".into(),
            ));
        }
        tokio::time::timeout(self.timeout, self.set_inner(id, change))
            .await
            .unwrap_or_else(|_| Err(timed_out()))
    }
}

struct Conn {
    reader: BufReader<tokio::net::tcp::OwnedReadHalf>,
    writer: tokio::net::tcp::OwnedWriteHalf,
}

impl Conn {
    /// Sends one request and reads lines until the answer with the same `id`.
    async fn call(&mut self, id: u64, method: &str, params: Value) -> Result<Value, SpeakerError> {
        let mut line =
            json!({"id": id, "jsonrpc": "2.0", "method": method, "params": params}).to_string();
        line.push('\n');
        self.writer
            .write_all(line.as_bytes())
            .await
            .map_err(io_error)?;
        loop {
            let mut buf = String::new();
            if self.reader.read_line(&mut buf).await.map_err(io_error)? == 0 {
                tracing::warn!("snapserver closed the connection");
                return Err(unreachable_error());
            }
            let msg: Value = serde_json::from_str(buf.trim()).map_err(|e| {
                tracing::warn!("snapserver sent an unreadable line: {e}");
                unreachable_error()
            })?;
            if msg.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(error) = msg.get("error") {
                let message = error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("snapserver refused the request")
                    .to_owned();
                let status = if message.to_lowercase().contains("not found") {
                    StatusCode::NOT_FOUND
                } else {
                    StatusCode::BAD_REQUEST
                };
                return Err(SpeakerError::Rejected(status, message));
            }
            return msg.get("result").cloned().ok_or_else(|| {
                tracing::warn!("snapserver answer has no result");
                unreachable_error()
            });
        }
    }
}

/// Flattens `server.groups[].clients[]` of a `Server.GetStatus` result.
fn clients(status: &Value) -> Vec<Speaker> {
    let groups = status.pointer("/server/groups").and_then(Value::as_array);
    groups
        .into_iter()
        .flatten()
        .filter_map(|g| g.get("clients").and_then(Value::as_array))
        .flatten()
        .filter_map(|c| {
            let id = c.get("id")?.as_str()?.to_owned();
            let configured = c.pointer("/config/name").and_then(Value::as_str);
            let host = c.pointer("/host/name").and_then(Value::as_str);
            let name = configured
                .filter(|n| !n.is_empty())
                .or(host)
                .unwrap_or_default()
                .to_owned();
            let volume = c
                .pointer("/config/volume/percent")
                .and_then(Value::as_u64)
                .map_or(100, |p| p.min(100) as u8);
            Some(Speaker {
                id,
                name,
                volume,
                muted: c
                    .pointer("/config/volume/muted")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                connected: c.get("connected").and_then(Value::as_bool).unwrap_or(false),
            })
        })
        .collect()
}

fn unknown_speaker() -> SpeakerError {
    SpeakerError::Rejected(StatusCode::NOT_FOUND, "unknown speaker".into())
}

fn io_error(e: std::io::Error) -> SpeakerError {
    tracing::warn!("can't reach snapserver: {e}");
    unreachable_error()
}

fn timed_out() -> SpeakerError {
    tracing::warn!("snapserver timed out");
    unreachable_error()
}

fn unreachable_error() -> SpeakerError {
    SpeakerError::Unreachable(UNREACHABLE.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::net::TcpListener;

    /// A fake snapserver: per connection, for each request line it records the
    /// request and writes the next canned reply block (each block is a list of lines).
    async fn fake_snapserver(replies: Vec<Vec<String>>) -> (String, Arc<Mutex<Vec<Value>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let rec = seen.clone();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = stream.into_split();
            let mut lines = BufReader::new(read).lines();
            for block in replies {
                let Ok(Some(line)) = lines.next_line().await else {
                    return;
                };
                rec.lock()
                    .unwrap()
                    .push(serde_json::from_str(&line).unwrap());
                for l in block {
                    write.write_all(format!("{l}\n").as_bytes()).await.unwrap();
                }
            }
            let _ = lines.next_line().await;
        });
        (addr, seen)
    }

    const STATUS: &str = r#"{"id":1,"jsonrpc":"2.0","result":{"server":{"groups":[{"id":"g","stream_id":"Pinson","clients":[
        {"id":"aa:bb","connected":true,"host":{"name":"kitchen","ip":"1.2.3.4"},"config":{"name":"","volume":{"muted":false,"percent":74},"latency":0}},
        {"id":"cc:dd","connected":false,"host":{"name":"hall","ip":"1.2.3.5"},"config":{"name":"Hallway","volume":{"muted":true,"percent":40},"latency":0}}]}],"streams":[]}}}"#;

    fn speakers(addr: String) -> SnapcastSpeakers {
        SnapcastSpeakers::new(addr, Duration::from_secs(2))
    }

    fn one_line(s: &str) -> String {
        s.split_whitespace().collect::<Vec<_>>().join("")
    }

    #[tokio::test]
    async fn lists_clients_with_fallback_names() {
        let (addr, seen) = fake_snapserver(vec![vec![one_line(STATUS)]]).await;
        let list = speakers(addr).list().await.unwrap();
        assert_eq!(
            list,
            vec![
                Speaker {
                    id: "aa:bb".into(),
                    name: "kitchen".into(),
                    volume: 74,
                    muted: false,
                    connected: true
                },
                Speaker {
                    id: "cc:dd".into(),
                    name: "Hallway".into(),
                    volume: 40,
                    muted: true,
                    connected: false
                },
            ]
        );
        let req = seen.lock().unwrap()[0].clone();
        assert_eq!(req["method"], "Server.GetStatus");
        assert_eq!(req["jsonrpc"], "2.0");
        assert_eq!(req["id"], 1);
    }

    #[tokio::test]
    async fn skips_notifications_before_response() {
        let note = r#"{"jsonrpc":"2.0","method":"Client.OnConnect","params":{}}"#.to_string();
        let (addr, _) = fake_snapserver(vec![vec![note, one_line(STATUS)]]).await;
        assert_eq!(speakers(addr).list().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn set_merges_volume_and_mute() {
        let ok = r#"{"id":2,"jsonrpc":"2.0","result":{"volume":{"muted":true,"percent":40}}}"#;
        let (addr, seen) = fake_snapserver(vec![vec![one_line(STATUS)], vec![ok.into()]]).await;
        let change = SpeakerChange {
            volume: None,
            muted: Some(true),
        };
        speakers(addr).set("cc:dd", &change).await.unwrap();
        let req = seen.lock().unwrap()[1].clone();
        assert_eq!(req["id"], 2);
        assert_eq!(req["method"], "Client.SetVolume");
        assert_eq!(
            req["params"],
            json!({"id": "cc:dd", "volume": {"percent": 40, "muted": true}})
        );
    }

    #[tokio::test]
    async fn unknown_client_is_404() {
        let (addr, _) = fake_snapserver(vec![vec![one_line(STATUS)]]).await;
        let change = SpeakerChange {
            volume: Some(10),
            muted: None,
        };
        let err = speakers(addr).set("nope", &change).await.unwrap_err();
        assert!(
            matches!(err, SpeakerError::Rejected(StatusCode::NOT_FOUND, m) if m == "unknown speaker")
        );
        let long = "x".repeat(129);
        let err = speakers("127.0.0.1:1".into())
            .set(&long, &change)
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            SpeakerError::Rejected(StatusCode::NOT_FOUND, _)
        ));
        let err = speakers("127.0.0.1:1".into())
            .set("", &change)
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            SpeakerError::Rejected(StatusCode::NOT_FOUND, _)
        ));
    }

    #[tokio::test]
    async fn rpc_error_maps_to_404_or_400() {
        let nf = r#"{"id":1,"jsonrpc":"2.0","error":{"code":-32603,"message":"Client not found"}}"#;
        let (addr, _) = fake_snapserver(vec![vec![nf.into()]]).await;
        let err = speakers(addr).list().await.unwrap_err();
        assert!(matches!(
            err,
            SpeakerError::Rejected(StatusCode::NOT_FOUND, _)
        ));
        let bad = r#"{"id":1,"jsonrpc":"2.0","error":{"code":-32602,"message":"Invalid params"}}"#;
        let (addr, _) = fake_snapserver(vec![vec![bad.into()]]).await;
        let err = speakers(addr).list().await.unwrap_err();
        assert!(
            matches!(err, SpeakerError::Rejected(StatusCode::BAD_REQUEST, m) if m == "Invalid params")
        );
    }

    #[tokio::test]
    async fn volume_over_100_is_400() {
        let change = SpeakerChange {
            volume: Some(101),
            muted: None,
        };
        let err = speakers("127.0.0.1:1".into())
            .set("aa:bb", &change)
            .await
            .unwrap_err();
        assert!(
            matches!(err, SpeakerError::Rejected(StatusCode::BAD_REQUEST, m) if m == "volume must be 0 to 100")
        );
    }

    #[tokio::test]
    async fn down_is_unreachable() {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap().to_string();
        drop(l);
        let err = speakers(addr).list().await.unwrap_err();
        assert!(matches!(err, SpeakerError::Unreachable(m) if m == "Can't reach snapserver"));
    }

    #[tokio::test]
    async fn garbage_is_unreachable() {
        let (addr, _) = fake_snapserver(vec![vec!["not json".into()]]).await;
        let err = speakers(addr).list().await.unwrap_err();
        assert!(matches!(err, SpeakerError::Unreachable(_)));
    }

    #[test]
    fn control_address_rule() {
        assert_eq!(
            snapcast_control("192.168.1.5:4953", None),
            "192.168.1.5:1705"
        );
        assert_eq!(snapcast_control("host", None), "host:1705");
        assert_eq!(snapcast_control("[::1]:4953", None), "[::1]:1705");
        assert_eq!(snapcast_control("x:1", Some("h:9")), "h:9");
    }
}
