//! Audio output. The player drives an `AudioEngine`; engines report back
//! through an `mpsc::UnboundedSender<EngineEvent>` given at construction.

pub mod fake;
pub mod log;
pub mod nidoo;

use std::time::Duration;

use crate::source::PcmReader;

#[derive(Clone, Debug, PartialEq)]
pub enum EngineEvent {
    /// The reader for `generation` ended and its last byte has been written.
    TrackEnded { generation: u64 },
    /// Playback of `generation` failed (bad PCM, read error).
    TrackFailed { generation: u64, reason: String },
}

pub trait AudioEngine: Send + 'static {
    /// Replaces whatever is playing and starts playing `reader`, even if paused.
    fn play(&mut self, reader: Box<dyn PcmReader>, generation: u64) -> anyhow::Result<()>;
    fn pause(&mut self) -> anyhow::Result<()>;
    fn resume(&mut self) -> anyhow::Result<()>;
    /// Goes idle: closes the output and drops the reader.
    fn stop(&mut self) -> anyhow::Result<()>;
    /// How far the speakers are behind what the engine has written.
    fn latency(&self) -> Duration;
}
