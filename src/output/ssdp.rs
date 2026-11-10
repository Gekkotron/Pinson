//! SSDP discovery of UPnP media renderers.
//!
//! One background task searches every non-loopback IPv4 interface every 30 s, reads the
//! description of each new device and publishes the live set. Each socket is bound to its
//! interface and sends multicast through it: a plain socket gets no answers when the default
//! route is not the LAN (a VPN, for example).

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::Duration;

use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;
use tokio::time::{Instant, sleep, timeout_at};

use super::upnp::xml::{DeviceDesc, parse_description};

const GROUP: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 250);
const PORT: u16 = 1900;
const ROUND_EVERY: Duration = Duration::from_secs(30);
const COLLECT: Duration = Duration::from_secs(3);
const FETCH_TIMEOUT: Duration = Duration::from_secs(3);
/// A device stays listed for this many rounds after the last reply.
const KEEP_ROUNDS: u64 = 2;

/// The M-SEARCH for media renderers.
pub(crate) fn search_request() -> String {
    "M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: 2\r\nST: urn:schemas-upnp-org:device:MediaRenderer:1\r\n\r\n"
        .to_string()
}

/// What a search reply tells us.
#[derive(Debug, PartialEq)]
pub(crate) struct Reply {
    pub location: String,
    pub usn: Option<String>,
}

/// Reads a `HTTP/1.1 200` search reply; anything else (NOTIFY, no LOCATION) is `None`.
pub(crate) fn parse_reply(bytes: &[u8]) -> Option<Reply> {
    let text = std::str::from_utf8(bytes).ok()?;
    let mut lines = text.split("\r\n");
    if !lines.next()?.trim().starts_with("HTTP/1.1 200") {
        return None;
    }
    let (mut location, mut usn) = (None, None);
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.trim().eq_ignore_ascii_case("location") {
            location = Some(value.to_string());
        } else if name.trim().eq_ignore_ascii_case("usn") {
            usn = Some(value.to_string());
        }
    }
    let location = location.filter(|l| l.starts_with("http://") || l.starts_with("https://"))?;
    Some(Reply { location, usn })
}

/// Which locations answered in which search round.
#[derive(Default)]
pub(crate) struct Seen {
    rounds: HashMap<String, u64>,
}

impl Seen {
    pub fn mark(&mut self, location: &str, round: u64) {
        self.rounds.insert(location.to_string(), round);
    }

    /// Locations seen in rounds `round-2..=round`.
    pub fn live(&self, round: u64) -> Vec<String> {
        let mut v: Vec<String> = self
            .rounds
            .iter()
            .filter(|(_, r)| **r <= round && **r + KEEP_ROUNDS >= round)
            .map(|(l, _)| l.clone())
            .collect();
        v.sort();
        v
    }

    pub fn forget_old(&mut self, round: u64) {
        self.rounds.retain(|_, r| *r + KEEP_ROUNDS >= round);
    }
}

fn bind(ip: Ipv4Addr) -> std::io::Result<UdpSocket> {
    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    s.bind(&SocketAddr::V4(SocketAddrV4::new(ip, 0)).into())?;
    s.set_multicast_if_v4(&ip)?;
    s.set_multicast_ttl_v4(2)?;
    s.set_nonblocking(true)?;
    UdpSocket::from_std(s.into())
}

/// Searches from one interface for `COLLECT`, returning the locations that answered.
async fn search_from(ip: Ipv4Addr) -> std::io::Result<Vec<String>> {
    let sock = bind(ip)?;
    let dest = SocketAddr::V4(SocketAddrV4::new(GROUP, PORT));
    let request = search_request();
    let deadline = Instant::now() + COLLECT;
    sock.send_to(request.as_bytes(), dest).await?;
    let mut second_sent = false;
    let second_at = Instant::now() + Duration::from_millis(100);
    let mut found = Vec::new();
    let mut buf = [0u8; 2048];
    loop {
        let wake = if second_sent { deadline } else { second_at };
        match timeout_at(wake, sock.recv_from(&mut buf)).await {
            Ok(Ok((n, _))) => {
                if let Some(r) = parse_reply(&buf[..n]) {
                    found.push(r.location);
                }
            }
            Ok(Err(e)) => tracing::debug!("SSDP receive on {ip}: {e}"),
            Err(_) if !second_sent => {
                second_sent = true;
                let _ = sock.send_to(request.as_bytes(), dest).await;
            }
            Err(_) => break,
        }
    }
    Ok(found)
}

/// One search round on every interface; the locations that answered.
async fn search_round() -> Vec<String> {
    let ifaces = if_addrs::get_if_addrs().unwrap_or_default();
    let ips: Vec<Ipv4Addr> = ifaces
        .into_iter()
        .filter(|i| !i.is_loopback())
        .filter_map(|i| match i.ip() {
            std::net::IpAddr::V4(v4) => Some(v4),
            _ => None,
        })
        .collect();
    let searches = ips.iter().map(|ip| async move {
        match search_from(*ip).await {
            Ok(v) => v,
            Err(e) => {
                tracing::debug!("SSDP search on {ip} skipped: {e}");
                Vec::new()
            }
        }
    });
    let mut all: Vec<String> = futures_util::future::join_all(searches)
        .await
        .into_iter()
        .flatten()
        .collect();
    all.sort();
    all.dedup();
    all
}

async fn fetch(client: &reqwest::Client, location: &str) -> Result<DeviceDesc, String> {
    let body = client
        .get(location)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| e.to_string())?
        .text()
        .await
        .map_err(|e| e.to_string())?;
    parse_description(&body, location)
}

/// Runs one round: search, fetch new descriptions, return the live renderers.
struct Round {
    seen: Seen,
    descs: HashMap<String, (u64, DeviceDesc)>,
    n: u64,
    fetched: u64,
    client: reqwest::Client,
}

impl Round {
    fn new() -> Self {
        Round {
            seen: Seen::default(),
            descs: HashMap::new(),
            n: 0,
            fetched: 0,
            client: reqwest::Client::builder()
                .no_proxy()
                .timeout(FETCH_TIMEOUT)
                .build()
                .expect("reqwest client"),
        }
    }

    async fn run(&mut self) -> Vec<DeviceDesc> {
        self.n += 1;
        let locations = search_round().await;
        for loc in &locations {
            self.seen.mark(loc, self.n);
            if !self.descs.contains_key(loc) {
                match fetch(&self.client, loc).await {
                    Ok(d) => {
                        self.fetched += 1;
                        self.descs.insert(loc.clone(), (self.fetched, d));
                    }
                    Err(e) => tracing::debug!("UPnP description {loc}: {e}"),
                }
            }
        }
        self.seen.forget_old(self.n);
        let live = self.seen.live(self.n);
        self.descs.retain(|l, _| live.contains(l));
        let mut latest: HashMap<&str, &(u64, DeviceDesc)> = HashMap::new();
        for entry in self.descs.values() {
            let slot = latest.entry(entry.1.udn.as_str()).or_insert(entry);
            if entry.0 > slot.0 {
                *slot = entry;
            }
        }
        let mut out: Vec<DeviceDesc> = latest.into_values().map(|e| e.1.clone()).collect();
        out.sort_by(|a, b| (&a.name, &a.udn).cmp(&(&b.name, &b.udn)));
        out
    }
}

/// Searches now and every 30 s; calls `publish` with the live renderers after each round.
/// Must run inside the tokio runtime.
pub(crate) fn spawn(publish: impl Fn(Vec<DeviceDesc>) + Send + 'static) {
    tokio::spawn(async move {
        let mut round = Round::new();
        loop {
            let found = round.run().await;
            publish(found);
            sleep(ROUND_EVERY).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_request_is_exact() {
        assert_eq!(
            search_request(),
            "M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: 2\r\nST: urn:schemas-upnp-org:device:MediaRenderer:1\r\n\r\n"
        );
    }

    #[test]
    fn parse_reply_reads_location() {
        let raw = "HTTP/1.1 200 OK\r\nCache-Control: max-age=1800\r\nlocation: http://10.0.0.3:49152/description.xml\r\nST: urn:schemas-upnp-org:device:MediaRenderer:1\r\nUsn: uuid:4d696e69-1234-4abc-8def-0123456789ab::urn:schemas-upnp-org:device:MediaRenderer:1\r\n\r\n";
        assert_eq!(
            parse_reply(raw.as_bytes()),
            Some(Reply {
                location: "http://10.0.0.3:49152/description.xml".into(),
                usn: Some(
                    "uuid:4d696e69-1234-4abc-8def-0123456789ab::urn:schemas-upnp-org:device:MediaRenderer:1"
                        .into()
                ),
            })
        );
    }

    #[test]
    fn parse_reply_rejects_notify_and_missing_location() {
        let notify = "NOTIFY * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nLOCATION: http://10.0.0.3:49152/d.xml\r\nNTS: ssdp:alive\r\n\r\n";
        assert_eq!(parse_reply(notify.as_bytes()), None);
        let none = "HTTP/1.1 200 OK\r\nST: x\r\n\r\n";
        assert_eq!(parse_reply(none.as_bytes()), None);
        let bad = "HTTP/1.1 200 OK\r\nLOCATION: ftp://x/y\r\n\r\n";
        assert_eq!(parse_reply(bad.as_bytes()), None);
        assert_eq!(parse_reply(&[0xff, 0xfe]), None);
    }

    #[test]
    fn seen_ages_out_after_three_rounds() {
        let mut s = Seen::default();
        s.mark("http://a/d.xml", 1);
        for r in 1..=3 {
            assert_eq!(s.live(r), vec!["http://a/d.xml".to_string()], "round {r}");
        }
        assert!(s.live(4).is_empty());
        s.forget_old(4);
        assert!(s.rounds.is_empty());
    }

    #[tokio::test]
    #[ignore = "needs real multicast on a LAN"]
    async fn live_ssdp_finds_renderers() {
        let mut round = Round::new();
        let found = round.run().await;
        for d in &found {
            println!("{} | {} | {}", d.name, d.udn, d.location);
        }
        println!("{} renderer(s)", found.len());
    }
}
