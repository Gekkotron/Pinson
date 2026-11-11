//! In-memory PCM queue: a [`PcmSink`] on the writing side, and a consumer
//! (a real-time ticker, the sound card callback) that pulls bytes out.

use std::collections::VecDeque;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures_util::future::BoxFuture;
use tokio::sync::Notify;

use super::paced::{Connector, PcmSink};

/// 48 kHz × 2 channels × 2 bytes, per millisecond.
const BYTES_PER_MS: usize = 192;

/// Whole milliseconds, rounded down to a whole number of 4-byte samples.
fn byte_len(d: Duration) -> usize {
    d.as_millis() as usize * BYTES_PER_MS / 4 * 4
}

#[derive(Default)]
struct Inner {
    bytes: VecDeque<u8>,
    error: Option<String>,
}

/// Bytes waiting for a consumer. The sink stops writing at `target` and
/// the queue drops its oldest bytes past `cap`.
pub struct PcmQueue {
    inner: Mutex<Inner>,
    /// Signalled when bytes are popped or an error is set.
    notify: Notify,
    target: usize,
    cap: usize,
}

impl PcmQueue {
    pub fn new(target: Duration, cap: Duration) -> Arc<Self> {
        Self::with_bytes(byte_len(target), byte_len(cap))
    }

    fn with_bytes(target: usize, cap: usize) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Inner::default()),
            notify: Notify::new(),
            target,
            cap,
        })
    }

    /// Copies queued bytes into `out` and zero-fills the rest. Returns how
    /// many bytes were real audio. Safe on a real-time thread: a short lock,
    /// no allocation.
    pub fn pop_into(&self, out: &mut [u8]) -> usize {
        let n = {
            let mut inner = self.inner.lock().unwrap();
            let n = out.len().min(inner.bytes.len());
            let (a, b) = inner.bytes.as_slices();
            let from_a = n.min(a.len());
            out[..from_a].copy_from_slice(&a[..from_a]);
            out[from_a..n].copy_from_slice(&b[..n - from_a]);
            inner.bytes.drain(..n);
            n
        };
        out[n..].fill(0);
        if n > 0 {
            self.notify.notify_one();
        }
        n
    }

    pub fn len_bytes(&self) -> usize {
        self.inner.lock().unwrap().bytes.len()
    }

    pub fn clear(&self) {
        self.inner.lock().unwrap().bytes.clear();
    }

    /// Makes the next sink write fail with `why`; the error is then cleared.
    pub fn fail(&self, why: String) {
        self.inner.lock().unwrap().error = Some(why);
        self.notify.notify_one();
    }

    fn push(&self, bytes: &[u8]) {
        let mut inner = self.inner.lock().unwrap();
        inner.bytes.extend(bytes);
        let over = inner.bytes.len().saturating_sub(self.cap);
        if over > 0 {
            inner.bytes.drain(..over.next_multiple_of(4));
        }
    }
}

type Open = Arc<dyn Fn() -> BoxFuture<'static, io::Result<Box<dyn Send>>> + Send + Sync>;

/// Connects the writer to a [`PcmQueue`]. `open` runs on every connect and
/// its guard lives as long as the sink (e.g. an audio stream).
pub struct QueueConnector {
    pub queue: Arc<PcmQueue>,
    /// Drop queued audio when the sink is dropped (pause, stop).
    pub clear_on_close: bool,
    pub open: Option<Open>,
    /// What logs call the sink: a device name, a URL.
    pub target: String,
}

impl QueueConnector {
    pub fn new(queue: Arc<PcmQueue>, clear_on_close: bool) -> Self {
        Self {
            queue,
            clear_on_close,
            open: None,
            target: "queue".into(),
        }
    }
}

struct QueueSink {
    queue: Arc<PcmQueue>,
    clear_on_close: bool,
    _guard: Option<Box<dyn Send>>,
}

#[async_trait]
impl PcmSink for QueueSink {
    async fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        let q = &self.queue;
        loop {
            let notified = q.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let mut inner = q.inner.lock().unwrap();
                if let Some(why) = inner.error.take() {
                    return Err(io::Error::other(why));
                }
                if inner.bytes.len() < q.target {
                    drop(inner);
                    q.push(bytes);
                    return Ok(());
                }
            }
            notified.await;
        }
    }
}

impl Drop for QueueSink {
    fn drop(&mut self) {
        if self.clear_on_close {
            self.queue.clear();
        }
    }
}

impl Connector for QueueConnector {
    fn target(&self) -> String {
        self.target.clone()
    }

    fn connect(&self) -> BoxFuture<'static, io::Result<Box<dyn PcmSink>>> {
        let queue = self.queue.clone();
        let clear_on_close = self.clear_on_close;
        let open = self.open.clone();
        Box::pin(async move {
            // A new sink starts clean: an error left by the last one must not
            // fail its first write. Errors from the new stream arrive after `open`.
            queue.inner.lock().unwrap().error = None;
            let guard = match open {
                Some(open) => Some(open().await?),
                None => None,
            };
            Ok(Box::new(QueueSink {
                queue,
                clear_on_close,
                _guard: guard,
            }) as Box<dyn PcmSink>)
        })
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::task::Poll;
    use std::time::Duration;

    use tokio::sync::mpsc;
    use tokio::time::{advance, sleep};

    use super::*;
    use crate::engine::AudioEngine;
    use crate::engine::paced::{PacedConfig, PacedEngine};
    use crate::source::PcmReader;

    const FRAME: usize = 3_840;

    async fn sink(q: &Arc<PcmQueue>, clear: bool) -> Box<dyn PcmSink> {
        QueueConnector::new(q.clone(), clear)
            .connect()
            .await
            .unwrap()
    }

    #[test]
    fn pop_fills_silence() {
        let q = PcmQueue::with_bytes(100, 100);
        q.push(&[1; 8]);
        let mut out = [9u8; 16];
        assert_eq!(q.pop_into(&mut out), 8);
        assert_eq!(out[..8], [1; 8]);
        assert_eq!(out[8..], [0; 8]);
        assert_eq!(q.len_bytes(), 0);
    }

    #[test]
    fn cap_drops_oldest_aligned() {
        let q = PcmQueue::with_bytes(100, 8);
        let data: Vec<u8> = (0..12).collect();
        q.push(&data);
        let mut out = [0u8; 16];
        assert_eq!(q.pop_into(&mut out), 8);
        assert_eq!(out[..8], data[4..]);
    }

    #[test]
    fn durations_become_aligned_bytes() {
        let q = PcmQueue::new(Duration::from_millis(40), Duration::from_millis(100));
        assert_eq!((q.target, q.cap), (7_680, 19_200));
    }

    #[tokio::test]
    async fn sink_waits_at_target() {
        let q = PcmQueue::new(Duration::from_millis(40), Duration::from_secs(1));
        let mut s = sink(&q, false).await;
        let frame = vec![5u8; FRAME];
        s.write(&frame).await.unwrap();
        s.write(&frame).await.unwrap();
        let mut third = Box::pin(s.write(&frame));
        let pending = std::future::poll_fn(|cx| Poll::Ready(third.as_mut().poll(cx).is_pending()));
        assert!(pending.await);
        let mut out = vec![0u8; FRAME];
        q.pop_into(&mut out);
        third.await.unwrap();
        assert_eq!(q.len_bytes(), 2 * FRAME);
    }

    #[tokio::test]
    async fn drop_clears_when_asked() {
        for clear in [true, false] {
            let q = PcmQueue::new(Duration::from_millis(40), Duration::from_secs(1));
            let mut s = sink(&q, clear).await;
            s.write(&[1; 16]).await.unwrap();
            drop(s);
            assert_eq!(q.len_bytes(), if clear { 0 } else { 16 });
        }
    }

    #[tokio::test]
    async fn fail_errors_next_write() {
        let q = PcmQueue::new(Duration::from_millis(40), Duration::from_secs(1));
        let mut s = sink(&q, false).await;
        q.fail("boom".into());
        let err = s.write(&[1; 16]).await.unwrap_err();
        assert_eq!(err.to_string(), "boom");
        s.write(&[1; 16]).await.unwrap();
    }

    #[tokio::test]
    async fn new_connect_clears_a_stale_error() {
        let q = PcmQueue::new(Duration::from_millis(40), Duration::from_secs(1));
        q.fail("left over".into());
        let mut s = sink(&q, false).await;
        s.write(&[1; 16]).await.unwrap();
    }

    #[tokio::test]
    async fn open_guard_lives_with_sink() {
        struct Guard(Arc<Mutex<bool>>);
        impl Drop for Guard {
            fn drop(&mut self) {
                *self.0.lock().unwrap() = true;
            }
        }
        let dropped = Arc::new(Mutex::new(false));
        let flag = dropped.clone();
        let mut c = QueueConnector::new(
            PcmQueue::new(Duration::from_millis(40), Duration::from_secs(1)),
            false,
        );
        c.open = Some(Arc::new(move || {
            let flag = flag.clone();
            Box::pin(async move { Ok(Box::new(Guard(flag)) as Box<dyn Send>) })
        }));
        let s = c.connect().await.unwrap();
        assert!(!*dropped.lock().unwrap());
        drop(s);
        assert!(*dropped.lock().unwrap());
    }

    /// Sample `i` is the u32 `i` (little endian).
    struct PatternReader {
        next: u32,
        end: u32,
    }

    #[async_trait]
    impl PcmReader for PatternReader {
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

    fn engine(
        q: &Arc<PcmQueue>,
    ) -> (
        PacedEngine,
        mpsc::UnboundedReceiver<crate::engine::EngineEvent>,
    ) {
        let (tx, rx) = mpsc::unbounded_channel();
        let cfg = PacedConfig {
            label: "queue",
            connector: Arc::new(QueueConnector::new(q.clone(), true)),
            max_ahead: Duration::from_millis(200),
            latency: Duration::ZERO,
            resend_on_resume: false,
            flush_on_play: false,
        };
        (PacedEngine::spawn(cfg, tx), rx)
    }

    #[tokio::test(start_paused = true)]
    async fn blocked_sink_times_out() {
        let q = PcmQueue::new(Duration::from_millis(40), Duration::from_secs(1));
        let (mut e, _events) = engine(&q);
        let mut link = e.link();
        e.play(
            Box::new(PatternReader {
                next: 0,
                end: 48_000 * 5,
            }),
            1,
        )
        .unwrap();
        for _ in 0..40 {
            sleep(Duration::from_millis(100)).await;
            if *link.borrow_and_update() == Some(false) {
                return;
            }
        }
        panic!("link never went down; queue {} bytes", q.len_bytes());
    }

    #[tokio::test(start_paused = true)]
    async fn engine_writes_pattern_into_queue() {
        let q = PcmQueue::new(Duration::from_millis(100), Duration::from_secs(2));
        let (mut e, _events) = engine(&q);
        e.play(
            Box::new(PatternReader {
                next: 0,
                end: 24_000,
            }),
            1,
        )
        .unwrap();
        let mut got = Vec::new();
        let mut buf = vec![0u8; FRAME];
        for _ in 0..40 {
            advance(Duration::from_millis(20)).await;
            tokio::task::yield_now().await;
            let n = q.pop_into(&mut buf);
            got.extend_from_slice(&buf[..n]);
        }
        let expected: Vec<u8> = (0..24_000u32).flat_map(u32::to_le_bytes).collect();
        assert_eq!(got.len(), expected.len());
        assert!(got == expected, "popped audio is not the pattern in order");
    }
}
