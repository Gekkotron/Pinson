//! Minimal RTSP client for RAOP (AirPlay 1): one TCP connection, one
//! request in flight at a time.

use super::packet::Cipher;
use std::{
    io,
    net::{IpAddr, SocketAddr},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{
        TcpStream,
        tcp::{OwnedReadHalf, OwnedWriteHalf},
    },
    time::timeout,
};

const TIMEOUT: Duration = Duration::from_secs(3);
/// Longest status or header line we accept.
const MAX_LINE: u64 = 8 * 1024;
/// Largest reply body we accept.
const MAX_BODY: usize = 64 * 1024;

#[derive(Debug)]
pub enum RtspError {
    Io(io::Error),
    Status(u16, &'static str),
    Bad(String),
}

#[derive(Debug, PartialEq, Eq)]
pub enum Failure {
    Retry,
    Busy,
    Refused,
}

impl RtspError {
    pub fn kind(&self) -> Failure {
        match self {
            RtspError::Status(453, _) => Failure::Busy,
            RtspError::Status(c, _) if (400..500).contains(c) => Failure::Refused,
            _ => Failure::Retry,
        }
    }
}

impl std::fmt::Display for RtspError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RtspError::Io(e) => write!(f, "I/O error: {e}"),
            RtspError::Status(c, m) => write!(f, "{m} answered {c}"),
            RtspError::Bad(s) => write!(f, "bad RTSP answer: {s}"),
        }
    }
}

impl std::error::Error for RtspError {}

impl From<io::Error> for RtspError {
    fn from(e: io::Error) -> Self {
        RtspError::Io(e)
    }
}

fn timed_out() -> RtspError {
    RtspError::Io(io::Error::new(io::ErrorKind::TimedOut, "RTSP timeout"))
}

#[derive(Debug)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Ports {
    pub server: u16,
    pub control: u16,
    pub timing: u16,
}

pub struct Rtsp {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    local: IpAddr,
    url: String,
    sid: String,
    client_id: String,
    active_remote: u32,
    cseq: u32,
    session: Option<String>,
}

impl Rtsp {
    pub async fn connect(addr: SocketAddr, client_id: &str) -> Result<Rtsp, RtspError> {
        let stream = timeout(TIMEOUT, TcpStream::connect(addr))
            .await
            .map_err(|_| timed_out())??;
        let _ = stream.set_nodelay(true);
        let local = stream.local_addr()?.ip();
        let (r, w) = stream.into_split();
        let sid = rand::random::<u32>().to_string();
        let url = match local {
            IpAddr::V4(ip) => format!("rtsp://{ip}/{sid}"),
            IpAddr::V6(ip) => format!("rtsp://[{ip}]/{sid}"),
        };
        Ok(Rtsp {
            reader: BufReader::new(r),
            writer: w,
            local,
            url,
            sid,
            client_id: client_id.to_string(),
            active_remote: rand::random(),
            cseq: 0,
            session: None,
        })
    }

    pub fn local_ip(&self) -> IpAddr {
        self.local
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub async fn request(
        &mut self,
        method: &'static str,
        uri: &str,
        headers: &[(&str, String)],
        body: Option<(&str, &[u8])>,
    ) -> Result<Response, RtspError> {
        self.cseq += 1;
        let mut head = format!(
            "{method} {uri} RTSP/1.0\r\nCSeq: {}\r\nUser-Agent: Pinson/{}\r\nClient-Instance: {}\r\nDACP-ID: {}\r\nActive-Remote: {}\r\n",
            self.cseq,
            env!("CARGO_PKG_VERSION"),
            self.client_id,
            self.client_id,
            self.active_remote
        );
        if let Some(s) = &self.session {
            head.push_str(&format!("Session: {s}\r\n"));
        }
        for (k, v) in headers {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        let mut out = Vec::new();
        match body {
            Some((ct, b)) => {
                head.push_str(&format!(
                    "Content-Type: {ct}\r\nContent-Length: {}\r\n\r\n",
                    b.len()
                ));
                out.extend_from_slice(head.as_bytes());
                out.extend_from_slice(b);
            }
            None => {
                head.push_str("\r\n");
                out.extend_from_slice(head.as_bytes());
            }
        }
        let resp = timeout(TIMEOUT, async {
            self.writer.write_all(&out).await?;
            self.read_response().await
        })
        .await
        .map_err(|_| timed_out())??;
        if resp.status != 200 {
            return Err(RtspError::Status(resp.status, method));
        }
        Ok(resp)
    }

    async fn read_line(&mut self) -> Result<String, RtspError> {
        let mut line = Vec::new();
        let n = (&mut self.reader)
            .take(MAX_LINE)
            .read_until(b'\n', &mut line)
            .await?;
        if n == 0 {
            return Err(RtspError::Io(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "connection closed",
            )));
        }
        if n as u64 >= MAX_LINE && line.last() != Some(&b'\n') {
            return Err(RtspError::Bad("line too long".into()));
        }
        Ok(String::from_utf8_lossy(&line).trim_end().to_string())
    }

    async fn read_response(&mut self) -> Result<Response, RtspError> {
        let status_line = self.read_line().await?;
        let status = status_line
            .strip_prefix("RTSP/1.0 ")
            .and_then(|s| s.split_whitespace().next())
            .and_then(|s| s.parse::<u16>().ok())
            .ok_or_else(|| RtspError::Bad(format!("status line {status_line:?}")))?;
        let mut headers = Vec::new();
        loop {
            let line = self.read_line().await?;
            if line.is_empty() {
                break;
            }
            let (k, v) = line
                .split_once(':')
                .ok_or_else(|| RtspError::Bad(format!("header {line:?}")))?;
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
        let len = headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
            .map(|(_, v)| {
                v.parse::<usize>()
                    .map_err(|_| RtspError::Bad(format!("Content-Length {v:?}")))
            })
            .transpose()?
            .unwrap_or(0);
        if len > MAX_BODY {
            return Err(RtspError::Bad(format!("Content-Length {len} is too large")));
        }
        let mut body = vec![0u8; len];
        self.reader.read_exact(&mut body).await?;
        Ok(Response {
            status,
            headers,
            body,
        })
    }

    pub async fn options(&mut self) -> Result<(), RtspError> {
        self.request("OPTIONS", "*", &[], None).await.map(|_| ())
    }

    pub async fn announce(
        &mut self,
        remote: IpAddr,
        cipher: Option<&Cipher>,
    ) -> Result<(), RtspError> {
        let body = sdp(&self.sid, self.local, remote, cipher);
        let url = self.url.clone();
        self.request(
            "ANNOUNCE",
            &url,
            &[],
            Some(("application/sdp", body.as_bytes())),
        )
        .await
        .map(|_| ())
    }

    pub async fn setup(&mut self, control_port: u16, timing_port: u16) -> Result<Ports, RtspError> {
        let url = self.url.clone();
        let transport = format!(
            "RTP/AVP/UDP;unicast;interleaved=0-1;mode=record;control_port={control_port};timing_port={timing_port}"
        );
        let resp = self
            .request("SETUP", &url, &[("Transport", transport)], None)
            .await?;
        if let Some(s) = resp.header("Session") {
            self.session = Some(s.split(';').next().unwrap_or(s).trim().to_string());
        }
        let transport = resp
            .header("Transport")
            .ok_or_else(|| RtspError::Bad("SETUP reply has no Transport".into()))?;
        let port = |name: &str| -> Option<u16> {
            transport.split(';').find_map(|p| {
                let (k, v) = p.split_once('=')?;
                (k.trim() == name).then(|| v.trim().parse().ok())?
            })
        };
        Ok(Ports {
            server: port("server_port")
                .ok_or_else(|| RtspError::Bad("Transport has no server_port".into()))?,
            control: port("control_port")
                .ok_or_else(|| RtspError::Bad("Transport has no control_port".into()))?,
            timing: port("timing_port").unwrap_or(0),
        })
    }

    pub async fn record(&mut self, seq: u16, ts: u32) -> Result<(), RtspError> {
        let url = self.url.clone();
        self.request(
            "RECORD",
            &url,
            &[
                ("Range", "npt=0-".to_string()),
                ("RTP-Info", format!("seq={seq};rtptime={ts}")),
            ],
            None,
        )
        .await
        .map(|_| ())
    }

    pub async fn set_volume(&mut self, db: f32) -> Result<(), RtspError> {
        let url = self.url.clone();
        let body = format!("volume: {db:.6}\r\n");
        self.request(
            "SET_PARAMETER",
            &url,
            &[],
            Some(("text/parameters", body.as_bytes())),
        )
        .await
        .map(|_| ())
    }

    pub async fn flush(&mut self, seq: u16, ts: u32) -> Result<(), RtspError> {
        let url = self.url.clone();
        self.request(
            "FLUSH",
            &url,
            &[("RTP-Info", format!("seq={seq};rtptime={ts}"))],
            None,
        )
        .await
        .map(|_| ())
    }

    pub async fn teardown(&mut self) -> Result<(), RtspError> {
        let url = self.url.clone();
        self.request("TEARDOWN", &url, &[], None).await.map(|_| ())
    }

    /// Resolves when the peer closes or sends unsolicited bytes. Cancel-safe:
    /// it only waits for buffered data and consumes nothing.
    pub async fn closed(&mut self) -> io::Result<()> {
        self.reader.fill_buf().await.map(|_| ())
    }
}

pub fn sdp(sid: &str, local: IpAddr, remote: IpAddr, cipher: Option<&Cipher>) -> String {
    let fam = |ip: &IpAddr| if ip.is_ipv6() { "IP6" } else { "IP4" };
    let mut lines = vec![
        "v=0".to_string(),
        format!("o=iTunes {sid} 0 IN {} {local}", fam(&local)),
        "s=iTunes".to_string(),
        format!("c=IN {} {remote}", fam(&remote)),
        "t=0 0".to_string(),
        "m=audio 0 RTP/AVP 96".to_string(),
        "a=rtpmap:96 AppleLossless".to_string(),
        "a=fmtp:96 352 0 16 40 10 14 2 255 0 0 44100".to_string(),
    ];
    if let Some(c) = cipher {
        lines.push(format!("a=rsaaeskey:{}", c.rsaaeskey()));
        lines.push(format!("a=aesiv:{}", c.aesiv()));
    }
    let mut s = lines.join("\r\n");
    s.push_str("\r\n");
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::HashMap,
        sync::{Arc, Mutex},
    };
    use tokio::net::TcpListener;

    #[derive(Debug, Clone)]
    struct Req {
        method: String,
        uri: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    }
    impl Req {
        fn h(&self, n: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(n))
                .map(|(_, v)| v.as_str())
        }
    }

    struct Fake {
        addr: SocketAddr,
        reqs: Arc<Mutex<Vec<Req>>>,
    }

    /// `statuses`: per-method status override. `silent`: never answer.
    /// `transport`: SETUP reply Transport. `close_after`: close after N requests.
    async fn fake(
        statuses: HashMap<&'static str, u16>,
        transport: &'static str,
        silent: bool,
        close_after: Option<usize>,
    ) -> Fake {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let reqs = Arc::new(Mutex::new(Vec::new()));
        let r2 = reqs.clone();
        tokio::spawn(async move {
            let (s, _) = l.accept().await.unwrap();
            let (r, mut w) = s.into_split();
            let mut r = BufReader::new(r);
            let mut n = 0;
            loop {
                let mut line = String::new();
                if r.read_line(&mut line).await.unwrap_or(0) == 0 {
                    return;
                }
                let mut parts = line.split_whitespace();
                let method = parts.next().unwrap().to_string();
                let uri = parts.next().unwrap().to_string();
                let mut headers = Vec::new();
                loop {
                    let mut l = String::new();
                    r.read_line(&mut l).await.unwrap();
                    let l = l.trim_end();
                    if l.is_empty() {
                        break;
                    }
                    let (k, v) = l.split_once(':').unwrap();
                    headers.push((k.to_string(), v.trim().to_string()));
                }
                let len = headers
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
                    .map(|(_, v)| v.parse().unwrap())
                    .unwrap_or(0);
                let mut body = vec![0; len];
                r.read_exact(&mut body).await.unwrap();
                let cseq = headers.iter().find(|(k, _)| k == "CSeq").unwrap().1.clone();
                let status = statuses.get(method.as_str()).copied().unwrap_or(200);
                r2.lock().unwrap().push(Req {
                    method: method.clone(),
                    uri,
                    headers,
                    body,
                });
                n += 1;
                if silent {
                    continue;
                }
                let mut resp = format!("RTSP/1.0 {status} X\r\nCSeq: {cseq}\r\n");
                if method == "SETUP" {
                    resp.push_str(&format!("Transport: {transport}\r\nSession: 1\r\n"));
                }
                resp.push_str("\r\n");
                w.write_all(resp.as_bytes()).await.unwrap();
                if close_after == Some(n) {
                    return;
                }
            }
        });
        Fake { addr, reqs }
    }

    const FULL: &str = "RTP/AVP/UDP;unicast;interleaved=0-1;mode=record;control_port=6001;timing_port=6002;server_port=6003";

    async fn ok_fake() -> Fake {
        fake(HashMap::new(), FULL, false, None).await
    }

    #[tokio::test]
    async fn handshake_sends_expected_requests() {
        let f = ok_fake().await;
        let mut c = Rtsp::connect(f.addr, "0123456789ABCDEF").await.unwrap();
        let remote: IpAddr = "10.0.0.9".parse().unwrap();
        c.options().await.unwrap();
        c.announce(remote, None).await.unwrap();
        let ports = c.setup(7001, 7002).await.unwrap();
        c.record(7, 1000).await.unwrap();
        assert_eq!(
            ports,
            Ports {
                server: 6003,
                control: 6001,
                timing: 6002
            }
        );
        let reqs = f.reqs.lock().unwrap().clone();
        let methods: Vec<_> = reqs.iter().map(|r| r.method.as_str()).collect();
        assert_eq!(methods, ["OPTIONS", "ANNOUNCE", "SETUP", "RECORD"]);
        for (i, r) in reqs.iter().enumerate() {
            assert_eq!(r.h("CSeq"), Some((i + 1).to_string().as_str()));
            assert_eq!(r.h("Client-Instance"), Some("0123456789ABCDEF"));
            assert_eq!(r.h("DACP-ID"), Some("0123456789ABCDEF"));
            assert!(r.h("Active-Remote").is_some());
            assert!(r.h("User-Agent").unwrap().starts_with("Pinson/"));
        }
        assert_eq!(reqs[0].uri, "*");
        assert_eq!(reqs[1].uri, c.url());
        assert_eq!(reqs[1].h("Content-Type"), Some("application/sdp"));
        let sid = c.url().rsplit('/').next().unwrap();
        let expect = sdp(sid, c.local_ip(), remote, None);
        assert_eq!(reqs[1].body, expect.as_bytes());
        assert!(!expect.contains("rsaaeskey"));
        assert_eq!(
            reqs[2].h("Transport"),
            Some(
                "RTP/AVP/UDP;unicast;interleaved=0-1;mode=record;control_port=7001;timing_port=7002"
            )
        );
        assert_eq!(reqs[2].h("Session"), None);
        assert_eq!(reqs[3].h("Session"), Some("1"));
        assert_eq!(reqs[3].h("RTP-Info"), Some("seq=7;rtptime=1000"));
        assert_eq!(reqs[3].h("Range"), Some("npt=0-"));
    }

    #[test]
    fn sdp_with_cipher_has_key_and_iv() {
        let c = Cipher::new([1; 16], [2; 16]);
        let s = sdp(
            "42",
            "10.0.0.2".parse().unwrap(),
            "10.0.0.9".parse().unwrap(),
            Some(&c),
        );
        assert!(s.starts_with("v=0\r\no=iTunes 42 0 IN IP4 10.0.0.2\r\n"));
        assert!(s.contains("c=IN IP4 10.0.0.9\r\n"));
        // RSA-OAEP is randomized: check the line's shape, not its value.
        let key = s
            .lines()
            .find_map(|l| l.strip_prefix("a=rsaaeskey:"))
            .expect("rsaaeskey line");
        assert!(key.len() > 300 && !key.contains('='), "{key}");
        assert!(s.ends_with(&format!("a=aesiv:{}\r\n", c.aesiv())));
        let v6 = sdp(
            "1",
            "::1".parse().unwrap(),
            "fe80::1".parse().unwrap(),
            None,
        );
        assert!(v6.contains("IN IP6 ::1") && v6.contains("c=IN IP6 fe80::1"));
    }

    #[tokio::test]
    async fn set_volume_body() {
        let f = ok_fake().await;
        let mut c = Rtsp::connect(f.addr, "0123456789ABCDEF").await.unwrap();
        c.set_volume(-15.0).await.unwrap();
        c.flush(3, 99).await.unwrap();
        c.teardown().await.unwrap();
        let reqs = f.reqs.lock().unwrap().clone();
        assert_eq!(reqs[0].method, "SET_PARAMETER");
        assert_eq!(reqs[0].h("Content-Type"), Some("text/parameters"));
        assert_eq!(reqs[0].body, b"volume: -15.000000\r\n");
        assert_eq!(reqs[1].h("RTP-Info"), Some("seq=3;rtptime=99"));
        assert_eq!(reqs[2].method, "TEARDOWN");
    }

    #[tokio::test]
    async fn status_classification() {
        for (code, want) in [
            (453, Failure::Busy),
            (403, Failure::Refused),
            (470, Failure::Refused),
            (500, Failure::Retry),
        ] {
            let f = fake(HashMap::from([("ANNOUNCE", code)]), FULL, false, None).await;
            let mut c = Rtsp::connect(f.addr, "0123456789ABCDEF").await.unwrap();
            let e = c
                .announce("10.0.0.9".parse().unwrap(), None)
                .await
                .unwrap_err();
            assert!(matches!(e, RtspError::Status(n, "ANNOUNCE") if n == code));
            assert_eq!(e.kind(), want, "status {code}");
        }
        assert_eq!(RtspError::Bad("x".into()).kind(), Failure::Retry);
        assert_eq!(RtspError::Io(io::Error::other("x")).kind(), Failure::Retry);
    }

    #[tokio::test]
    async fn missing_server_port_is_bad() {
        let f = fake(
            HashMap::new(),
            "RTP/AVP/UDP;unicast;control_port=6001;timing_port=6002",
            false,
            None,
        )
        .await;
        let mut c = Rtsp::connect(f.addr, "0123456789ABCDEF").await.unwrap();
        let e = c.setup(1, 2).await.unwrap_err();
        assert!(matches!(e, RtspError::Bad(_)), "{e:?}");
        assert_eq!(e.kind(), Failure::Retry);
    }

    #[tokio::test]
    async fn closed_resolves_on_peer_close() {
        let f = fake(HashMap::new(), FULL, false, Some(1)).await;
        let mut c = Rtsp::connect(f.addr, "0123456789ABCDEF").await.unwrap();
        c.options().await.unwrap();
        timeout(Duration::from_secs(2), c.closed())
            .await
            .expect("closed() should resolve")
            .unwrap();
    }

    #[tokio::test]
    async fn closed_pending_while_open_and_cancel_safe() {
        let f = ok_fake().await;
        let mut c = Rtsp::connect(f.addr, "0123456789ABCDEF").await.unwrap();
        assert!(
            timeout(Duration::from_millis(100), c.closed())
                .await
                .is_err()
        );
        c.options().await.unwrap();
    }

    #[tokio::test]
    async fn request_times_out() {
        let f = fake(HashMap::new(), FULL, true, None).await;
        let mut c = Rtsp::connect(f.addr, "0123456789ABCDEF").await.unwrap();
        let t = std::time::Instant::now();
        let e = c.options().await.unwrap_err();
        assert!(matches!(e, RtspError::Io(_)), "{e:?}");
        assert!(t.elapsed() < Duration::from_secs(4));
    }

    /// Serves one canned reply to the first request, then holds the socket.
    async fn raw_reply(reply: Vec<u8>) -> SocketAddr {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            let (s, _) = l.accept().await.unwrap();
            let (r, mut w) = s.into_split();
            let mut r = BufReader::new(r);
            loop {
                let mut line = String::new();
                r.read_line(&mut line).await.unwrap();
                if line.trim().is_empty() {
                    break;
                }
            }
            w.write_all(&reply).await.ok();
            tokio::time::sleep(Duration::from_secs(5)).await;
        });
        addr
    }

    #[tokio::test]
    async fn huge_content_length_is_bad() {
        let addr =
            raw_reply(b"RTSP/1.0 200 OK\r\nCSeq: 1\r\nContent-Length: 4000000000\r\n\r\n".to_vec())
                .await;
        let mut c = Rtsp::connect(addr, "0123456789ABCDEF").await.unwrap();
        let e = c.options().await.unwrap_err();
        assert!(matches!(e, RtspError::Bad(_)), "{e:?}");
    }

    #[tokio::test]
    async fn overlong_header_line_is_bad() {
        let mut reply = b"RTSP/1.0 200 OK\r\nCSeq: 1\r\nX-Pad: ".to_vec();
        reply.extend(std::iter::repeat_n(b'a', 20_000));
        reply.extend_from_slice(b"\r\n\r\n");
        let addr = raw_reply(reply).await;
        let mut c = Rtsp::connect(addr, "0123456789ABCDEF").await.unwrap();
        let e = c.options().await.unwrap_err();
        assert!(matches!(e, RtspError::Bad(_)), "{e:?}");
    }
}
