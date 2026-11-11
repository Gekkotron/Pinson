//! An in-process RAOP receiver on 127.0.0.1 for tests: RTSP over TCP plus
//! the audio, control and timing UDP sockets.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::{Notify, mpsc};
use tokio::task::JoinHandle;
use tokio::time::timeout;

use super::packet::alac_raw_decode;

#[derive(Clone, Debug)]
pub(crate) struct Req {
    pub method: String,
    #[allow(dead_code)]
    pub uri: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Req {
    pub fn h(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

#[derive(Default)]
struct Rec {
    reqs: Vec<Req>,
    packets: Vec<(bool, u16, u32, Vec<u8>)>,
    syncs: Vec<(u8, u32, u32)>,
    sender_control: Option<u16>,
    sender_timing: Option<u16>,
}

pub(crate) struct FakeReceiver {
    /// The RTSP address.
    pub addr: SocketAddr,
    rec: Arc<Mutex<Rec>>,
    timing: Arc<UdpSocket>,
    control: Arc<UdpSocket>,
    resends: tokio::sync::Mutex<mpsc::UnboundedReceiver<Vec<u8>>>,
    close: Arc<Notify>,
    tasks: Vec<JoinHandle<()>>,
}

impl Drop for FakeReceiver {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

async fn read_request<R: AsyncBufReadExt + Unpin>(r: &mut R) -> Option<(Req, String)> {
    let mut line = String::new();
    if r.read_line(&mut line).await.ok()? == 0 {
        return None;
    }
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_string();
    let uri = parts.next()?.to_string();
    let mut headers = Vec::new();
    loop {
        let mut l = String::new();
        r.read_line(&mut l).await.ok()?;
        let l = l.trim_end();
        if l.is_empty() {
            break;
        }
        let (k, v) = l.split_once(':')?;
        headers.push((k.to_string(), v.trim().to_string()));
    }
    let len = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0; len];
    r.read_exact(&mut body).await.ok()?;
    let cseq = headers.iter().find(|(k, _)| k == "CSeq")?.1.clone();
    Some((
        Req {
            method,
            uri,
            headers,
            body,
        },
        cseq,
    ))
}

fn transport_port(transport: &str, name: &str) -> Option<u16> {
    transport.split(';').find_map(|p| {
        let (k, v) = p.split_once('=')?;
        (k.trim() == name).then(|| v.trim().parse().ok())?
    })
}

impl FakeReceiver {
    /// `table`: per-method status overrides (default 200).
    pub async fn start(table: HashMap<&'static str, u16>) -> FakeReceiver {
        Self::start_with(table, false).await
    }

    /// With `silent_keepalive`, OPTIONS requests after RECORD go unanswered.
    pub async fn start_with(
        table: HashMap<&'static str, u16>,
        silent_keepalive: bool,
    ) -> FakeReceiver {
        Self::start_full(table, silent_keepalive, HashMap::new()).await
    }

    /// `late`: per-method status overrides that apply after RECORD.
    pub async fn start_late(late: HashMap<&'static str, u16>) -> FakeReceiver {
        Self::start_full(HashMap::new(), false, late).await
    }

    async fn start_full(
        table: HashMap<&'static str, u16>,
        silent_keepalive: bool,
        late: HashMap<&'static str, u16>,
    ) -> FakeReceiver {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let audio = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let control = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let timing = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let ports = (
            audio.local_addr().unwrap().port(),
            control.local_addr().unwrap().port(),
            timing.local_addr().unwrap().port(),
        );
        let rec = Arc::new(Mutex::new(Rec::default()));
        let close = Arc::new(Notify::new());
        let (resend_tx, resend_rx) = mpsc::unbounded_channel();
        let mut tasks = Vec::new();

        let r = rec.clone();
        tasks.push(tokio::spawn(async move {
            let mut buf = vec![0u8; 4096];
            while let Ok((n, _)) = audio.recv_from(&mut buf).await {
                if n >= 12 && buf[1] & 0x7f == 0x60 {
                    r.lock().unwrap().packets.push((
                        buf[1] & 0x80 != 0,
                        u16::from_be_bytes([buf[2], buf[3]]),
                        u32::from_be_bytes(buf[4..8].try_into().unwrap()),
                        buf[12..n].to_vec(),
                    ));
                }
            }
        }));

        let (r, c) = (rec.clone(), control.clone());
        tasks.push(tokio::spawn(async move {
            let mut buf = vec![0u8; 4096];
            while let Ok((n, _)) = c.recv_from(&mut buf).await {
                if n < 2 {
                    continue;
                }
                match buf[1] & 0x7f {
                    0x54 if n >= 20 => r.lock().unwrap().syncs.push((
                        buf[0],
                        u32::from_be_bytes(buf[4..8].try_into().unwrap()),
                        u32::from_be_bytes(buf[16..20].try_into().unwrap()),
                    )),
                    0x56 => {
                        resend_tx.send(buf[..n].to_vec()).ok();
                    }
                    _ => {}
                }
            }
        }));

        let (r, cl) = (rec.clone(), close.clone());
        tasks.push(tokio::spawn(async move {
            loop {
                let Ok((sock, _)) = listener.accept().await else {
                    return;
                };
                let (rd, mut wr) = sock.into_split();
                let mut rd = BufReader::new(rd);
                let (r, cl, table, late) = (r.clone(), cl.clone(), table.clone(), late.clone());
                tokio::spawn(async move {
                    let mut recorded = false;
                    loop {
                        let next = tokio::select! {
                            () = cl.notified() => return,
                            next = read_request(&mut rd) => next,
                        };
                        let Some((req, cseq)) = next else { return };
                        let method = req.method.clone();
                        let mut status = table.get(method.as_str()).copied().unwrap_or(200);
                        if recorded && let Some(s) = late.get(method.as_str()) {
                            status = *s;
                        }
                        let transport = req.h("Transport").map(str::to_string);
                        if method == "SETUP" {
                            let mut g = r.lock().unwrap();
                            if let Some(t) = &transport {
                                g.sender_control = transport_port(t, "control_port");
                                g.sender_timing = transport_port(t, "timing_port");
                            }
                        }
                        r.lock().unwrap().reqs.push(req);
                        if method == "RECORD" && status == 200 {
                            recorded = true;
                        }
                        if silent_keepalive && recorded && method == "OPTIONS" {
                            continue;
                        }
                        let mut resp = format!("RTSP/1.0 {status} X\r\nCSeq: {cseq}\r\n");
                        if method == "SETUP" && status == 200 {
                            resp.push_str(&format!(
                                "Transport: RTP/AVP/UDP;unicast;mode=record;server_port={};control_port={};timing_port={}\r\nSession: 1\r\n",
                                ports.0, ports.1, ports.2
                            ));
                        }
                        resp.push_str("\r\n");
                        if wr.write_all(resp.as_bytes()).await.is_err() {
                            return;
                        }
                    }
                });
            }
        }));

        FakeReceiver {
            addr,
            rec,
            timing,
            control,
            resends: tokio::sync::Mutex::new(resend_rx),
            close,
            tasks,
        }
    }

    pub fn requests(&self) -> Vec<Req> {
        self.rec.lock().unwrap().reqs.clone()
    }

    pub fn methods(&self) -> Vec<String> {
        self.requests().into_iter().map(|r| r.method).collect()
    }

    /// `(marker, seq, ts, payload)` of every audio packet received.
    pub fn packets(&self) -> Vec<(bool, u16, u32, Vec<u8>)> {
        self.rec.lock().unwrap().packets.clone()
    }

    /// The plain payloads decoded to interleaved samples.
    pub fn pcm(&self) -> Vec<i16> {
        self.packets()
            .iter()
            .filter_map(|(_, _, _, p)| alac_raw_decode(p))
            .flatten()
            .collect()
    }

    /// `(first byte, bytes 4..8, bytes 16..20)` of every sync.
    pub fn syncs(&self) -> Vec<(u8, u32, u32)> {
        self.rec.lock().unwrap().syncs.clone()
    }

    /// Sends a timing request to the sender and waits 1 s for the reply.
    pub async fn ask_timing(&self) -> Option<[u8; 32]> {
        let port = self.rec.lock().unwrap().sender_timing?;
        let mut req = [0u8; 32];
        req[..4].copy_from_slice(&[0x80, 0xd2, 0x00, 0x07]);
        req[24..32].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        self.timing.send_to(&req, ("127.0.0.1", port)).await.ok()?;
        let mut buf = [0u8; 64];
        let (n, _) = timeout(Duration::from_secs(1), self.timing.recv_from(&mut buf))
            .await
            .ok()?
            .ok()?;
        buf[..n].try_into().ok()
    }

    /// Asks for `count` packets from `seq`; returns the replies it gets
    /// within 1 s.
    pub async fn ask_resend(&self, seq: u16, count: u16) -> Vec<Vec<u8>> {
        let Some(port) = self.rec.lock().unwrap().sender_control else {
            return Vec::new();
        };
        let mut rx = self.resends.lock().await;
        while rx.try_recv().is_ok() {}
        let mut req = vec![0x80, 0xd5, 0x00, 0x01];
        req.extend_from_slice(&seq.to_be_bytes());
        req.extend_from_slice(&count.to_be_bytes());
        self.control
            .send_to(&req, ("127.0.0.1", port))
            .await
            .unwrap();
        let mut out = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
        while out.len() < usize::from(count) {
            match timeout(deadline - tokio::time::Instant::now(), rx.recv()).await {
                Ok(Some(p)) => out.push(p),
                _ => break,
            }
        }
        out
    }

    /// Closes the RTSP connection, like another sender taking the speaker.
    pub fn close_rtsp(&self) {
        self.close.notify_one();
    }
}
