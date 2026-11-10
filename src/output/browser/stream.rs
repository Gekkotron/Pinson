//! The MP3 a browser tab plays: an append-only buffer fed by an encoder, and
//! the ffmpeg encoder that fills it.
//!
//! The stream is CBR 192 kbit/s at 48 kHz, so one millisecond is exactly 24
//! bytes and every 576-byte frame holds 24 ms. That makes a byte offset
//! computable from a time without parsing the MP3.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, anyhow};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::source::PcmReader;

pub const BYTES_PER_MS: usize = 24;
pub const FRAME_BYTES: usize = 576;
/// The most one body chunk copies, so a slow reader holds little memory.
const MAX_CHUNK: usize = 64 * 1024;

/// Start of the 24 ms frame that holds `ms`.
pub fn byte_offset(ms: u64) -> usize {
    ((ms / 24) as usize).saturating_mul(FRAME_BYTES)
}

#[derive(Clone, Debug, PartialEq)]
enum End {
    Open,
    Done,
    Failed(String),
}

struct Inner {
    bytes: Vec<u8>,
    end: End,
}

/// Bytes of one encoded stream. Readers may start at any offset and wait for
/// more until the encoder finishes.
pub struct StreamBuf {
    inner: Mutex<Inner>,
    notify: Notify,
}

impl StreamBuf {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Inner {
                bytes: Vec::new(),
                end: End::Open,
            }),
            notify: Notify::new(),
        })
    }

    pub fn push(&self, bytes: &[u8]) {
        self.inner.lock().unwrap().bytes.extend_from_slice(bytes);
        self.notify.notify_waiters();
    }

    pub fn finish(&self) {
        self.set_end(End::Done);
    }

    pub fn fail(&self, reason: String) {
        self.set_end(End::Failed(reason));
    }

    fn set_end(&self, end: End) {
        self.inner.lock().unwrap().end = end;
        self.notify.notify_waiters();
    }

    pub fn is_done(&self) -> bool {
        self.inner.lock().unwrap().end == End::Done
    }

    /// Everything written so far.
    pub fn snapshot(&self) -> Vec<u8> {
        self.inner.lock().unwrap().bytes.clone()
    }

    /// An HTTP body of the bytes from `offset` on. It waits for more bytes
    /// until the buffer is done, and errors if the encoder failed.
    pub fn body_from(self: &Arc<Self>, offset: usize) -> axum::body::Body {
        let stream =
            futures_util::stream::unfold((self.clone(), offset), |(buf, pos)| async move {
                loop {
                    let waiter = buf.clone();
                    let notified = waiter.notify.notified();
                    tokio::pin!(notified);
                    notified.as_mut().enable();
                    let (chunk, end) = {
                        let inner = buf.inner.lock().unwrap();
                        let chunk = (pos < inner.bytes.len()).then(|| {
                            let to = inner.bytes.len().min(pos.saturating_add(MAX_CHUNK));
                            bytes_chunk(&inner.bytes[pos..to])
                        });
                        (chunk, inner.end.clone())
                    };
                    if let Some(chunk) = chunk {
                        let next = pos + chunk.len();
                        return Some((Ok::<_, std::io::Error>(chunk), (buf, next)));
                    }
                    match end {
                        End::Open => notified.await,
                        End::Done => return None,
                        End::Failed(reason) => {
                            return Some((Err(std::io::Error::other(reason)), (buf, usize::MAX)));
                        }
                    }
                }
            });
        axum::body::Body::from_stream(stream)
    }
}

fn bytes_chunk(slice: &[u8]) -> axum::body::Bytes {
    axum::body::Bytes::copy_from_slice(slice)
}

/// Aborts the encoder task when dropped.
struct AbortOnDrop(JoinHandle<anyhow::Result<()>>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// One song's stream: encoded from `start` into the song, for `generation`.
pub struct Stream {
    pub generation: u64,
    pub start: Duration,
    pub buf: Arc<StreamBuf>,
    _task: AbortOnDrop,
}

impl Drop for Stream {
    /// The encoder is aborted with the stream: end the bytes it will never
    /// write, so a reader still holding the buffer stops waiting.
    fn drop(&mut self) {
        let mut inner = self.buf.inner.lock().unwrap();
        if inner.end == End::Open {
            inner.end = End::Done;
            drop(inner);
            self.buf.notify.notify_waiters();
        }
    }
}

impl Stream {
    pub fn new(
        generation: u64,
        start: Duration,
        buf: Arc<StreamBuf>,
        task: JoinHandle<anyhow::Result<()>>,
    ) -> Self {
        Self {
            generation,
            start,
            buf,
            _task: AbortOnDrop(task),
        }
    }
}

/// Turns PCM (s16le / 48 kHz / stereo) into the stream's bytes.
pub trait Encoder: Send + Sync + 'static {
    fn start(
        &self,
        reader: Box<dyn PcmReader>,
        out: Arc<StreamBuf>,
    ) -> JoinHandle<anyhow::Result<()>>;
}

pub struct FfmpegMp3 {
    pub ffmpeg: PathBuf,
}

impl Encoder for FfmpegMp3 {
    fn start(
        &self,
        reader: Box<dyn PcmReader>,
        out: Arc<StreamBuf>,
    ) -> JoinHandle<anyhow::Result<()>> {
        let ffmpeg = self.ffmpeg.clone();
        tokio::spawn(async move {
            let result = run_ffmpeg(ffmpeg, reader, &out).await;
            match &result {
                Ok(()) => out.finish(),
                Err(e) => out.fail(format!("{e:#}")),
            }
            result
        })
    }
}

async fn run_ffmpeg(
    ffmpeg: PathBuf,
    mut reader: Box<dyn PcmReader>,
    out: &StreamBuf,
) -> anyhow::Result<()> {
    let mut child = Command::new(&ffmpeg)
        .args(["-hide_banner", "-loglevel", "error"])
        .args(["-f", "s16le", "-ar", "48000", "-ac", "2", "-i", "pipe:0"])
        .args(["-c:a", "libmp3lame", "-b:a", "192k"])
        .args([
            "-id3v2_version",
            "0",
            "-write_xing",
            "0",
            "-f",
            "mp3",
            "pipe:1",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("starting {}", ffmpeg.display()))?;
    let mut stdin = child.stdin.take().context("ffmpeg stdin")?;
    let mut stdout = child.stdout.take().context("ffmpeg stdout")?;
    let mut stderr = child.stderr.take().context("ffmpeg stderr")?;

    let feed = async {
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            let n = reader.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            stdin.write_all(&buf[..n]).await?;
        }
        stdin.shutdown().await?;
        drop(stdin);
        anyhow::Ok(())
    };
    let drain = async {
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            let n = stdout.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            out.push(&buf[..n]);
        }
        anyhow::Ok(())
    };
    let errors = async {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text).await;
        text
    };
    let (fed, drained, text) = tokio::join!(feed, drain, errors);
    let status = child.wait().await?;
    let last = text.lines().last().unwrap_or("").to_owned();
    if !status.success() {
        return Err(anyhow!("ffmpeg exited with {status}: {last}"));
    }
    fed?;
    drained?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::PcmReader;
    use async_trait::async_trait;
    use http_body_util::BodyExt;

    async fn collect(body: axum::body::Body) -> Result<Vec<u8>, axum::Error> {
        Ok(body.collect().await?.to_bytes().to_vec())
    }

    #[test]
    fn byte_offset_is_frame_aligned() {
        assert_eq!(byte_offset(0), 0);
        assert_eq!(byte_offset(23), 0);
        assert_eq!(byte_offset(24), 576);
        assert_eq!(byte_offset(1000), 41 * 576);
    }

    #[test]
    fn byte_offset_saturates() {
        assert_eq!(byte_offset(u64::MAX), usize::MAX);
    }

    #[tokio::test]
    async fn body_chunks_are_capped() {
        let buf = StreamBuf::new();
        buf.push(&vec![7u8; 200 * 1024]);
        buf.finish();
        let mut body = buf.body_from(0);
        let mut total = 0;
        while let Some(frame) = body.frame().await {
            let data = frame.unwrap().into_data().unwrap();
            assert!(data.len() <= MAX_CHUNK, "{}", data.len());
            total += data.len();
        }
        assert_eq!(total, 200 * 1024);
    }

    #[tokio::test]
    async fn reader_from_offset_starts_on_frame_boundary() {
        let buf = StreamBuf::new();
        let data: Vec<u8> = (0..2000u32).map(|i| i as u8).collect();
        buf.push(&data);
        buf.finish();
        assert_eq!(collect(buf.body_from(576)).await.unwrap(), data[576..]);
    }

    #[tokio::test]
    async fn body_waits_for_more_bytes_then_ends() {
        let buf = StreamBuf::new();
        let task = tokio::spawn(collect(buf.body_from(0)));
        buf.push(b"ab");
        tokio::task::yield_now().await;
        buf.push(b"cd");
        buf.finish();
        assert_eq!(task.await.unwrap().unwrap(), b"abcd");
    }

    #[tokio::test]
    async fn failed_buffer_ends_body_with_error() {
        let buf = StreamBuf::new();
        buf.push(b"ab");
        buf.fail("x".into());
        assert!(collect(buf.body_from(0)).await.is_err());
    }

    #[tokio::test]
    async fn dropping_an_unfinished_stream_ends_its_readers() {
        let buf = StreamBuf::new();
        buf.push(b"ab");
        let stream = Stream::new(1, Duration::ZERO, buf.clone(), tokio::spawn(async { Ok(()) }));
        let reader = tokio::spawn(collect(buf.body_from(0)));
        tokio::task::yield_now().await;
        drop(stream);
        let bytes = tokio::time::timeout(Duration::from_secs(2), reader)
            .await
            .expect("body hung")
            .unwrap()
            .unwrap();
        assert_eq!(bytes, b"ab");
    }

    struct Zeros(usize);

    #[async_trait]
    impl PcmReader for Zeros {
        async fn read(&mut self, buf: &mut [u8]) -> anyhow::Result<usize> {
            let n = buf.len().min(self.0);
            buf[..n].fill(0);
            self.0 -= n;
            Ok(n)
        }
    }

    #[tokio::test]
    async fn ffmpeg_encodes_one_second_of_silence() {
        let buf = StreamBuf::new();
        let enc = FfmpegMp3 {
            ffmpeg: "ffmpeg".into(),
        };
        let task = enc.start(Box::new(Zeros(192_000)), buf.clone());
        task.await.unwrap().unwrap();
        assert!(buf.is_done());
        let bytes = buf.snapshot();
        assert!(
            bytes.len() >= 20 * 576 && bytes.len() <= 50 * 576,
            "{}",
            bytes.len()
        );
        assert!(bytes[0] == 0xFF && bytes[1] & 0xE0 == 0xE0);
    }
}
