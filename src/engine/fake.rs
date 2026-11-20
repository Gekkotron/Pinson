//! Test doubles: an engine that records calls and a source of fake readers.

use std::any::Any;
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::bail;
use async_trait::async_trait;

use super::AudioEngine;
use crate::fade::Fade;
use crate::source::{PcmReader, PcmSource};

/// Records "song:<videoId>", "play:<videoId>", "pause", "resume" and "stop".
#[derive(Clone, Default)]
pub struct FakeEngine {
    pub calls: Arc<Mutex<Vec<String>>>,
    /// Generation of the most recent `play`.
    pub last_generation: Arc<AtomicU64>,
    pub latency: Duration,
    pub reports: bool,
    /// The reader of the most recent `play`, for tests that read its audio.
    pub last_reader: Arc<Mutex<Option<Box<dyn PcmReader>>>>,
}

impl FakeEngine {
    pub fn new(latency: Duration) -> Self {
        Self {
            latency,
            ..Self::default()
        }
    }

    /// An engine that reports the heard position (and has no latency).
    pub fn reporting() -> Self {
        Self {
            reports: true,
            ..Self::default()
        }
    }

    /// Every call except `song:*`, which only some tests care about.
    pub fn calls(&self) -> Vec<String> {
        let mut calls = self.all_calls();
        calls.retain(|c| !c.starts_with("song:"));
        calls
    }

    /// Every call, including `song:<videoId>` (recorded right before each `play`).
    pub fn all_calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    pub fn generation(&self) -> u64 {
        self.last_generation.load(Ordering::SeqCst)
    }

    fn record(&self, call: String) {
        self.calls.lock().unwrap().push(call);
    }
}

impl AudioEngine for FakeEngine {
    fn song(&mut self, song: &crate::song::Song) {
        self.record(format!("song:{}", song.video_id));
    }

    fn play(&mut self, reader: Box<dyn PcmReader>, generation: u64) -> anyhow::Result<()> {
        self.play_from(reader, generation, Duration::ZERO)
    }

    fn play_from(
        &mut self,
        reader: Box<dyn PcmReader>,
        generation: u64,
        start: Duration,
    ) -> anyhow::Result<()> {
        let id = video_id_of(&*reader);
        let id = id.as_str();
        if start.is_zero() {
            self.record(format!("play:{id}"));
        } else {
            self.record(format!("play:{id}@{}", start.as_millis()));
        }
        self.last_generation.store(generation, Ordering::SeqCst);
        *self.last_reader.lock().unwrap() = Some(reader);
        Ok(())
    }

    fn pause(&mut self) -> anyhow::Result<()> {
        self.record("pause".into());
        Ok(())
    }

    fn resume(&mut self) -> anyhow::Result<()> {
        self.record("resume".into());
        Ok(())
    }

    fn stop(&mut self) -> anyhow::Result<()> {
        self.record("stop".into());
        Ok(())
    }

    fn latency(&self) -> Duration {
        self.latency
    }

    fn reports_position(&self) -> bool {
        self.reports
    }
}

/// The video id a fake reader carries, looking through a `Fade` wrapper;
/// "?" for any other reader.
fn video_id_of(reader: &dyn PcmReader) -> String {
    let any: &dyn Any = reader;
    if let Some(fade) = any.downcast_ref::<Fade>() {
        return video_id_of(fade.inner());
    }
    if let Some(r) = any.downcast_ref::<FakeReader>() {
        return r.video_id.clone();
    }
    if let Some(r) = any.downcast_ref::<ToneReader>() {
        return r.video_id.clone();
    }
    "?".into()
}

/// A reader that carries its video id and produces no audio.
pub struct FakeReader {
    pub video_id: String,
}

#[async_trait]
impl PcmReader for FakeReader {
    async fn read(&mut self, _buf: &mut [u8]) -> anyhow::Result<usize> {
        Ok(0)
    }
}

/// A reader that never ends: every sample is `value`.
pub struct ToneReader {
    pub video_id: String,
    pub value: i16,
}

#[async_trait]
impl PcmReader for ToneReader {
    async fn read(&mut self, buf: &mut [u8]) -> anyhow::Result<usize> {
        let n = buf.len() & !1;
        for sample in buf[..n].as_chunks_mut::<2>().0 {
            *sample = self.value.to_le_bytes();
        }
        Ok(n)
    }
}

/// Opens `FakeReader`s after `delay`; ids in `failing` fail to open.
#[derive(Clone, Default)]
pub struct FakeSource {
    pub delay: Duration,
    pub failing: HashSet<String>,
    /// Open `ToneReader`s of this value instead of silent `FakeReader`s.
    pub tone: Option<i16>,
    /// Every `open` call, in order (recorded when the open starts).
    pub opens: Arc<Mutex<Vec<String>>>,
    /// The same calls with where each one starts in the song.
    pub starts: Arc<Mutex<Vec<(String, Duration)>>>,
    /// Every `prepare` call, in order.
    pub prepares: Arc<Mutex<Vec<String>>>,
}

impl FakeSource {
    pub fn opens(&self) -> Vec<String> {
        self.opens.lock().unwrap().clone()
    }

    pub fn prepares(&self) -> Vec<String> {
        self.prepares.lock().unwrap().clone()
    }

    pub fn starts(&self) -> Vec<(String, Duration)> {
        self.starts.lock().unwrap().clone()
    }
}

#[async_trait]
impl PcmSource for FakeSource {
    async fn open(&self, video_id: &str, start: Duration) -> anyhow::Result<Box<dyn PcmReader>> {
        self.opens.lock().unwrap().push(video_id.to_owned());
        self.starts
            .lock()
            .unwrap()
            .push((video_id.to_owned(), start));
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        if self.failing.contains(video_id) {
            bail!("ERROR: Video unavailable");
        }
        if let Some(value) = self.tone {
            return Ok(Box::new(ToneReader {
                video_id: video_id.to_owned(),
                value,
            }));
        }
        Ok(Box::new(FakeReader {
            video_id: video_id.to_owned(),
        }))
    }

    fn prepare(&self, video_id: &str) {
        self.prepares.lock().unwrap().push(video_id.to_owned());
    }
}
