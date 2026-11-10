//! Test doubles: an engine that records calls and a source of fake readers.

use std::any::Any;
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::bail;
use async_trait::async_trait;

use super::AudioEngine;
use crate::source::{PcmReader, PcmSource};

/// Records "play:<videoId>", "pause", "resume" and "stop".
#[derive(Clone, Default)]
pub struct FakeEngine {
    pub calls: Arc<Mutex<Vec<String>>>,
    /// Generation of the most recent `play`.
    pub last_generation: Arc<AtomicU64>,
    pub latency: Duration,
    pub reports: bool,
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

    pub fn calls(&self) -> Vec<String> {
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
    fn play(&mut self, reader: Box<dyn PcmReader>, generation: u64) -> anyhow::Result<()> {
        let any: &dyn Any = &*reader;
        let id = any
            .downcast_ref::<FakeReader>()
            .map_or("?", |r| r.video_id.as_str());
        self.record(format!("play:{id}"));
        self.last_generation.store(generation, Ordering::SeqCst);
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

/// Opens `FakeReader`s after `delay`; ids in `failing` fail to open.
#[derive(Clone, Default)]
pub struct FakeSource {
    pub delay: Duration,
    pub failing: HashSet<String>,
    /// Every `open` call, in order (recorded when the open starts).
    pub opens: Arc<Mutex<Vec<String>>>,
    /// The same calls with where each one starts in the song.
    pub starts: Arc<Mutex<Vec<(String, Duration)>>>,
}

impl FakeSource {
    pub fn opens(&self) -> Vec<String> {
        self.opens.lock().unwrap().clone()
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
        Ok(Box::new(FakeReader {
            video_id: video_id.to_owned(),
        }))
    }
}
