//! nidoo-sound output: s16le / 48 kHz / stereo PCM over TCP.
//!
//! nidoo-sound plays what it receives on its own clock, so pacing is the
//! sender's job: write at the real-time rate and stay at most `max_ahead`
//! in front of it.

use std::collections::VecDeque;
use std::future::pending;
use std::io;
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
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
const BACKOFF_MIN: Duration = Duration::from_millis(500);
const BACKOFF_MAX: Duration = Duration::from_secs(5);

#[derive(Clone, Debug)]
pub struct NidooSoundConfig {
    /// nidoo-sound's PCM input, e.g. `192.168.1.91:4953`.
    pub addr: String,
    /// How far in front of real time the writer may get.
    pub max_ahead: Duration,
    /// From a written byte to the speakers; also how much is re-sent on resume.
    pub latency: Duration,
}

enum Cmd {
    Play(Box<dyn PcmReader>, u64),
    Pause,
    Resume,
    Stop,
}

/// Drives a writer task; dropping the engine ends the task and closes the socket.
pub struct NidooSoundEngine {
    cmds: mpsc::UnboundedSender<Cmd>,
    latency: Duration,
}

impl NidooSoundEngine {
    pub fn spawn(cfg: NidooSoundConfig, events: mpsc::UnboundedSender<EngineEvent>) -> Self {
        let (cmds, rx) = mpsc::unbounded_channel();
        let latency = cfg.latency;
        tokio::spawn(Writer::new(cfg, events, rx).run());
        Self { cmds, latency }
    }

    fn send(&self, cmd: Cmd) -> anyhow::Result<()> {
        self.cmds
            .send(cmd)
            .map_err(|_| anyhow::anyhow!("nidoo-sound writer has stopped"))
    }
}

impl AudioEngine for NidooSoundEngine {
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
    Connected(io::Result<TcpStream>),
    Due,
}

type Connect = JoinHandle<io::Result<TcpStream>>;

struct Writer {
    addr: String,
    max_ahead: Duration,
    ring_cap: usize,
    events: mpsc::UnboundedSender<EngineEvent>,
    cmds: mpsc::UnboundedReceiver<Cmd>,
    track: Option<Track>,
    paused: bool,
    // Link: `stream` up, `connecting` in flight, `retry_at` down, or all
    // empty = closed (paused, stopped, never opened).
    stream: Option<TcpStream>,
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
async fn connected(connecting: &mut Option<Connect>) -> io::Result<TcpStream> {
    match connecting {
        Some(task) => task.await.unwrap_or_else(|e| Err(io::Error::other(e))),
        None => pending().await,
    }
}

impl Writer {
    fn new(
        cfg: NidooSoundConfig,
        events: mpsc::UnboundedSender<EngineEvent>,
        cmds: mpsc::UnboundedReceiver<Cmd>,
    ) -> Self {
        Self {
            addr: cfg.addr,
            max_ahead: cfg.max_ahead,
            ring_cap: byte_len(cfg.latency),
            events,
            cmds,
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
                Wake::Due => self.write_next().await,
            }
        }
    }

    fn handle(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Play(reader, generation) => {
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
                    if self.track.is_some() {
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
            tracing::info!("nidoo-sound: closed {}", self.addr);
        }
        if let Some(task) = self.connecting.take() {
            task.abort();
        }
        self.connecting_fresh = false;
        self.retry_at = None;
    }

    fn start_connect(&mut self, fresh: bool) {
        let addr = self.addr.clone();
        self.retry_at = None;
        self.connecting_fresh = fresh;
        self.connecting = Some(tokio::spawn(async move {
            match timeout(CONNECT_TIMEOUT, TcpStream::connect(&addr)).await {
                Ok(res) => res,
                Err(_) => Err(io::Error::new(io::ErrorKind::TimedOut, "connect timed out")),
            }
        }));
    }

    fn on_connected(&mut self, res: io::Result<TcpStream>) {
        self.connecting = None;
        let fresh = std::mem::take(&mut self.connecting_fresh);
        if fresh {
            self.clock_start = Instant::now();
            self.clock_bytes = 0;
        }
        match res {
            Ok(stream) => {
                stream.set_nodelay(true).ok();
                tracing::info!("nidoo-sound: connected to {}", self.addr);
                self.stream = Some(stream);
                self.backoff = BACKOFF_MIN;
            }
            Err(e) => self.link_down(&format!("connect to {}: {e}", self.addr)),
        }
    }

    /// Drops the connection and schedules a retry; audio keeps being consumed
    /// in real time meanwhile, so the position stays honest.
    fn link_down(&mut self, why: &str) {
        tracing::warn!(
            "nidoo-sound: {why}; retrying in {:.1} s",
            self.backoff.as_secs_f32()
        );
        self.stream = None;
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
        played_out.checked_sub(self.max_ahead).unwrap_or(now)
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
            match timeout(WRITE_TIMEOUT, stream.write_all(&bytes)).await {
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

    fn cfg(addr: SocketAddr) -> NidooSoundConfig {
        NidooSoundConfig {
            addr: addr.to_string(),
            max_ahead: Duration::from_millis(200),
            latency: Duration::from_millis(750),
        }
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
        let mut engine = NidooSoundEngine::spawn(cfg(nidoo.addr), tx);
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
        let mut engine = NidooSoundEngine::spawn(cfg(nidoo.addr), tx);
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
        let mut engine = NidooSoundEngine::spawn(cfg(nidoo.addr), tx);
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
    async fn resume_does_not_burst() {
        let (nidoo, mut seen) = FakeNidoo::start("127.0.0.1:0").await;
        let (tx, _events) = mpsc::unbounded_channel();
        let mut engine = NidooSoundEngine::spawn(cfg(nidoo.addr), tx);
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
        let mut engine = NidooSoundEngine::spawn(cfg(addr), tx);
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
        let mut engine = NidooSoundEngine::spawn(cfg(nidoo.addr), tx);
        engine.play(pattern(0, 0.5), 1).unwrap();
        engine.play(pattern(0, 0.5), 2).unwrap();
        let mut got = Vec::new();
        let deadline = Instant::now() + Duration::from_millis(1_500);
        while let Ok(Some(e)) = timeout(deadline - Instant::now(), events.recv()).await {
            got.push(e);
        }
        assert_eq!(got, vec![EngineEvent::TrackEnded { generation: 2 }]);
    }
}
