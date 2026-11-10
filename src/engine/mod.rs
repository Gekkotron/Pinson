//! Audio output. The player drives an `AudioEngine`; engines report back
//! through an `mpsc::UnboundedSender<EngineEvent>` given at construction.

pub mod fake;
pub mod log;
pub mod null;
pub mod paced;

use std::time::Duration;

use crate::source::PcmReader;

#[derive(Clone, Debug, PartialEq)]
pub enum EngineEvent {
    /// The reader for `generation` ended and its last byte has been written.
    TrackEnded { generation: u64 },
    /// Playback of `generation` failed (bad PCM, read error).
    TrackFailed { generation: u64, reason: String },
    /// At `at` the listener hears `position` of `generation`. Only engines
    /// whose `reports_position` is true send it.
    Playing {
        generation: u64,
        position: Duration,
        at: tokio::time::Instant,
    },
}

pub trait AudioEngine: Send + 'static {
    /// Replaces whatever is playing and starts playing `reader`, even if paused.
    fn play(&mut self, reader: Box<dyn PcmReader>, generation: u64) -> anyhow::Result<()>;
    /// Like `play`, for a stream that starts `start` into the song.
    fn play_from(
        &mut self,
        reader: Box<dyn PcmReader>,
        generation: u64,
        _start: Duration,
    ) -> anyhow::Result<()> {
        self.play(reader, generation)
    }
    fn pause(&mut self) -> anyhow::Result<()>;
    fn resume(&mut self) -> anyhow::Result<()>;
    /// Goes idle: closes the output and drops the reader.
    fn stop(&mut self) -> anyhow::Result<()>;
    /// How far the speakers are behind what the engine has written.
    fn latency(&self) -> Duration;
    /// Whether the engine tells the player where the listener is with
    /// `EngineEvent::Playing`. Such engines return `Duration::ZERO` from
    /// `latency()`: their reports already include any buffering.
    fn reports_position(&self) -> bool {
        false
    }
}
