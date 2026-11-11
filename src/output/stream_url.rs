//! Stream URL: one continuous MP3 at `/stream.mp3` that any player can open.
//!
//! The paced writer fills a PCM queue; a caster task reads it in real time
//! (silence when it is empty), encodes it and broadcasts MP3 frames to every
//! listener, so the connection survives pauses and song changes.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Bytes;
use futures_util::future::BoxFuture;
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep, sleep_until};

use super::browser::FRAME_BYTES;
use super::browser::stream::run_ffmpeg;
use super::{Output, OutputInfo, OutputKind, STREAM_HINT};
use crate::engine::EngineEvent;
use crate::engine::paced::{Connector, PacedConfig, PacedEngine, PcmSink};
use crate::engine::queue::{PcmQueue, QueueConnector};
use crate::source::PcmReader;

/// 20 ms of s16le / 48 kHz / stereo.
const TICK_BYTES: usize = 3_840;
const TICK: Duration = Duration::from_millis(20);
/// A read this late resets the clock instead of bursting to catch up.
const MAX_LATE: Duration = Duration::from_secs(1);
const TAP_CAPACITY: usize = 64;
const RESTART_DELAY: Duration = Duration::from_secs(1);

/// Reads the queue in real time, one 20 ms frame per tick, silence when empty.
pub struct TickReader {
    queue: Arc<PcmQueue>,
    next: Instant,
}

impl TickReader {
    pub fn new(queue: Arc<PcmQueue>) -> Self {
        Self {
            queue,
            next: Instant::now(),
        }
    }
}

#[async_trait]
impl PcmReader for TickReader {
    /// `buf` must hold at least 3 840 bytes; only the first 3 840 are filled.
    async fn read(&mut self, buf: &mut [u8]) -> anyhow::Result<usize> {
        debug_assert!(
            buf.len() >= TICK_BYTES,
            "TickReader needs a 3840-byte buffer"
        );
        let len = buf.len().min(TICK_BYTES) / 4 * 4;
        sleep_until(self.next).await;
        let now = Instant::now();
        if now.duration_since(self.next) > MAX_LATE {
            self.next = now;
        }
        self.next += TICK;
        self.queue.pop_into(&mut buf[..len]);
        Ok(len)
    }
}

/// Collects encoder output into whole 576-byte MP3 frames.
#[derive(Default)]
pub struct FrameAligner {
    pending: Vec<u8>,
}

impl FrameAligner {
    /// The longest prefix of whole frames accumulated so far; the rest is kept.
    pub fn push(&mut self, bytes: &[u8]) -> Option<Bytes> {
        self.pending.extend_from_slice(bytes);
        let whole = self.pending.len() / FRAME_BYTES * FRAME_BYTES;
        (whole > 0).then(|| Bytes::from(self.pending.drain(..whole).collect::<Vec<u8>>()))
    }
}

/// Where listeners subscribe to the MP3 frames.
#[derive(Clone)]
pub struct StreamTap(broadcast::Sender<Bytes>);

impl StreamTap {
    pub fn subscribe(&self) -> broadcast::Receiver<Bytes> {
        self.0.subscribe()
    }
}

/// Receives encoder output as it comes.
pub type PushFn = Box<dyn FnMut(&[u8]) + Send>;

/// Turns the live PCM into MP3 bytes, handed to `push` as they come.
pub trait LiveEncoder: Send + Sync + 'static {
    fn run(
        &self,
        reader: Box<dyn PcmReader>,
        push: PushFn,
    ) -> BoxFuture<'static, anyhow::Result<()>>;
}

pub struct FfmpegLive {
    pub ffmpeg: std::path::PathBuf,
}

impl LiveEncoder for FfmpegLive {
    fn run(
        &self,
        reader: Box<dyn PcmReader>,
        push: PushFn,
    ) -> BoxFuture<'static, anyhow::Result<()>> {
        Box::pin(run_ffmpeg(self.ffmpeg.clone(), reader, push))
    }
}

/// Aborts its task when dropped.
pub struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Starts the caster: encodes the queue in real time and broadcasts it. If
/// the encoder fails it is restarted after a second.
pub fn spawn_caster(
    encoder: Arc<dyn LiveEncoder>,
    queue: Arc<PcmQueue>,
) -> (StreamTap, AbortOnDrop) {
    let (tx, _) = broadcast::channel(TAP_CAPACITY);
    let tap = StreamTap(tx.clone());
    let task = tokio::spawn(async move {
        loop {
            let mut aligner = FrameAligner::default();
            let out = tx.clone();
            let push = Box::new(move |bytes: &[u8]| {
                if let Some(frames) = aligner.push(bytes) {
                    let _ = out.send(frames);
                }
            });
            match encoder
                .run(Box::new(TickReader::new(queue.clone())), push)
                .await
            {
                Ok(()) => tracing::warn!("stream encoder ended; restarting"),
                Err(e) => tracing::warn!("stream encoder failed: {e:#}; restarting"),
            }
            sleep(RESTART_DELAY).await;
        }
    });
    (tap, AbortOnDrop(task))
}

/// The queue connector, plus the caster: the writer task owns both, so
/// dropping the engine ends the caster and closes every listener.
struct StreamConnector {
    inner: QueueConnector,
    _caster: AbortOnDrop,
}

impl Connector for StreamConnector {
    fn target(&self) -> String {
        self.inner.target()
    }

    fn connect(&self) -> BoxFuture<'static, std::io::Result<Box<dyn PcmSink>>> {
        self.inner.connect()
    }
}

pub fn build_stream(
    encoder: Arc<dyn LiveEncoder>,
    latency: Duration,
    events: mpsc::UnboundedSender<EngineEvent>,
) -> Output {
    let queue = PcmQueue::new(Duration::from_secs(1), Duration::from_secs(2));
    let (tap, caster) = spawn_caster(encoder, queue.clone());
    let connector = StreamConnector {
        inner: QueueConnector {
            target: "/stream.mp3".into(),
            ..QueueConnector::new(queue, false)
        },
        _caster: caster,
    };
    let engine = PacedEngine::spawn(
        PacedConfig {
            label: "stream",
            connector: Arc::new(connector),
            max_ahead: Duration::from_millis(200),
            latency,
            resend_on_resume: false,
        },
        events,
    );
    let link = engine.link();
    Output {
        engine: Box::new(engine),
        info: OutputInfo {
            id: "stream".into(),
            kind: OutputKind::Stream,
            name: "Stream URL".into(),
            address: None,
            hint: Some(STREAM_HINT.into()),
        },
        speakers: None,
        link: Some(link),
        browser_player: None,
        stream: Some(tap),
        closed: None,
    }
}

#[cfg(test)]
mod tests {
    use tokio::sync::broadcast::error::{RecvError, TryRecvError};
    use tokio::time::advance;

    use super::*;
    use crate::output::fake::PassthroughLive;

    fn queue() -> Arc<PcmQueue> {
        PcmQueue::new(Duration::from_secs(1), Duration::from_secs(2))
    }

    #[tokio::test(start_paused = true)]
    async fn tick_reader_paces_20ms() {
        let mut r = TickReader::new(queue());
        let start = Instant::now();
        let mut buf = vec![0u8; TICK_BYTES];
        for _ in 0..5 {
            assert_eq!(r.read(&mut buf).await.unwrap(), TICK_BYTES);
        }
        // The first read is immediate, so five reads span four ticks.
        assert_eq!(start.elapsed(), Duration::from_millis(80));
        assert_eq!(r.read(&mut buf).await.unwrap(), TICK_BYTES);
        assert_eq!(start.elapsed(), Duration::from_millis(100));
    }

    #[tokio::test(start_paused = true)]
    async fn tick_reader_resets_after_stall() {
        let mut r = TickReader::new(queue());
        let mut buf = vec![0u8; TICK_BYTES];
        r.read(&mut buf).await.unwrap();
        advance(Duration::from_secs(5)).await;
        let start = Instant::now();
        r.read(&mut buf).await.unwrap();
        assert_eq!(start.elapsed(), Duration::ZERO);
        r.read(&mut buf).await.unwrap();
        r.read(&mut buf).await.unwrap();
        assert_eq!(start.elapsed(), Duration::from_millis(40));
    }

    #[test]
    fn aligner_emits_whole_frames() {
        let mut a = FrameAligner::default();
        assert_eq!(a.push(&[1; 1_000]).unwrap().len(), 576);
        assert_eq!(a.push(&[2; 200]).unwrap().len(), 576);
        assert_eq!(a.pending.len(), 48);
        assert!(a.push(&[3; 10]).is_none());
    }

    /// Lets spawned tasks run for `ms` of virtual time, 20 ms at a time.
    async fn run_for(ms: u64) {
        for _ in 0..ms / 20 {
            advance(Duration::from_millis(20)).await;
            for _ in 0..8 {
                tokio::task::yield_now().await;
            }
        }
    }

    fn drain(rx: &mut broadcast::Receiver<Bytes>, into: &mut Vec<u8>) {
        loop {
            match rx.try_recv() {
                Ok(b) => into.extend_from_slice(&b),
                Err(TryRecvError::Lagged(_)) => {}
                Err(_) => return,
            }
        }
    }

    struct Pattern {
        next: u32,
        end: u32,
    }

    #[async_trait]
    impl PcmReader for Pattern {
        async fn read(&mut self, buf: &mut [u8]) -> anyhow::Result<usize> {
            let mut n = 0;
            while n + 4 <= buf.len().min(1_000) && self.next < self.end {
                buf[n..n + 4].copy_from_slice(&self.next.to_le_bytes());
                self.next += 1;
                n += 4;
            }
            Ok(n)
        }
    }

    #[tokio::test(start_paused = true)]
    async fn pause_drains_then_silence() {
        let (events, _rx) = mpsc::unbounded_channel();
        let mut out = build_stream(Arc::new(PassthroughLive), Duration::ZERO, events);
        let mut rx = out.stream.as_ref().unwrap().subscribe();
        // 0.3 s of audio: samples 1..=14400, each u32 little endian.
        out.engine
            .play(
                Box::new(Pattern {
                    next: 1,
                    end: 14_401,
                }),
                1,
            )
            .unwrap();
        let mut got = Vec::new();
        run_for(400).await;
        drain(&mut rx, &mut got);
        out.engine.pause().unwrap();
        run_for(2_000).await;
        drain(&mut rx, &mut got);

        let expected: Vec<u8> = (1..=14_400u32).flat_map(u32::to_le_bytes).collect();
        let first = got.iter().position(|&b| b != 0).expect("no audio at all");
        assert_eq!(&got[first..first + expected.len()], &expected[..]);
        assert!(got[first + expected.len()..].iter().all(|&b| b == 0));
        assert!(
            got.len() > first + expected.len() + 10_000,
            "no silence after"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn lagging_listener_keeps_streaming() {
        let (tap, _guard) = spawn_caster(Arc::new(PassthroughLive), queue());
        let mut rx = tap.subscribe();
        run_for(2_000).await;
        assert!(matches!(rx.recv().await, Err(RecvError::Lagged(_))));
        let chunk = rx.recv().await.unwrap();
        assert!(!chunk.is_empty() && chunk.len() % FRAME_BYTES == 0);
        run_for(100).await;
        let chunk = loop {
            match rx.recv().await {
                Err(RecvError::Lagged(_)) => {}
                other => break other.unwrap(),
            }
        };
        assert_eq!(chunk.len() % FRAME_BYTES, 0);
    }
}
