//! Paced PCM output: s16le / 48 kHz / stereo PCM into a pluggable sink.
//!
//! The receiver plays what it gets on its own clock, so pacing is the
//! sender's job: write at the real-time rate and stay at most `max_ahead`
//! in front of it. A [`Connector`] opens the sink (TCP for nidoo-sound).

use std::collections::VecDeque;
use std::future::pending;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::future::BoxFuture;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep_until, timeout};

use super::{AudioEngine, EngineEvent};
use crate::source::PcmReader;

/// 48,000 samples × 2 channels × 2 bytes.
const BYTES_PER_SEC: u64 = 192_000;
/// 20 ms of audio, the unit we write.
const FRAME: usize = 3_840;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);
/// While there is no sink, the writer may run at most this far ahead of real
/// time, so a link drop only costs that much of the song.
const DOWN_AHEAD: Duration = Duration::from_millis(200);
const BACKOFF_MIN: Duration = Duration::from_millis(500);
const BACKOFF_MAX: Duration = Duration::from_secs(5);

/// Where paced bytes go.
#[async_trait]
pub trait PcmSink: Send {
    async fn write(&mut self, bytes: &[u8]) -> io::Result<()>;

    /// Drops what the receiver has buffered but not played. Only called
    /// when `PacedConfig::flush_on_play` is set.
    async fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Opens sinks; called again after every drop.
pub trait Connector: Send + Sync + 'static {
    /// Shown in logs, e.g. `192.168.1.91:4953`.
    fn target(&self) -> String;
    fn connect(&self) -> BoxFuture<'static, io::Result<Box<dyn PcmSink>>>;
}

/// A TCP peer, e.g. nidoo-sound's PCM input.
pub struct TcpConnector {
    pub addr: String,
}

struct TcpSink(TcpStream);

#[async_trait]
impl PcmSink for TcpSink {
    async fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.0.write_all(bytes).await
    }
}

impl Connector for TcpConnector {
    fn target(&self) -> String {
        self.addr.clone()
    }

    fn connect(&self) -> BoxFuture<'static, io::Result<Box<dyn PcmSink>>> {
        let addr = self.addr.clone();
        Box::pin(async move {
            let stream = match timeout(CONNECT_TIMEOUT, TcpStream::connect(&addr)).await {
                Ok(res) => res?,
                Err(_) => {
                    return Err(io::Error::new(io::ErrorKind::TimedOut, "connect timed out"));
                }
            };
            stream.set_nodelay(true).ok();
            Ok(Box::new(TcpSink(stream)) as Box<dyn PcmSink>)
        })
    }
}

#[derive(Clone)]
pub struct PacedConfig {
    /// Log prefix, e.g. `nidoo-sound`.
    pub label: &'static str,
    pub connector: Arc<dyn Connector>,
    /// How far in front of real time the writer may get.
    pub max_ahead: Duration,
    /// From a written byte to the speakers; also how much is re-sent on resume.
    pub latency: Duration,
    /// Re-send the last `latency` on resume (the receiver dropped it on pause).
    pub resend_on_resume: bool,
    /// A new song while one is playing makes the sink drop what it has
    /// buffered (AirPlay's FLUSH), so a skip is heard at once.
    pub flush_on_play: bool,
}

impl PacedConfig {
    pub fn tcp(label: &'static str, addr: String, latency: Duration) -> Self {
        Self {
            label,
            connector: Arc::new(TcpConnector { addr }),
            max_ahead: Duration::from_millis(200),
            latency,
            resend_on_resume: true,
            flush_on_play: false,
        }
    }
}

enum Cmd {
    Play(Box<dyn PcmReader>, u64),
    Pause,
    Resume,
    Stop,
}

/// Drives a writer task; dropping the engine ends the task and closes the socket.
pub struct PacedEngine {
    cmds: mpsc::UnboundedSender<Cmd>,
    label: &'static str,
    latency: Duration,
    link: watch::Receiver<Option<bool>>,
}

impl PacedEngine {
    pub fn spawn(cfg: PacedConfig, events: mpsc::UnboundedSender<EngineEvent>) -> Self {
        let (cmds, rx) = mpsc::unbounded_channel();
        let latency = cfg.latency;
        let label = cfg.label;
        let (link_tx, link) = watch::channel(None);
        tokio::spawn(Writer::new(cfg, events, rx, link_tx).run());
        Self {
            cmds,
            label,
            latency,
            link,
        }
    }

    /// Whether the receiver is reachable: `None` until the first connect
    /// result, then `Some(true)` after a connect and `Some(false)` after a
    /// failure or drop. A pause or stop leaves it as it was.
    pub fn link(&self) -> watch::Receiver<Option<bool>> {
        self.link.clone()
    }

    fn send(&self, cmd: Cmd) -> anyhow::Result<()> {
        self.cmds
            .send(cmd)
            .map_err(|_| anyhow::anyhow!("{} writer has stopped", self.label))
    }
}

impl AudioEngine for PacedEngine {
    fn play(&mut self, reader: Box<dyn PcmReader>, generation: u64) -> anyhow::Result<()> {
        self.send(Cmd::Play(reader, generation))
    }

    fn pause(&mut self) -> anyhow::Result<()> {
        self.send(Cmd::Pause)
    }

    fn resume(&mut self) -> anyhow::Result<()> {
        self.send(Cmd::Resume)
    }

    fn stop(&mut self) -> anyhow::Result<()> {
        self.send(Cmd::Stop)
    }

    fn latency(&self) -> Duration {
        self.latency
    }
}

fn audio_len(bytes: u64) -> Duration {
    Duration::from_micros(bytes * 1_000_000 / BYTES_PER_SEC)
}

/// Whole milliseconds, so always a whole number of 4-byte samples.
fn byte_len(d: Duration) -> usize {
    (d.as_millis() as u64 * BYTES_PER_SEC / 1_000) as usize
}

struct Track {
    reader: Box<dyn PcmReader>,
    generation: u64,
    eof: bool,
}

enum Wake {
    Cmd(Option<Cmd>),
    Read(anyhow::Result<usize>),
    Connected(io::Result<Box<dyn PcmSink>>),
    Due,
}

type Connect = JoinHandle<io::Result<Box<dyn PcmSink>>>;

struct Writer {
    label: &'static str,
    connector: Arc<dyn Connector>,
    resend_on_resume: bool,
    flush_on_play: bool,
    /// A skip happened: flush the sink before the next write.
    flush_pending: bool,
    max_ahead: Duration,
    ring_cap: usize,
    events: mpsc::UnboundedSender<EngineEvent>,
    cmds: mpsc::UnboundedReceiver<Cmd>,
    link: watch::Sender<Option<bool>>,
    track: Option<Track>,
    paused: bool,
    // Link: `stream` up, `connecting` in flight, `retry_at` down, or all
    // empty = closed (paused, stopped, never opened).
    stream: Option<Box<dyn PcmSink>>,
    connecting: Option<Connect>,
    /// The connect in flight opens a stream (first play, resume): wait for it
    /// rather than drop audio.
    connecting_fresh: bool,
    retry_at: Option<Instant>,
    backoff: Duration,
    /// Pacing: `clock_bytes` written since `clock_start`.
    clock_start: Instant,
    clock_bytes: u64,
    /// Read but not yet written; written once it holds a whole frame.
    chunk: Vec<u8>,
    chunk_ready: bool,
    /// Written before a pause, never heard: re-sent first on resume.
    resend: VecDeque<u8>,
    /// The last `latency` of written audio.
    ring: VecDeque<u8>,
    buf: Vec<u8>,
}

/// Awaits the connect in flight, or never.
async fn connected(connecting: &mut Option<Connect>) -> io::Result<Box<dyn PcmSink>> {
    match connecting {
        Some(task) => task.await.unwrap_or_else(|e| Err(io::Error::other(e))),
        None => pending().await,
    }
}

impl Writer {
    fn new(
        cfg: PacedConfig,
        events: mpsc::UnboundedSender<EngineEvent>,
        cmds: mpsc::UnboundedReceiver<Cmd>,
        link: watch::Sender<Option<bool>>,
    ) -> Self {
        Self {
            label: cfg.label,
            connector: cfg.connector,
            resend_on_resume: cfg.resend_on_resume,
            flush_on_play: cfg.flush_on_play,
            flush_pending: false,
            max_ahead: cfg.max_ahead,
            ring_cap: byte_len(cfg.latency),
            events,
            cmds,
            link,
            track: None,
            paused: false,
            stream: None,
            connecting: None,
            connecting_fresh: false,
            retry_at: None,
            backoff: BACKOFF_MIN,
            clock_start: Instant::now(),
            clock_bytes: 0,
            chunk: Vec::with_capacity(FRAME),
            chunk_ready: false,
            resend: VecDeque::new(),
            ring: VecDeque::new(),
            buf: vec![0; FRAME],
        }
    }

    async fn run(mut self) {
        loop {
            if self.track.is_none() || self.paused {
                match self.cmds.recv().await {
                    Some(cmd) => self.handle(cmd),
                    None => return,
                }
                continue;
            }
            if self.stream.is_none() && self.connecting.is_none() {
                match self.retry_at {
                    None => self.start_connect(true),
                    Some(at) if Instant::now() >= at => self.start_connect(false),
                    Some(_) => {}
                }
            }
            let has_bytes = !self.resend.is_empty() || self.chunk_ready;
            let track = self.track.as_mut().expect("playing");
            if !has_bytes && track.eof {
                let generation = track.generation;
                self.track = None;
                self.events
                    .send(EngineEvent::TrackEnded { generation })
                    .ok();
                continue;
            }

            let wake = if self.connecting_fresh && self.connecting.is_some() {
                tokio::select! {
                    cmd = self.cmds.recv() => Wake::Cmd(cmd),
                    res = connected(&mut self.connecting) => Wake::Connected(res),
                }
            } else if !has_bytes {
                let want = FRAME - self.chunk.len();
                tokio::select! {
                    cmd = self.cmds.recv() => Wake::Cmd(cmd),
                    n = track.reader.read(&mut self.buf[..want]) => Wake::Read(n),
                    res = connected(&mut self.connecting) => Wake::Connected(res),
                }
            } else {
                let due = self.due();
                tokio::select! {
                    cmd = self.cmds.recv() => Wake::Cmd(cmd),
                    () = sleep_until(due) => Wake::Due,
                    res = connected(&mut self.connecting) => Wake::Connected(res),
                }
            };

            match wake {
                Wake::Cmd(None) => return,
                Wake::Cmd(Some(cmd)) => self.handle(cmd),
                Wake::Read(Ok(0)) => {
                    self.track.as_mut().expect("playing").eof = true;
                    if !self.chunk.is_empty() {
                        // Pad the last partial frame with silence.
                        self.chunk.resize(FRAME, 0);
                        self.chunk_ready = true;
                    }
                }
                Wake::Read(Ok(n)) => {
                    self.chunk.extend_from_slice(&self.buf[..n]);
                    self.chunk_ready = self.chunk.len() == FRAME;
                }
                Wake::Read(Err(e)) => {
                    let generation = self.track.take().expect("playing").generation;
                    self.chunk.clear();
                    self.chunk_ready = false;
                    self.events
                        .send(EngineEvent::TrackFailed {
                            generation,
                            reason: format!("{e:#}"),
                        })
                        .ok();
                }
                Wake::Connected(res) => self.on_connected(res),
                Wake::Due => {
                    self.flush_if_pending().await;
                    self.write_next().await;
                }
            }
        }
    }

    fn handle(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Play(reader, generation) => {
                if self.flush_on_play && self.track.is_some() && !self.paused {
                    // A skip: what the receiver holds is the old song.
                    self.ring.clear();
                    self.flush_pending = true;
                }
                if self.paused {
                    // A new song while paused starts at once; the unheard tail
                    // of the old one is not wanted.
                    self.paused = false;
                    self.ring.clear();
                }
                self.resend.clear();
                self.chunk.clear();
                self.chunk_ready = false;
                self.track = Some(Track {
                    reader,
                    generation,
                    eof: false,
                });
            }
            Cmd::Pause => {
                if !self.paused {
                    self.paused = true;
                    self.close();
                }
            }
            Cmd::Resume => {
                if self.paused {
                    self.paused = false;
                    if self.track.is_some() && self.resend_on_resume {
                        self.resend = std::mem::take(&mut self.ring);
                    } else {
                        self.ring.clear();
                    }
                }
            }
            Cmd::Stop => {
                self.track = None;
                self.paused = false;
                self.close();
                self.ring.clear();
                self.resend.clear();
                self.chunk.clear();
                self.chunk_ready = false;
            }
        }
    }

    fn close(&mut self) {
        if self.stream.take().is_some() {
            tracing::info!("{}: closed {}", self.label, self.connector.target());
        }
        if let Some(task) = self.connecting.take() {
            task.abort();
        }
        self.connecting_fresh = false;
        self.retry_at = None;
        self.flush_pending = false;
    }

    fn start_connect(&mut self, fresh: bool) {
        self.retry_at = None;
        self.connecting_fresh = fresh;
        self.connecting = Some(tokio::spawn(self.connector.connect()));
    }

    fn on_connected(&mut self, res: io::Result<Box<dyn PcmSink>>) {
        self.connecting = None;
        let fresh = std::mem::take(&mut self.connecting_fresh);
        if fresh {
            self.clock_start = Instant::now();
            self.clock_bytes = 0;
        }
        match res {
            Ok(stream) => {
                tracing::info!("{}: connected to {}", self.label, self.connector.target());
                self.stream = Some(stream);
                self.link.send_replace(Some(true));
                self.backoff = BACKOFF_MIN;
            }
            Err(e) => self.link_down(&format!("connect to {}: {e}", self.connector.target())),
        }
    }

    /// Drops the connection and schedules a retry; audio keeps being consumed
    /// in real time meanwhile, so the position stays honest.
    fn link_down(&mut self, why: &str) {
        tracing::warn!(
            "{}: {why}; retrying in {:.1} s",
            self.label,
            self.backoff.as_secs_f32()
        );
        self.stream = None;
        self.link.send_replace(Some(false));
        self.retry_at = Some(Instant::now() + self.backoff);
        self.backoff = (self.backoff * 2).min(BACKOFF_MAX);
    }

    /// When the next bytes may be written: once they would put us no more
    /// than `max_ahead` in front of real time. Behind real time (a stall, an
    /// idle gap) the clock restarts instead of bursting to catch up.
    fn due(&mut self) -> Instant {
        let now = Instant::now();
        let played_out = self.clock_start + audio_len(self.clock_bytes);
        if played_out < now {
            self.clock_start = now;
            self.clock_bytes = 0;
            return now;
        }
        let ahead = if self.stream.is_some() {
            self.max_ahead
        } else {
            self.max_ahead.min(DOWN_AHEAD)
        };
        played_out.checked_sub(ahead).unwrap_or(now)
    }

    async fn flush_if_pending(&mut self) {
        if !std::mem::take(&mut self.flush_pending) {
            return;
        }
        if let Some(stream) = &mut self.stream {
            match timeout(WRITE_TIMEOUT, stream.flush()).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => self.link_down(&format!("flush: {e}")),
                Err(_) => self.link_down("flush timed out"),
            }
            self.clock_start = Instant::now();
            self.clock_bytes = 0;
        }
    }

    async fn write_next(&mut self) {
        let bytes: Vec<u8> = if self.resend.is_empty() {
            self.chunk_ready = false;
            std::mem::replace(&mut self.chunk, Vec::with_capacity(FRAME))
        } else {
            let n = self.resend.len().min(FRAME);
            self.resend.drain(..n).collect()
        };
        if let Some(stream) = &mut self.stream {
            match timeout(WRITE_TIMEOUT, stream.write(&bytes)).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => self.link_down(&format!("write: {e}")),
                Err(_) => self.link_down("write timed out"),
            }
        }
        self.clock_bytes += bytes.len() as u64;
        self.ring.extend(&bytes);
        let excess = self.ring.len().saturating_sub(self.ring_cap);
        self.ring.drain(..excess);
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use async_trait::async_trait;
    use tokio::io::AsyncReadExt;
    use tokio::net::TcpListener;
    use tokio::sync::mpsc;
    use tokio::task::AbortHandle;
    use tokio::time::{Instant, sleep, timeout};

    use super::*;
    use crate::engine::{AudioEngine, EngineEvent};
    use crate::source::PcmReader;

    const BPS: f64 = 192_000.0;
    const RING: usize = 144_000; // 750 ms

    /// Sample `i` is the u32 `i` (little endian), so gaps and repeats show.
    struct PatternReader {
        next: u32,
        end: u32,
    }

    fn pattern(first_sample: u32, secs: f64) -> Box<dyn PcmReader> {
        Box::new(PatternReader {
            next: first_sample,
            end: first_sample + (secs * 48_000.0) as u32,
        })
    }

    #[async_trait]
    impl PcmReader for PatternReader {
        async fn read(&mut self, buf: &mut [u8]) -> anyhow::Result<usize> {
            let mut n = 0;
            // Odd-sized reads, like a pipe.
            while n + 4 <= buf.len().min(1_000) && self.next < self.end {
                buf[n..n + 4].copy_from_slice(&self.next.to_le_bytes());
                self.next += 1;
                n += 4;
            }
            Ok(n)
        }
    }

    #[derive(Debug)]
    enum Seen {
        Accepted(usize, Instant),
        Data(usize, Instant, Vec<u8>),
        Closed(usize),
    }

    /// A TCP listener standing in for nidoo-sound.
    struct FakeNidoo {
        addr: SocketAddr,
        tasks: Arc<Mutex<Vec<AbortHandle>>>,
    }

    impl FakeNidoo {
        async fn start(addr: &str) -> (Self, mpsc::UnboundedReceiver<Seen>) {
            let listener = TcpListener::bind(addr).await.unwrap();
            let addr = listener.local_addr().unwrap();
            let (tx, rx) = mpsc::unbounded_channel();
            let tasks = Arc::new(Mutex::new(Vec::new()));
            let conn_tasks = tasks.clone();
            let accept = tokio::spawn(async move {
                let mut next_id = 0;
                loop {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    let id = next_id;
                    next_id += 1;
                    tx.send(Seen::Accepted(id, Instant::now())).ok();
                    let tx = tx.clone();
                    let conn = tokio::spawn(async move {
                        let mut buf = vec![0; 65_536];
                        loop {
                            match socket.read(&mut buf).await {
                                Ok(0) | Err(_) => {
                                    tx.send(Seen::Closed(id)).ok();
                                    break;
                                }
                                Ok(n) => {
                                    tx.send(Seen::Data(id, Instant::now(), buf[..n].to_vec()))
                                        .ok();
                                }
                            }
                        }
                    });
                    conn_tasks.lock().unwrap().push(conn.abort_handle());
                }
            });
            tasks.lock().unwrap().push(accept.abort_handle());
            (Self { addr, tasks }, rx)
        }

        /// Drops the listener and every connection, like nidoo-sound restarting.
        fn kill(&self) {
            for task in self.tasks.lock().unwrap().drain(..) {
                task.abort();
            }
        }
    }

    fn cfg(addr: SocketAddr) -> PacedConfig {
        PacedConfig::tcp("nidoo-sound", addr.to_string(), Duration::from_millis(750))
    }

    /// Receives into `log` until `done(log)` holds; panics after `within`.
    async fn until(
        seen: &mut mpsc::UnboundedReceiver<Seen>,
        log: &mut Vec<Seen>,
        within: Duration,
        done: impl Fn(&[Seen]) -> bool,
    ) {
        let deadline = Instant::now() + within;
        while !done(log) {
            match timeout(deadline - Instant::now(), seen.recv()).await {
                Ok(Some(s)) => log.push(s),
                _ => panic!(
                    "condition not met within {within:?}; log: {} items",
                    log.len()
                ),
            }
        }
    }

    /// Receives everything for `dur`.
    async fn collect(seen: &mut mpsc::UnboundedReceiver<Seen>, log: &mut Vec<Seen>, dur: Duration) {
        let deadline = Instant::now() + dur;
        while let Ok(Some(s)) = timeout(deadline - Instant::now(), seen.recv()).await {
            log.push(s);
        }
    }

    fn chunks(log: &[Seen], conn: usize) -> Vec<(Instant, &[u8])> {
        log.iter()
            .filter_map(|s| match s {
                Seen::Data(c, at, d) if *c == conn => Some((*at, d.as_slice())),
                _ => None,
            })
            .collect()
    }

    fn bytes(log: &[Seen], conn: usize) -> Vec<u8> {
        chunks(log, conn)
            .iter()
            .flat_map(|(_, d)| d.iter().copied())
            .collect()
    }

    fn accepted(log: &[Seen]) -> Vec<(usize, Instant)> {
        log.iter()
            .filter_map(|s| match s {
                Seen::Accepted(c, at) => Some((*c, *at)),
                _ => None,
            })
            .collect()
    }

    fn sample(bytes: &[u8], i: usize) -> u32 {
        u32::from_le_bytes(bytes[i * 4..i * 4 + 4].try_into().unwrap())
    }

    async fn next_event(events: &mut mpsc::UnboundedReceiver<EngineEvent>) -> EngineEvent {
        timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("engine event within 5 s")
            .expect("events open")
    }

    #[tokio::test]
    async fn writer_paces_real_time() {
        let (nidoo, mut seen) = FakeNidoo::start("127.0.0.1:0").await;
        let (tx, _events) = mpsc::unbounded_channel();
        let mut engine = PacedEngine::spawn(cfg(nidoo.addr), tx);
        engine.play(pattern(0, 4.0), 1).unwrap();
        let mut log = Vec::new();
        collect(&mut seen, &mut log, Duration::from_secs(3)).await;

        let chunks = chunks(&log, 0);
        let t0 = chunks[0].0;
        let mut total = 0;
        for (at, data) in &chunks {
            total += data.len();
            let ahead_ms = total as f64 / BPS * 1000.0 - (*at - t0).as_secs_f64() * 1000.0;
            // max_ahead + 1 frame, plus 10 ms for scheduling.
            assert!(ahead_ms <= 230.0, "{ahead_ms:.1} ms ahead of real time");
        }
        let window: usize = chunks
            .iter()
            .filter(|(at, _)| {
                *at >= t0 + Duration::from_millis(500) && *at < t0 + Duration::from_millis(2_500)
            })
            .map(|(_, d)| d.len())
            .sum();
        assert!(
            (window as f64 - 2.0 * BPS).abs() <= 2.0 * BPS * 0.05,
            "{window} B in 2 s"
        );
    }

    #[tokio::test]
    async fn track_end_emits_event_and_next_play_continues_same_connection() {
        let (nidoo, mut seen) = FakeNidoo::start("127.0.0.1:0").await;
        let (tx, mut events) = mpsc::unbounded_channel();
        let mut engine = PacedEngine::spawn(cfg(nidoo.addr), tx);
        engine.play(pattern(0, 1.0), 1).unwrap();
        assert_eq!(
            next_event(&mut events).await,
            EngineEvent::TrackEnded { generation: 1 }
        );
        engine.play(pattern(48_000, 1.0), 2).unwrap();
        assert_eq!(
            next_event(&mut events).await,
            EngineEvent::TrackEnded { generation: 2 }
        );
        let mut log = Vec::new();
        collect(&mut seen, &mut log, Duration::from_millis(300)).await;

        assert_eq!(accepted(&log).len(), 1, "one connection for both songs");
        let all = bytes(&log, 0);
        assert_eq!(all.len(), 2 * 192_000);
        for i in 0..all.len() / 4 {
            assert_eq!(sample(&all, i), i as u32, "sample {i}");
        }
        // The first byte of song 2 arrives at most ~1 frame after the last of song 1.
        let mut offset = 0;
        let (mut last_a, mut first_b) = (None, None);
        for (at, data) in chunks(&log, 0) {
            if offset < 192_000 {
                last_a = Some(at);
            }
            if offset + data.len() > 192_000 && first_b.is_none() {
                first_b = Some(at);
            }
            offset += data.len();
        }
        let gap = first_b.unwrap() - last_a.unwrap();
        assert!(gap <= Duration::from_millis(40), "gap {gap:?}");
    }

    #[tokio::test]
    async fn pause_closes_and_resume_resends_unheard_audio() {
        let (nidoo, mut seen) = FakeNidoo::start("127.0.0.1:0").await;
        let (tx, _events) = mpsc::unbounded_channel();
        let mut engine = PacedEngine::spawn(cfg(nidoo.addr), tx);
        engine.play(pattern(0, 5.0), 1).unwrap();
        sleep(Duration::from_secs(1)).await;
        engine.pause().unwrap();
        let mut log = Vec::new();
        until(&mut seen, &mut log, Duration::from_secs(1), |l| {
            l.iter().any(|s| matches!(s, Seen::Closed(0)))
        })
        .await;
        let before = bytes(&log, 0);
        assert!(before.len() > RING, "{} B before pause", before.len());

        engine.resume().unwrap();
        until(&mut seen, &mut log, Duration::from_secs(2), |l| {
            bytes(l, 1).len() >= RING + 3_840
        })
        .await;
        let after = bytes(&log, 1);
        assert!(
            after[..RING] == before[before.len() - RING..],
            "resume re-sends the last 750 ms"
        );
        assert_eq!(
            sample(&after, RING / 4),
            (before.len() / 4) as u32,
            "then continues the song"
        );
    }

    #[tokio::test]
    async fn no_resend_when_disabled() {
        let (nidoo, mut seen) = FakeNidoo::start("127.0.0.1:0").await;
        let (tx, _events) = mpsc::unbounded_channel();
        let mut config = cfg(nidoo.addr);
        config.resend_on_resume = false;
        let mut engine = PacedEngine::spawn(config, tx);
        engine.play(pattern(0, 2.0), 1).unwrap();
        sleep(Duration::from_millis(500)).await;
        engine.pause().unwrap();
        let mut log = Vec::new();
        until(&mut seen, &mut log, Duration::from_secs(1), |l| {
            l.iter().any(|s| matches!(s, Seen::Closed(0)))
        })
        .await;
        let before = bytes(&log, 0);
        assert!(!before.is_empty());

        engine.resume().unwrap();
        until(&mut seen, &mut log, Duration::from_secs(2), |l| {
            bytes(l, 1).len() >= 3_840
        })
        .await;
        let after = bytes(&log, 1);
        assert_eq!(
            sample(&after, 0),
            (before.len() / 4) as u32,
            "continues right after the last sample heard, no repeat"
        );
    }

    #[tokio::test]
    async fn resume_does_not_burst() {
        let (nidoo, mut seen) = FakeNidoo::start("127.0.0.1:0").await;
        let (tx, _events) = mpsc::unbounded_channel();
        let mut engine = PacedEngine::spawn(cfg(nidoo.addr), tx);
        engine.play(pattern(0, 5.0), 1).unwrap();
        sleep(Duration::from_secs(1)).await;
        engine.pause().unwrap();
        let mut log = Vec::new();
        until(&mut seen, &mut log, Duration::from_secs(1), |l| {
            l.iter().any(|s| matches!(s, Seen::Closed(0)))
        })
        .await;
        engine.resume().unwrap();
        until(&mut seen, &mut log, Duration::from_secs(1), |l| {
            accepted(l).len() == 2
        })
        .await;
        let at = accepted(&log)[1].1;
        collect(&mut seen, &mut log, Duration::from_millis(600)).await;
        let early: usize = chunks(&log, 1)
            .iter()
            .filter(|(t, _)| *t <= at + Duration::from_millis(300))
            .map(|(_, d)| d.len())
            .sum();
        // re-sent ring + max_ahead + 300 ms
        let bound = RING + 38_400 + 57_600;
        assert!(
            early <= bound,
            "{early} B in the first 300 ms (bound {bound})"
        );
    }

    #[tokio::test]
    async fn reconnects_after_listener_restart() {
        let (nidoo, _seen) = FakeNidoo::start("127.0.0.1:0").await;
        let addr = nidoo.addr;
        let (tx, mut events) = mpsc::unbounded_channel();
        let mut engine = PacedEngine::spawn(cfg(addr), tx);
        let started = Instant::now();
        engine.play(pattern(0, 3.0), 1).unwrap();
        sleep(Duration::from_secs(1)).await;
        nidoo.kill();
        sleep(Duration::from_secs(1)).await;

        let (_nidoo2, mut seen2) = FakeNidoo::start(&addr.to_string()).await;
        let rebound = Instant::now();
        let mut log = Vec::new();
        until(&mut seen2, &mut log, Duration::from_millis(1_200), |l| {
            !accepted(l).is_empty()
        })
        .await;
        let reconnect = accepted(&log)[0].1 - rebound;
        assert!(
            reconnect <= Duration::from_millis(1_100),
            "reconnected after {reconnect:?}"
        );

        assert_eq!(
            next_event(&mut events).await,
            EngineEvent::TrackEnded { generation: 1 }
        );
        let ended = started.elapsed();
        assert!(
            (Duration::from_millis(2_600)..=Duration::from_millis(3_400)).contains(&ended),
            "song ended after {ended:?}"
        );
        collect(&mut seen2, &mut log, Duration::from_millis(100)).await;
        assert!(
            !bytes(&log, 0).is_empty(),
            "audio flowed after reconnecting"
        );
    }

    #[tokio::test]
    async fn stale_generation_events_are_tagged() {
        let (nidoo, _seen) = FakeNidoo::start("127.0.0.1:0").await;
        let (tx, mut events) = mpsc::unbounded_channel();
        let mut engine = PacedEngine::spawn(cfg(nidoo.addr), tx);
        engine.play(pattern(0, 0.5), 1).unwrap();
        engine.play(pattern(0, 0.5), 2).unwrap();
        let mut got = Vec::new();
        let deadline = Instant::now() + Duration::from_millis(1_500);
        while let Ok(Some(e)) = timeout(deadline - Instant::now(), events.recv()).await {
            got.push(e);
        }
        assert_eq!(got, vec![EngineEvent::TrackEnded { generation: 2 }]);
    }

    async fn wait_link(rx: &mut watch::Receiver<Option<bool>>, want: Option<bool>) {
        timeout(Duration::from_secs(5), rx.wait_for(|v| *v == want))
            .await
            .expect("link state within 5 s")
            .expect("sender alive");
    }

    #[tokio::test]
    async fn link_reports_up_and_down() {
        let (nidoo, _seen) = FakeNidoo::start("127.0.0.1:0").await;
        let (tx, _events) = mpsc::unbounded_channel();
        let mut engine = PacedEngine::spawn(cfg(nidoo.addr), tx);
        let mut link = engine.link();
        assert_eq!(*link.borrow(), None, "unknown before any play");

        engine.play(pattern(0, 30.0), 1).unwrap();
        wait_link(&mut link, Some(true)).await;

        engine.pause().unwrap();
        sleep(Duration::from_millis(300)).await;
        assert_eq!(*link.borrow(), Some(true), "pause leaves the link alone");
        engine.resume().unwrap();
        wait_link(&mut link, Some(true)).await;

        nidoo.kill();
        wait_link(&mut link, Some(false)).await;
    }

    struct DeadConnector;

    impl Connector for DeadConnector {
        fn target(&self) -> String {
            "nowhere".into()
        }

        fn connect(&self) -> BoxFuture<'static, io::Result<Box<dyn PcmSink>>> {
            Box::pin(async { Err(io::Error::other("refused")) })
        }
    }

    struct CountingReader(Arc<std::sync::atomic::AtomicUsize>);

    #[async_trait]
    impl PcmReader for CountingReader {
        async fn read(&mut self, buf: &mut [u8]) -> anyhow::Result<usize> {
            buf.fill(1);
            self.0
                .fetch_add(buf.len(), std::sync::atomic::Ordering::Relaxed);
            Ok(buf.len())
        }
    }

    #[tokio::test]
    async fn down_link_consumes_at_most_200ms_ahead() {
        let read = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (tx, _events) = mpsc::unbounded_channel();
        let mut engine = PacedEngine::spawn(
            PacedConfig {
                label: "dead",
                connector: Arc::new(DeadConnector),
                max_ahead: Duration::from_secs(5),
                latency: Duration::ZERO,
                resend_on_resume: true,
                flush_on_play: false,
            },
            tx,
        );
        let mut link = engine.link();
        engine
            .play(Box::new(CountingReader(read.clone())), 1)
            .unwrap();
        wait_link(&mut link, Some(false)).await;
        let from = read.load(std::sync::atomic::Ordering::Relaxed);
        sleep(Duration::from_secs(1)).await;
        let used = read.load(std::sync::atomic::Ordering::Relaxed) - from;
        // 1 s of downtime plus the 200 ms lead, with a frame of slack.
        let bound = 192_000 + 38_400 + 3_840;
        assert!(
            used <= bound,
            "{used} B consumed in 1 s down (bound {bound})"
        );
    }

    #[derive(Debug, PartialEq)]
    enum Ev {
        Connect,
        Write(Vec<u8>),
        Flush,
    }

    type EvLog = Arc<Mutex<Vec<Ev>>>;

    struct RecConnector(EvLog);
    struct RecSink(EvLog);

    #[async_trait]
    impl PcmSink for RecSink {
        async fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
            self.0.lock().unwrap().push(Ev::Write(bytes.to_vec()));
            Ok(())
        }

        async fn flush(&mut self) -> io::Result<()> {
            self.0.lock().unwrap().push(Ev::Flush);
            Ok(())
        }
    }

    impl Connector for RecConnector {
        fn target(&self) -> String {
            "rec".into()
        }

        fn connect(&self) -> BoxFuture<'static, io::Result<Box<dyn PcmSink>>> {
            let log = self.0.clone();
            Box::pin(async move {
                log.lock().unwrap().push(Ev::Connect);
                Ok(Box::new(RecSink(log)) as Box<dyn PcmSink>)
            })
        }
    }

    fn rec_engine(
        flush_on_play: bool,
    ) -> (PacedEngine, EvLog, mpsc::UnboundedReceiver<EngineEvent>) {
        let log: EvLog = Arc::default();
        let (tx, events) = mpsc::unbounded_channel();
        let engine = PacedEngine::spawn(
            PacedConfig {
                label: "rec",
                connector: Arc::new(RecConnector(log.clone())),
                max_ahead: Duration::from_millis(200),
                latency: Duration::from_millis(750),
                resend_on_resume: true,
                flush_on_play,
            },
            tx,
        );
        (engine, log, events)
    }

    /// Waits until `done(log)` holds; panics after 5 s.
    async fn wait_log(log: &EvLog, done: impl Fn(&[Ev]) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done(&log.lock().unwrap()) {
            assert!(Instant::now() < deadline, "log condition not met");
            sleep(Duration::from_millis(10)).await;
        }
    }

    fn written(log: &[Ev]) -> usize {
        log.iter()
            .map(|e| if let Ev::Write(b) = e { b.len() } else { 0 })
            .sum()
    }

    /// The writes after the last `from` marker, concatenated.
    fn writes_after(log: &[Ev], from: usize) -> Vec<u8> {
        log[from..]
            .iter()
            .filter_map(|e| {
                if let Ev::Write(b) = e {
                    Some(b.as_slice())
                } else {
                    None
                }
            })
            .flatten()
            .copied()
            .collect()
    }

    #[tokio::test]
    async fn flush_on_play_flushes_on_skip() {
        let (mut engine, log, _events) = rec_engine(true);
        engine.play(pattern(0, 30.0), 1).unwrap();
        wait_log(&log, |l| written(l) >= 3 * FRAME).await;
        engine.play(pattern(1_000_000, 30.0), 2).unwrap();
        wait_log(&log, |l| {
            l.iter()
                .position(|e| *e == Ev::Flush)
                .is_some_and(|f| written(&l[f..]) >= 2 * FRAME)
        })
        .await;
        let l = log.lock().unwrap();
        let flush = l.iter().position(|e| *e == Ev::Flush).unwrap();
        assert_eq!(l.iter().filter(|e| **e == Ev::Flush).count(), 1);
        let after = writes_after(&l, flush);
        assert_eq!(
            sample(&after, 0),
            1_000_000,
            "B's first bytes follow the flush"
        );
    }

    #[tokio::test]
    async fn natural_next_song_does_not_flush() {
        let (mut engine, log, mut events) = rec_engine(true);
        engine.play(pattern(0, 0.5), 1).unwrap();
        assert_eq!(
            next_event(&mut events).await,
            EngineEvent::TrackEnded { generation: 1 }
        );
        let before = written(&log.lock().unwrap());
        engine.play(pattern(1_000_000, 30.0), 2).unwrap();
        wait_log(&log, |l| written(l) >= before + 3 * FRAME).await;
        assert!(!log.lock().unwrap().contains(&Ev::Flush));
    }

    #[tokio::test]
    async fn skip_then_pause_resends_only_new_song() {
        let (mut engine, log, _events) = rec_engine(true);
        engine.play(pattern(0, 30.0), 1).unwrap();
        wait_log(&log, |l| written(l) >= 3 * FRAME).await;
        engine.play(pattern(1_000_000, 30.0), 2).unwrap();
        // 300 ms of B (1_000_000 + 14_400 samples), counted after the flush.
        wait_log(&log, |l| {
            l.iter()
                .position(|e| *e == Ev::Flush)
                .is_some_and(|f| written(&l[f..]) >= 57_600)
        })
        .await;
        engine.pause().unwrap();
        sleep(Duration::from_millis(100)).await;
        let mark = log.lock().unwrap().len();
        engine.resume().unwrap();
        wait_log(&log, |l| written(&l[mark..]) >= 2 * FRAME).await;
        let l = log.lock().unwrap();
        let resent = writes_after(&l, mark);
        assert!(resent.len() >= 2 * FRAME);
        for i in 0..2 * FRAME / 4 {
            assert!(
                sample(&resent, i) >= 1_000_000,
                "sample {i} is from the old song"
            );
        }
    }

    #[tokio::test]
    async fn tcp_config_never_flushes() {
        let cfg = PacedConfig::tcp("x", "127.0.0.1:1".into(), Duration::from_millis(750));
        assert!(!cfg.flush_on_play);
        let (mut engine, log, _events) = rec_engine(false);
        engine.play(pattern(0, 30.0), 1).unwrap();
        wait_log(&log, |l| written(l) >= 3 * FRAME).await;
        engine.play(pattern(1_000_000, 30.0), 2).unwrap();
        wait_log(&log, |l| {
            l.iter()
                .any(|e| matches!(e, Ev::Write(b) if b.len() >= 4 && sample(b, 0) == 1_000_000))
        })
        .await;
        assert!(!log.lock().unwrap().contains(&Ev::Flush));
    }
}
