//! The engine used while no output is chosen: accepts everything, plays nothing.

use std::time::Duration;

use super::AudioEngine;
use crate::source::PcmReader;

pub struct NullEngine;

impl AudioEngine for NullEngine {
    fn play(&mut self, _reader: Box<dyn PcmReader>, _generation: u64) -> anyhow::Result<()> {
        Ok(())
    }

    fn pause(&mut self) -> anyhow::Result<()> {
        Ok(())
    }

    fn resume(&mut self) -> anyhow::Result<()> {
        Ok(())
    }

    fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }

    fn latency(&self) -> Duration {
        Duration::ZERO
    }
}
