//! The RAOP sender: a [`Connector`] that opens an RTSP session with an
//! AirPlay 1 speaker and a [`PcmSink`] that streams 44.1 kHz ALAC over UDP.
//!
//! The speaker plays on its own clock from our RTP timestamps, `latency`
//! after they are due. We keep sending in real time (the paced writer does
//! the pacing), answer its timing and resend requests, and re-anchor with a
//! FLUSH when we fall behind or the player skips.

use std::collections::VecDeque;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures_util::future::BoxFuture;
use tokio::net::{UdpSocket, lookup_host};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::{Instant, MissedTickBehavior, interval_at, timeout};

use super::packet::{
    Cipher, FRAMES_PER_PACKET, Ntp, SAMPLE_RATE, alac_raw, audio_packet, parse_resend,
    resend_reply, sync_packet, timing_reply, volume_db,
};
use super::resample::Resampler;
use super::rtsp::{Failure, Ports, Rtsp, RtspError};
use crate::engine::paced::{Connector, PcmSink};
use crate::output::discovery::Discovery;
use crate::output::renderer::{External, Volume};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(8);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(3);
const TEARDOWN_TIMEOUT: Duration = Duration::from_secs(2);
const SYNC_EVERY: Duration = Duration::from_secs(1);
const HISTORY: usize = 512;

/// The last packets sent, for resend requests.
type History = Arc<Mutex<VecDeque<(u16, Vec<u8>)>>>;
const DEFAULT_PORT: u16 = 5000;
const VOLUME_ERROR: &str = "Can't reach the AirPlay speaker";

#[derive(Clone, Copy, Debug)]
pub struct RaopConfig {
    /// How long after a packet's timestamp the speaker plays it.
    pub latency: Duration,
    /// `OPTIONS *` interval.
    pub keepalive: Duration,
    /// Re-anchor (FLUSH) when this far behind schedule.
    pub reanchor_after: Duration,
}

impl RaopConfig {
    pub fn new(latency: Duration) -> Self {
        Self {
            latency,
            keepalive: Duration::from_secs(15),
            // Half the latency, so a stall never outlasts what the speaker
            // has buffered; 1 s at the 2 s default.
            reanchor_after: (latency / 2).min(Duration::from_secs(1)),
        }
    }

    fn latency_frames(&self) -> u32 {
        (self.latency.as_millis() as u64 * u64::from(SAMPLE_RATE) / 1_000) as u32
    }
}

/// Sent to the connection task, which owns the RTSP connection.
enum Cmd {
    Flush {
        seq: u16,
        ts: u32,
        reply: oneshot::Sender<Result<(), ()>>,
    },
    Volume {
        db: f32,
        reply: oneshot::Sender<Result<(), ()>>,
    },
}

struct Session {
    id: u64,
    cmds: mpsc::WeakSender<Cmd>,
}

struct Shared {
    address: String,
    discovery: Discovery,
    cfg: RaopConfig,
    external: Arc<dyn Fn(External) + Send + Sync>,
    client_id: String,
    volume: watch::Sender<Option<Volume>>,
    closed: watch::Sender<bool>,
    session: Mutex<Option<Session>>,
    next_session: AtomicU64,
}

pub struct RaopConnector {
    shared: Arc<Shared>,
}

impl RaopConnector {
    pub fn new(
        address: String,
        discovery: Discovery,
        cfg: RaopConfig,
        external: Arc<dyn Fn(External) + Send + Sync>,
    ) -> Arc<Self> {
        let client_id = format!("{:016X}", rand::random::<u64>());
        Arc::new(Self {
            shared: Arc::new(Shared {
                address,
                discovery,
                cfg,
                external,
                client_id,
                volume: watch::channel(Some(Volume {
                    level: 0.5,
                    muted: false,
                }))
                .0,
                closed: watch::channel(true).0,
                session: Mutex::new(None),
                next_session: AtomicU64::new(0),
            }),
        })
    }

    pub fn volume(&self) -> watch::Receiver<Option<Volume>> {
        self.shared.volume.subscribe()
    }

    /// `true` whenever no RTSP session is open.
    pub fn closed(&self) -> watch::Receiver<bool> {
        self.shared.closed.subscribe()
    }

    /// Updates the volume; with a live session it is sent to the speaker,
    /// else it is sent at the next connect.
    pub async fn set_volume(&self, level: Option<f32>, muted: Option<bool>) -> Result<(), String> {
        let mut v = (*self.shared.volume.borrow()).unwrap_or(Volume {
            level: 0.5,
            muted: false,
        });
        if let Some(l) = level {
            v.level = l.clamp(0.0, 1.0);
        }
        if let Some(m) = muted {
            v.muted = m;
        }
        self.shared.volume.send_replace(Some(v));
        let cmds = {
            let g = self.shared.session.lock().unwrap();
            g.as_ref().and_then(|s| s.cmds.upgrade())
        };
        let Some(cmds) = cmds else { return Ok(()) };
        let (reply, rx) = oneshot::channel();
        let db = volume_db(v.level, v.muted);
        if cmds.send(Cmd::Volume { db, reply }).await.is_err() {
            // The session just ended: the level goes out at the next connect.
            return Ok(());
        }
        match timeout(COMMAND_TIMEOUT, rx).await {
            Ok(Ok(Ok(()))) => Ok(()),
            _ => Err(VOLUME_ERROR.to_string()),
        }
    }
}

impl Connector for RaopConnector {
    fn target(&self) -> String {
        self.shared.address.clone()
    }

    fn connect(&self) -> BoxFuture<'static, io::Result<Box<dyn PcmSink>>> {
        let shared = self.shared.clone();
        Box::pin(async move {
            match timeout(CONNECT_TIMEOUT, shared.handshake()).await {
                Ok(res) => res.map(|sink| Box::new(sink) as Box<dyn PcmSink>),
                Err(_) => Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "AirPlay connect timed out",
                )),
            }
        })
    }
}

fn not_found(address: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        format!("{address} hasn't been found on the network"),
    )
}

impl Shared {
    async fn resolve(&self) -> io::Result<(SocketAddr, bool)> {
        if let Some(dev) = self.discovery.airplay(&self.address) {
            return Ok((dev.addr, dev.encrypt));
        }
        // `host:port` as given; a bare host gets the default RAOP port.
        let target = if self.address.contains(':') {
            self.address.clone()
        } else {
            format!("{}:{DEFAULT_PORT}", self.address)
        };
        let addr = lookup_host(target)
            .await
            .map_err(|_| not_found(&self.address))?
            .next()
            .ok_or_else(|| not_found(&self.address))?;
        Ok((addr, false))
    }

    /// Maps an RTSP failure to the error the paced writer logs, and tells
    /// the player when the speaker belongs to someone else.
    fn fail(&self, e: RtspError) -> io::Error {
        let target = &self.address;
        match e.kind() {
            Failure::Busy => {
                tracing::warn!("AirPlay speaker {target} is busy with another sender");
                (self.external)(External::Pause);
            }
            Failure::Refused => {
                if let RtspError::Status(status, method) = &e {
                    tracing::warn!(
                        "AirPlay speaker {target} refused Pinson ({status} on {method}): it may need AirPlay 2 pairing or a password, which Pinson doesn't support"
                    );
                }
                (self.external)(External::Pause);
            }
            Failure::Retry => {}
        }
        io::Error::other(e)
    }

    async fn handshake(self: &Arc<Self>) -> io::Result<RaopSink> {
        let (addr, encrypt) = self.resolve().await?;
        let mut rtsp = Rtsp::connect(addr, &self.client_id)
            .await
            .map_err(|e| self.fail(e))?;
        let cipher = encrypt.then(Cipher::random);
        rtsp.options().await.map_err(|e| self.fail(e))?;
        rtsp.announce(addr.ip(), cipher.as_ref())
            .await
            .map_err(|e| self.fail(e))?;

        let bind = if addr.is_ipv6() {
            "[::]:0"
        } else {
            "0.0.0.0:0"
        };
        let control = Arc::new(UdpSocket::bind(bind).await?);
        let timing = Arc::new(UdpSocket::bind(bind).await?);
        let audio = Arc::new(UdpSocket::bind(bind).await?);
        let ports = rtsp
            .setup(control.local_addr()?.port(), timing.local_addr()?.port())
            .await
            .map_err(|e| self.fail(e))?;

        let seq: u16 = rand::random();
        let ts: u32 = rand::random();
        let ssrc: u32 = rand::random();
        rtsp.record(seq, ts).await.map_err(|e| self.fail(e))?;
        let volume = *self.volume.borrow();
        if let Some(v) = volume {
            rtsp.set_volume(volume_db(v.level, v.muted))
                .await
                .map_err(|e| self.fail(e))?;
        }

        let history: History = Arc::default();
        let udp = vec![
            tokio::spawn(timing_task(timing, addr.ip())),
            tokio::spawn(control_task(control.clone(), history.clone(), addr.ip())),
        ];
        let (cmds, rx) = mpsc::channel(8);
        let dead = Arc::new(AtomicBool::new(false));
        let id = self.next_session.fetch_add(1, Ordering::Relaxed);
        *self.session.lock().unwrap() = Some(Session {
            id,
            cmds: cmds.downgrade(),
        });
        self.closed.send_replace(false);
        tokio::spawn(run_session(self.clone(), id, rtsp, rx, udp, dead.clone()));
        // `set_volume` may have run after we read the level above and before
        // the session was registered: it found no session and only stored the
        // level. Send it now, without waiting for the answer.
        let latest = *self.volume.borrow();
        if latest != volume
            && let Some(v) = latest
        {
            let (reply, _) = oneshot::channel();
            cmds.try_send(Cmd::Volume {
                db: volume_db(v.level, v.muted),
                reply,
            })
            .ok();
        }
        tracing::info!("AirPlay: connected to {}", self.address);

        Ok(RaopSink {
            cfg: self.cfg,
            remote: addr.ip(),
            ports,
            audio,
            control,
            history,
            cmds,
            dead,
            cipher,
            resampler: Resampler::new(),
            pending: Vec::new(),
            seq,
            ts,
            ssrc,
            anchor: None,
            last_sync: None,
        })
    }
}

async fn timing_task(sock: Arc<UdpSocket>, remote: IpAddr) {
    let mut buf = [0u8; 128];
    while let Ok((n, from)) = sock.recv_from(&mut buf).await {
        if from.ip() != remote {
            continue;
        }
        let received = Ntp::now();
        if let Some(reply) = timing_reply(&buf[..n], received, Ntp::now()) {
            sock.send_to(&reply, from).await.ok();
        }
    }
}

async fn control_task(sock: Arc<UdpSocket>, history: History, remote: IpAddr) {
    let mut buf = [0u8; 128];
    while let Ok((n, from)) = sock.recv_from(&mut buf).await {
        if from.ip() != remote {
            continue;
        }
        let Some((first, count)) = parse_resend(&buf[..n]) else {
            continue;
        };
        for i in 0..count.min(HISTORY as u16) {
            let want = first.wrapping_add(i);
            let packet = history
                .lock()
                .unwrap()
                .iter()
                .find(|(s, _)| *s == want)
                .map(|(_, p)| resend_reply(p));
            if let Some(p) = packet {
                sock.send_to(&p, from).await.ok();
            }
        }
    }
}

/// A 453 or another 4xx in mid-session means the speaker now belongs to
/// someone else: pause the player, like the takeover path. Other errors
/// just end the session and the writer retries.
fn lost_to_other(shared: &Shared, dead: &AtomicBool, e: &RtspError) {
    if matches!(e.kind(), Failure::Busy | Failure::Refused) {
        tracing::warn!("another sender took the AirPlay speaker {}", shared.address);
        dead.store(true, Ordering::Relaxed);
        (shared.external)(External::Pause);
    }
}

/// Owns the RTSP connection until the session ends.
async fn run_session(
    shared: Arc<Shared>,
    id: u64,
    mut rtsp: Rtsp,
    mut rx: mpsc::Receiver<Cmd>,
    udp: Vec<JoinHandle<()>>,
    dead: Arc<AtomicBool>,
) {
    let target = shared.address.clone();
    let mut keepalive = interval_at(Instant::now() + shared.cfg.keepalive, shared.cfg.keepalive);
    keepalive.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            cmd = rx.recv() => match cmd {
                None => {
                    // The sink was dropped: leave politely.
                    dead.store(true, Ordering::Relaxed);
                    timeout(TEARDOWN_TIMEOUT, rtsp.teardown()).await.ok();
                    tracing::info!("AirPlay: closed {target}");
                    break;
                }
                Some(Cmd::Flush { seq, ts, reply }) => {
                    let res = rtsp.flush(seq, ts).await.map_err(|e| {
                        tracing::warn!("AirPlay {target}: FLUSH failed: {e}");
                        lost_to_other(&shared, &dead, &e);
                    });
                    let failed = res.is_err();
                    reply.send(res).ok();
                    if failed {
                        break;
                    }
                }
                Some(Cmd::Volume { db, reply }) => {
                    let res = rtsp.set_volume(db).await.map_err(|e| {
                        tracing::warn!("AirPlay {target}: SET_PARAMETER failed: {e}");
                        lost_to_other(&shared, &dead, &e);
                    });
                    let failed = res.is_err();
                    reply.send(res).ok();
                    if failed {
                        break;
                    }
                }
            },
            _ = keepalive.tick() => {
                if let Err(e) = rtsp.options().await {
                    tracing::warn!("AirPlay {target}: keepalive failed: {e}");
                    lost_to_other(&shared, &dead, &e);
                    break;
                }
            }
            _ = rtsp.closed() => {
                tracing::warn!("another sender took the AirPlay speaker {target}");
                dead.store(true, Ordering::Relaxed);
                (shared.external)(External::Pause);
                break;
            }
        }
    }
    dead.store(true, Ordering::Relaxed);
    for t in udp {
        t.abort();
    }
    let mut session = shared.session.lock().unwrap();
    if session.as_ref().is_some_and(|s| s.id == id) {
        *session = None;
        shared.closed.send_replace(true);
    }
}

/// `anchor`: the wall-clock time of `ts0`, and the frames sent since.
struct Anchor {
    at: Instant,
    ts0: u32,
    frames: u64,
}

pub struct RaopSink {
    cfg: RaopConfig,
    remote: IpAddr,
    ports: Ports,
    audio: Arc<UdpSocket>,
    control: Arc<UdpSocket>,
    history: History,
    cmds: mpsc::Sender<Cmd>,
    dead: Arc<AtomicBool>,
    cipher: Option<Cipher>,
    resampler: Resampler,
    /// Resampled interleaved stereo not yet filling a packet.
    pending: Vec<i16>,
    /// Of the next packet.
    seq: u16,
    ts: u32,
    ssrc: u32,
    anchor: Option<Anchor>,
    last_sync: Option<Instant>,
}

fn aborted() -> io::Error {
    io::Error::new(io::ErrorKind::ConnectionAborted, "AirPlay session closed")
}

impl RaopSink {
    async fn rtsp_flush(&self, seq: u16, ts: u32) -> io::Result<()> {
        let (reply, rx) = oneshot::channel();
        self.cmds
            .send(Cmd::Flush { seq, ts, reply })
            .await
            .map_err(|_| aborted())?;
        match timeout(COMMAND_TIMEOUT + Duration::from_secs(1), rx).await {
            Ok(Ok(Ok(()))) => Ok(()),
            Ok(Ok(Err(()))) => Err(io::Error::other("AirPlay FLUSH failed")),
            Ok(Err(_)) => Err(aborted()),
            Err(_) => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "AirPlay FLUSH timed out",
            )),
        }
    }

    fn sync(&self, first: bool, anchor: &Anchor, now: Instant) -> [u8; 20] {
        let elapsed = now.saturating_duration_since(anchor.at).as_nanos();
        let frames = (elapsed * u128::from(SAMPLE_RATE) / 1_000_000_000) as u64;
        let now_ts = anchor.ts0.wrapping_add(frames as u32);
        sync_packet(first, now_ts, self.cfg.latency_frames(), Ntp::now())
    }

    async fn send_packet(&mut self, frame: &[i16]) -> io::Result<()> {
        let mut payload = alac_raw(frame);
        if let Some(c) = &self.cipher {
            c.encrypt(&mut payload);
        }
        let now = Instant::now();
        let mut reanchor = self.anchor.is_none();
        if let Some(a) = &self.anchor {
            let due =
                a.at + Duration::from_nanos(a.frames * 1_000_000_000 / u64::from(SAMPLE_RATE));
            if now.saturating_duration_since(due) > self.cfg.reanchor_after {
                tracing::debug!(
                    "AirPlay: {:?} behind schedule, re-anchoring with FLUSH",
                    now.saturating_duration_since(due)
                );
                self.rtsp_flush(self.seq, self.ts).await?;
                reanchor = true;
            }
        }
        let mut marker = false;
        if reanchor {
            let anchor = Anchor {
                at: now,
                ts0: self.ts,
                frames: 0,
            };
            let sync = self.sync(true, &anchor, now);
            self.control
                .send_to(&sync, (self.remote, self.ports.control))
                .await?;
            self.anchor = Some(anchor);
            self.last_sync = Some(now);
            marker = true;
        } else if self
            .last_sync
            .is_none_or(|t| now.saturating_duration_since(t) >= SYNC_EVERY)
        {
            let anchor = self.anchor.as_ref().expect("anchored");
            let sync = self.sync(false, anchor, now);
            self.control
                .send_to(&sync, (self.remote, self.ports.control))
                .await?;
            self.last_sync = Some(now);
        }

        let packet = audio_packet(marker, self.seq, self.ts, self.ssrc, &payload);
        self.audio
            .send_to(&packet, (self.remote, self.ports.server))
            .await?;
        {
            let mut h = self.history.lock().unwrap();
            h.push_back((self.seq, packet));
            if h.len() > HISTORY {
                h.pop_front();
            }
        }
        if let Some(a) = &mut self.anchor {
            a.frames += FRAMES_PER_PACKET as u64;
        }
        self.seq = self.seq.wrapping_add(1);
        self.ts = self.ts.wrapping_add(FRAMES_PER_PACKET as u32);
        Ok(())
    }
}

#[async_trait]
impl PcmSink for RaopSink {
    async fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        if self.dead.load(Ordering::Relaxed) {
            return Err(aborted());
        }
        let mut pending = std::mem::take(&mut self.pending);
        self.resampler.process(bytes, &mut pending);
        let per_packet = FRAMES_PER_PACKET * 2;
        let mut used = 0;
        let mut result = Ok(());
        while pending.len() - used >= per_packet {
            let frame = &pending[used..used + per_packet];
            used += per_packet;
            if let Err(e) = self.send_packet(frame).await {
                result = Err(e);
                break;
            }
        }
        pending.drain(..used);
        self.pending = pending;
        result
    }

    async fn flush(&mut self) -> io::Result<()> {
        if self.dead.load(Ordering::Relaxed) {
            return Err(aborted());
        }
        self.rtsp_flush(self.seq, self.ts).await?;
        self.pending.clear();
        self.resampler.reset();
        self.anchor = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::super::fake::FakeReceiver;
    use super::super::packet::alac_raw_decode;
    use super::*;
    use crate::output::discovery::{AirplayDevice, Found};

    fn cfg() -> RaopConfig {
        RaopConfig {
            latency: Duration::from_secs(2),
            keepalive: Duration::from_secs(15),
            reanchor_after: Duration::from_secs(1),
        }
    }

    type Pauses = Arc<Mutex<Vec<External>>>;

    fn connector_for(
        address: String,
        discovery: Discovery,
        cfg: RaopConfig,
    ) -> (Arc<RaopConnector>, Pauses) {
        let seen: Pauses = Arc::default();
        let s = seen.clone();
        let external: Arc<dyn Fn(External) + Send + Sync> =
            Arc::new(move |e| s.lock().unwrap().push(e));
        (RaopConnector::new(address, discovery, cfg, external), seen)
    }

    fn connector(fake: &FakeReceiver, cfg: RaopConfig) -> (Arc<RaopConnector>, Pauses) {
        connector_for(fake.addr.to_string(), Discovery::none(), cfg)
    }

    async fn connect(c: &RaopConnector) -> Box<dyn PcmSink> {
        c.connect().await.expect("connect")
    }

    /// 48 kHz stereo s16le sine, `frames` long, starting at frame `from`.
    fn sine(hz: f64, from: usize, frames: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(frames * 4);
        for i in from..from + frames {
            let v =
                (10_000.0 * (2.0 * std::f64::consts::PI * hz * i as f64 / 48_000.0).sin()) as i16;
            out.extend_from_slice(&v.to_le_bytes());
            out.extend_from_slice(&v.to_le_bytes());
        }
        out
    }

    async fn wait_for(what: &str, within: Duration, cond: impl Fn() -> bool) {
        let deadline = Instant::now() + within;
        while !cond() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn reply_value(req: &crate::output::airplay::fake::Req) -> String {
        String::from_utf8(req.body.clone()).unwrap()
    }

    async fn write_sine(sink: &mut Box<dyn PcmSink>, ms: usize) {
        for i in 0..ms / 20 {
            sink.write(&sine(1000.0, i * 960, 960)).await.unwrap();
        }
    }

    #[tokio::test]
    async fn connect_handshake_order() {
        let fake = FakeReceiver::start(HashMap::new()).await;
        let (c, _) = connector(&fake, cfg());
        assert!(*c.closed().borrow(), "closed before any connect");
        let _sink = connect(&c).await;
        assert_eq!(
            fake.methods(),
            ["OPTIONS", "ANNOUNCE", "SETUP", "RECORD", "SET_PARAMETER"]
        );
        let reqs = fake.requests();
        assert_eq!(reply_value(&reqs[4]), "volume: -15.000000\r\n");
        let announce = reply_value(&reqs[1]);
        assert!(!announce.contains("rsaaeskey"), "{announce}");
        assert!(reqs[3].h("RTP-Info").unwrap().starts_with("seq="));
        assert!(!*c.closed().borrow());
        assert_eq!(c.volume().borrow().unwrap().level, 0.5);
    }

    #[tokio::test]
    async fn streams_resampled_pcm() {
        let fake = FakeReceiver::start(HashMap::new()).await;
        let (c, _) = connector(&fake, cfg());
        let mut sink = connect(&c).await;
        write_sine(&mut sink, 1000).await;
        tokio::time::sleep(Duration::from_millis(200)).await;

        let packets = fake.packets();
        assert!(
            (124..=126).contains(&packets.len()),
            "{} packets",
            packets.len()
        );
        assert!(packets[0].0, "first packet has the marker");
        assert!(packets[1..].iter().all(|p| !p.0), "only the first");
        for w in packets.windows(2) {
            assert_eq!(w[1].1, w[0].1.wrapping_add(1), "seq contiguous");
            assert_eq!(w[1].2, w[0].2.wrapping_add(352), "ts steps of 352");
        }
        let pcm = fake.pcm();
        assert_eq!(pcm.len(), packets.len() * 704);
        let left: Vec<i16> = pcm.iter().step_by(2).copied().collect();
        let mut crossings = 0;
        let mut sign = 0i8;
        for v in &left {
            let s = match v {
                0 => continue,
                v if *v > 0 => 1,
                _ => -1,
            };
            if sign != 0 && s != sign {
                crossings += 1;
            }
            sign = s;
        }
        let secs = left.len() as f64 / 44_100.0;
        let hz = f64::from(crossings) / 2.0 / secs;
        assert!((hz - 1000.0).abs() <= 10.0, "{hz:.1} Hz");
    }

    #[tokio::test]
    async fn sends_sync_packets() {
        let fake = FakeReceiver::start(HashMap::new()).await;
        let (c, _) = connector(&fake, cfg());
        let mut sink = connect(&c).await;
        let start = Instant::now();
        // 1.3 s of audio in real time.
        for i in 0..65 {
            sink.write(&sine(1000.0, i * 960, 960)).await.unwrap();
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(start.elapsed() < Duration::from_secs(3));
        tokio::time::sleep(Duration::from_millis(100)).await;
        let syncs = fake.syncs();
        let first_ts = fake.packets()[0].2;
        assert_eq!(syncs[0].0, 0x90);
        assert_eq!(syncs[0].2, first_ts, "now_ts is the first packet's ts");
        assert_eq!(
            syncs[0].1,
            first_ts.wrapping_sub(88_200),
            "ts_now - latency"
        );
        let second = syncs.get(1).expect("a second sync within 1.2 s");
        assert_eq!(second.0, 0x80);
        let advanced = second.2.wrapping_sub(first_ts);
        assert!(
            (44_100..=44_100 + 8_000).contains(&advanced),
            "{advanced} frames since the anchor"
        );
    }

    #[tokio::test]
    async fn answers_timing_requests() {
        let fake = FakeReceiver::start(HashMap::new()).await;
        let (c, _) = connector(&fake, cfg());
        let _sink = connect(&c).await;
        let reply = fake.ask_timing().await.expect("timing reply");
        assert_eq!(&reply[..4], &[0x80, 0xd3, 0x00, 0x07]);
        assert_eq!(
            &reply[8..16],
            &[1, 2, 3, 4, 5, 6, 7, 8],
            "echoes the origin"
        );
        assert_ne!(&reply[16..24], &[0u8; 8], "receive time");
        assert_ne!(&reply[24..32], &[0u8; 8], "send time");
    }

    #[tokio::test]
    async fn answers_resend_requests() {
        let fake = FakeReceiver::start(HashMap::new()).await;
        let (c, _) = connector(&fake, cfg());
        let mut sink = connect(&c).await;
        write_sine(&mut sink, 200).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        let packets = fake.packets();
        assert!(packets.len() >= 4);
        let (_, seq, ts, payload) = packets[3].clone();
        let replies = fake.ask_resend(seq, 1).await;
        assert_eq!(replies.len(), 1);
        assert_eq!(&replies[0][..4], &[0x80, 0xd6, 0, 1]);
        let original = audio_packet(
            false,
            seq,
            ts,
            u32::from_be_bytes(replies[0][12..16].try_into().unwrap()),
            &payload,
        );
        assert_eq!(&replies[0][4..], original.as_slice());
        // Two in a row, and one that was never sent.
        assert_eq!(fake.ask_resend(packets[0].1, 2).await.len(), 2);
        assert!(fake.ask_resend(seq.wrapping_add(5_000), 1).await.is_empty());
    }

    #[tokio::test]
    async fn encrypted_when_device_needs_it() {
        let fake = FakeReceiver::start(HashMap::new()).await;
        let (discovery, found) = Discovery::manual();
        found.send_replace(Found {
            airplay: vec![AirplayDevice {
                id: "0A1B2C3D4E5F".into(),
                name: "Kitchen".into(),
                model: None,
                addr: fake.addr,
                encrypt: true,
            }],
            ..Found::default()
        });
        let (c, _) = connector_for("0A1B2C3D4E5F".into(), discovery, cfg());
        let mut sink = connect(&c).await;
        let input = sine(1000.0, 0, 960);
        sink.write(&input).await.unwrap();
        sink.write(&sine(1000.0, 960, 960)).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        let announce = reply_value(&fake.requests()[1]);
        let key = announce
            .lines()
            .find_map(|l| l.strip_prefix("a=rsaaeskey:"))
            .expect("rsaaeskey");
        use base64::Engine;
        let key = base64::engine::general_purpose::STANDARD_NO_PAD
            .decode(key)
            .unwrap();
        assert_eq!(key.len(), 256);
        assert!(announce.contains("a=aesiv:"));

        let mut samples = Vec::new();
        Resampler::new().process(&input, &mut samples);
        let plain = alac_raw(&samples[..704]);
        let payload = &fake.packets()[0].3;
        assert_eq!(payload.len(), plain.len());
        assert_ne!(payload[..16], plain[..16], "first block is encrypted");
        let tail = plain.len() - plain.len() % 16;
        assert_eq!(payload[tail..], plain[tail..], "the short tail stays clear");
        assert!(
            alac_raw_decode(payload).is_none()
                || alac_raw_decode(payload).unwrap() != samples[..704]
        );
    }

    #[tokio::test]
    async fn flush_sends_rtsp_flush_and_reanchors() {
        let fake = FakeReceiver::start(HashMap::new()).await;
        let (c, _) = connector(&fake, cfg());
        let mut sink = connect(&c).await;
        write_sine(&mut sink, 200).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        let last = fake.packets().last().unwrap().clone();
        let (next_seq, next_ts) = (last.1.wrapping_add(1), last.2.wrapping_add(352));

        sink.flush().await.unwrap();
        let reqs = fake.requests();
        let flush = reqs.iter().find(|r| r.method == "FLUSH").expect("FLUSH");
        assert_eq!(
            flush.h("RTP-Info"),
            Some(format!("seq={next_seq};rtptime={next_ts}").as_str())
        );

        let before = fake.packets().len();
        let syncs_before = fake.syncs().len();
        write_sine(&mut sink, 100).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        let packets = fake.packets();
        let after = &packets[before..];
        assert!(after[0].0, "marker after FLUSH");
        assert_eq!(after[0].1, next_seq, "seq continues");
        assert_eq!(after[0].2, next_ts, "ts continues");
        assert!(after[1..].iter().all(|p| !p.0));
        let syncs = fake.syncs();
        assert_eq!(syncs[syncs_before].0, 0x90, "a first-sync follows");
        assert_eq!(syncs[syncs_before].2, next_ts);
    }

    #[tokio::test]
    async fn late_write_reanchors_with_flush() {
        let fake = FakeReceiver::start(HashMap::new()).await;
        let (c, _) = connector(&fake, cfg());
        let mut sink = connect(&c).await;
        write_sine(&mut sink, 100).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!fake.methods().contains(&"FLUSH".to_string()));
        let before = fake.packets().len();
        tokio::time::sleep(Duration::from_millis(1_200)).await;
        write_sine(&mut sink, 100).await;
        tokio::time::sleep(Duration::from_millis(100)).await;

        assert_eq!(fake.methods().iter().filter(|m| *m == "FLUSH").count(), 1);
        let packets = fake.packets();
        assert!(packets[before].0, "marker on the first late packet");
        assert!(packets[before + 1..].iter().all(|p| !p.0));
    }

    #[tokio::test]
    async fn drop_sends_teardown_and_closes() {
        let fake = FakeReceiver::start(HashMap::new()).await;
        let (c, _) = connector(&fake, cfg());
        let sink = connect(&c).await;
        let mut closed = c.closed();
        assert!(!*closed.borrow());
        drop(sink);
        timeout(Duration::from_secs(3), closed.wait_for(|c| *c))
            .await
            .expect("closed within 3 s")
            .unwrap();
        assert_eq!(fake.methods().last().unwrap(), "TEARDOWN");
    }

    #[tokio::test]
    async fn busy_receiver_pauses_player() {
        let fake = FakeReceiver::start(HashMap::from([("ANNOUNCE", 453)])).await;
        let (c, seen) = connector(&fake, cfg());
        assert!(c.connect().await.is_err());
        assert_eq!(*seen.lock().unwrap(), [External::Pause]);
        assert!(*c.closed().borrow());
    }

    #[tokio::test]
    async fn pairing_required_pauses_player() {
        let fake = FakeReceiver::start(HashMap::from([("ANNOUNCE", 470)])).await;
        let (c, seen) = connector(&fake, cfg());
        assert!(c.connect().await.is_err());
        assert_eq!(*seen.lock().unwrap(), [External::Pause]);
    }

    #[tokio::test]
    async fn server_error_retries_without_pause() {
        let fake = FakeReceiver::start(HashMap::from([("ANNOUNCE", 500)])).await;
        let (c, seen) = connector(&fake, cfg());
        assert!(c.connect().await.is_err());
        assert!(seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn unknown_target_is_not_found() {
        let (c, seen) = connector_for("no-such-speaker.invalid".into(), Discovery::none(), cfg());
        let e = c.connect().await.err().expect("error");
        assert_eq!(e.kind(), io::ErrorKind::NotFound);
        assert!(e.to_string().contains("hasn't been found on the network"));
        assert!(seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn takeover_closes_and_pauses() {
        let fake = FakeReceiver::start(HashMap::new()).await;
        let (c, seen) = connector(&fake, cfg());
        let mut sink = connect(&c).await;
        sink.write(&sine(1000.0, 0, 960)).await.unwrap();
        fake.close_rtsp();
        timeout(Duration::from_secs(1), async {
            loop {
                match sink.write(&sine(1000.0, 0, 960)).await {
                    Ok(()) => tokio::time::sleep(Duration::from_millis(20)).await,
                    Err(e) => return e,
                }
            }
        })
        .await
        .map(|e| assert_eq!(e.kind(), io::ErrorKind::ConnectionAborted))
        .expect("write fails within 1 s");
        assert_eq!(*seen.lock().unwrap(), [External::Pause]);
        assert!(*c.closed().borrow());
    }

    #[tokio::test]
    async fn keepalive_failure_drops_link_without_pause() {
        let fake = FakeReceiver::start_with(HashMap::new(), true).await;
        let mut short = cfg();
        short.keepalive = Duration::from_millis(300);
        let (c, seen) = connector(&fake, short);
        let mut sink = connect(&c).await;
        timeout(Duration::from_secs(4), async {
            loop {
                if sink.write(&sine(1000.0, 0, 960)).await.is_err() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("write fails within 4 s");
        assert!(seen.lock().unwrap().is_empty());
        assert!(*c.closed().borrow());
    }

    #[tokio::test]
    async fn volume_set_live_and_offline() {
        let fake = FakeReceiver::start(HashMap::new()).await;
        let (c, _) = connector(&fake, cfg());
        let sink = connect(&c).await;
        c.set_volume(Some(0.75), None).await.unwrap();
        c.set_volume(None, Some(true)).await.unwrap();
        let params: Vec<String> = fake
            .requests()
            .iter()
            .filter(|r| r.method == "SET_PARAMETER")
            .map(reply_value)
            .collect();
        assert_eq!(
            params,
            [
                "volume: -15.000000\r\n",
                "volume: -7.500000\r\n",
                "volume: -144.000000\r\n"
            ]
        );
        assert_eq!(
            *c.volume().borrow(),
            Some(Volume {
                level: 0.75,
                muted: true
            })
        );

        let mut closed = c.closed();
        drop(sink);
        timeout(Duration::from_secs(3), closed.wait_for(|c| *c))
            .await
            .unwrap()
            .unwrap();
        c.set_volume(Some(0.2), Some(false)).await.unwrap();
        let _sink = connect(&c).await;
        let last = fake
            .requests()
            .iter()
            .filter(|r| r.method == "SET_PARAMETER")
            .map(reply_value)
            .next_back()
            .unwrap();
        assert_eq!(last, "volume: -24.000000\r\n");
    }

    #[tokio::test]
    async fn volume_after_session_ends_is_kept() {
        let fake = FakeReceiver::start(HashMap::new()).await;
        let (c, _) = connector(&fake, cfg());
        let _sink = connect(&c).await;
        // The speaker goes away mid-session.
        fake.close_rtsp();
        wait_for("session end", Duration::from_secs(2), || {
            *c.closed().borrow()
        })
        .await;
        // With no session the level is kept for the next connect.
        assert!(c.set_volume(Some(0.3), None).await.is_ok());
    }

    #[test]
    fn reanchor_threshold_scales_with_latency_config() {
        assert_eq!(
            RaopConfig::new(Duration::from_millis(500)).reanchor_after,
            Duration::from_millis(250)
        );
        assert_eq!(
            RaopConfig::new(Duration::from_secs(2)).reanchor_after,
            Duration::from_secs(1)
        );
    }

    #[tokio::test]
    async fn reanchor_threshold_scales_with_latency() {
        let fake = FakeReceiver::start(HashMap::new()).await;
        let (c, _) = connector(&fake, RaopConfig::new(Duration::from_millis(500)));
        let mut sink = connect(&c).await;
        write_sine(&mut sink, 100).await;
        tokio::time::sleep(Duration::from_millis(700)).await;
        let before = fake.packets().len();
        write_sine(&mut sink, 100).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(fake.methods().iter().filter(|m| *m == "FLUSH").count(), 1);
        assert!(fake.packets()[before].0, "marker on the late packet");
    }

    #[tokio::test]
    async fn udp_tasks_ignore_other_senders() {
        let mut req = [0u8; 32];
        req[..4].copy_from_slice(&[0x80, 0xd2, 0x00, 0x07]);
        for (remote, answered) in [("10.0.0.9", false), ("127.0.0.1", true)] {
            let sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
            let port = sock.local_addr().unwrap().port();
            let task = tokio::spawn(timing_task(sock, remote.parse().unwrap()));
            let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            client.send_to(&req, ("127.0.0.1", port)).await.unwrap();
            let mut buf = [0u8; 64];
            let got = timeout(Duration::from_millis(300), client.recv_from(&mut buf)).await;
            assert_eq!(got.is_ok(), answered, "remote {remote}");

            // The control task: a resend request from the wrong host.
            let sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
            let port = sock.local_addr().unwrap().port();
            let history: History = Arc::default();
            history
                .lock()
                .unwrap()
                .push_back((5, vec![0x80, 0x60, 0, 5, 0, 0, 0, 0, 0, 0, 0, 0]));
            let ctl = tokio::spawn(control_task(sock, history, remote.parse().unwrap()));
            let mut resend = vec![0x80, 0xd5, 0x00, 0x01];
            resend.extend_from_slice(&5u16.to_be_bytes());
            resend.extend_from_slice(&1u16.to_be_bytes());
            client.send_to(&resend, ("127.0.0.1", port)).await.unwrap();
            let got = timeout(Duration::from_millis(300), client.recv_from(&mut buf)).await;
            assert_eq!(got.is_ok(), answered, "control, remote {remote}");
            task.abort();
            ctl.abort();
        }
    }

    #[tokio::test]
    async fn keepalive_453_pauses_player() {
        let fake = FakeReceiver::start_late(HashMap::from([("OPTIONS", 453)])).await;
        let mut short = cfg();
        short.keepalive = Duration::from_millis(300);
        let (c, seen) = connector(&fake, short);
        let mut sink = connect(&c).await;
        timeout(Duration::from_secs(4), async {
            loop {
                if sink.write(&sine(1000.0, 0, 960)).await.is_err() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("write fails within 4 s");
        assert_eq!(*seen.lock().unwrap(), [External::Pause]);
        assert!(*c.closed().borrow());
    }

    /// Manual: `shairport-sync --service-type classic -p 5100 -o stdout`, then
    /// `RAOP_TEST_ADDR=127.0.0.1:5100 cargo test --lib live_receiver -- --ignored`.
    /// `RAOP_TEST_ENCRYPT=1` forces the AES path.
    #[tokio::test]
    #[ignore]
    async fn live_receiver() {
        use crate::engine::paced::{PacedConfig, PacedEngine};
        use crate::engine::{AudioEngine, EngineEvent};
        use crate::source::PcmReader;

        struct Tone(usize);
        #[async_trait]
        impl PcmReader for Tone {
            async fn read(&mut self, buf: &mut [u8]) -> anyhow::Result<usize> {
                if self.0 >= 96_000 {
                    return Ok(0);
                }
                let frames = (buf.len() / 4).min(960);
                let out = sine(440.0, self.0, frames);
                buf[..out.len()].copy_from_slice(&out);
                self.0 += frames;
                Ok(out.len())
            }
        }

        let Ok(addr) = std::env::var("RAOP_TEST_ADDR") else {
            eprintln!("RAOP_TEST_ADDR not set; nothing to do");
            return;
        };
        let (discovery, found) = Discovery::manual();
        let mut address = addr.clone();
        if std::env::var("RAOP_TEST_ENCRYPT").is_ok_and(|v| v == "1") {
            found.send_replace(Found {
                airplay: vec![AirplayDevice {
                    id: "0A1B2C3D4E5F".into(),
                    name: "Live".into(),
                    model: None,
                    addr: addr.parse().expect("RAOP_TEST_ADDR is host:port"),
                    encrypt: true,
                }],
                ..Found::default()
            });
            address = "0A1B2C3D4E5F".into();
        }
        let (c, _) = connector_for(address, discovery, RaopConfig::new(Duration::from_secs(2)));
        let (tx, mut events) = mpsc::unbounded_channel();
        let mut engine = PacedEngine::spawn(
            PacedConfig {
                label: "airplay",
                connector: c,
                max_ahead: Duration::from_millis(200),
                latency: Duration::from_secs(2),
                resend_on_resume: true,
                flush_on_play: true,
            },
            tx,
        );
        engine.play(Box::new(Tone(0)), 1).unwrap();
        let ev = timeout(Duration::from_secs(10), events.recv())
            .await
            .expect("track ends")
            .expect("open");
        assert_eq!(ev, EngineEvent::TrackEnded { generation: 1 });
    }
}
