//! An engine that only logs. It never ends a track; use it to try the page
//! and the API without speakers (`--engine log`).

use std::time::Duration;

use super::AudioEngine;
use crate::source::PcmReader;

#[derive(Default)]
pub struct LoggingEngine {
    /// Kept so the source stays open while "playing", like a real engine.
    reader: Option<Box<dyn PcmReader>>,
}

impl AudioEngine for LoggingEngine {
    fn play(&mut self, reader: Box<dyn PcmReader>, generation: u64) -> anyhow::Result<()> {
        tracing::info!(generation, "engine: play");
        self.reader = Some(reader);
        Ok(())
    }

    fn pause(&mut self) -> anyhow::Result<()> {
        tracing::info!("engine: pause");
        Ok(())
    }

    fn resume(&mut self) -> anyhow::Result<()> {
        tracing::info!("engine: resume");
        Ok(())
    }

    fn stop(&mut self) -> anyhow::Result<()> {
        tracing::info!("engine: stop");
        self.reader = None;
        Ok(())
    }

    fn latency(&self) -> Duration {
        Duration::ZERO
    }
}
