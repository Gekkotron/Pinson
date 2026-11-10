//! The engine behind a Chromecast output: it encodes each song to MP3, offers it to the
//! device at `/api/cast/stream/{generation}` and tells the session to load it.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;

use super::CastStreams;
use super::session::{CastMeta, CastSession, LoadReq, SessionGuard};
use crate::engine::{AudioEngine, EngineEvent};
use crate::output::browser::stream::watch_encoder;
use crate::output::browser::{Encoder, Stream, StreamBuf};
use crate::song::Song;
use crate::source::PcmReader;

pub struct CastEngine {
    session: CastSession,
    encoder: Arc<dyn Encoder>,
    streams: CastStreams,
    events: mpsc::UnboundedSender<EngineEvent>,
    /// What `song` announced for the next `play_from`.
    meta: Option<CastMeta>,
    /// The generation of the stream this engine registered, if any.
    generation: Option<u64>,
    /// Dropping it stops the app on the device and ends the session task.
    _guard: Option<SessionGuard>,
}

impl CastEngine {
    pub fn new(
        session: CastSession,
        encoder: Arc<dyn Encoder>,
        streams: CastStreams,
        events: mpsc::UnboundedSender<EngineEvent>,
    ) -> Self {
        Self {
            session,
            encoder,
            streams,
            events,
            meta: None,
            generation: None,
            _guard: None,
        }
    }

    /// Makes the engine own the session's task: it ends when the engine is dropped.
    pub fn with_guard(mut self, guard: SessionGuard) -> Self {
        self._guard = Some(guard);
        self
    }

    fn clear_stream(&mut self) {
        if let Some(generation) = self.generation.take() {
            self.streams.clear_if(generation);
        }
    }
}

impl AudioEngine for CastEngine {
    fn song(&mut self, song: &Song) {
        self.meta = Some(CastMeta {
            title: song.title.clone(),
            artist: song.artists.clone(),
            album: song.album.clone(),
            image: song.artwork_url.clone(),
        });
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
        self.clear_stream();
        let buf = StreamBuf::new();
        let encoder_task = self.encoder.start(reader, buf.clone());
        let task = watch_encoder(generation, encoder_task, self.events.clone());
        let stream = Arc::new(Stream::new(generation, start, buf, task));
        self.streams.set(Some(stream));
        self.generation = Some(generation);
        self.session.load(LoadReq {
            generation,
            path: format!("/api/cast/stream/{generation}"),
            from: start,
            meta: self.meta.take(),
        });
        Ok(())
    }

    fn pause(&mut self) -> anyhow::Result<()> {
        self.session.pause();
        Ok(())
    }

    fn resume(&mut self) -> anyhow::Result<()> {
        self.session.resume();
        Ok(())
    }

    fn stop(&mut self) -> anyhow::Result<()> {
        self.session.stop();
        self.clear_stream();
        Ok(())
    }

    fn latency(&self) -> Duration {
        Duration::ZERO
    }

    fn reports_position(&self) -> bool {
        true
    }
}

impl Drop for CastEngine {
    fn drop(&mut self) {
        self.clear_stream();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::fake::FakeReader;
    use crate::output::cast::fake::FakeReceiver;
    use crate::output::cast::session::{SessionConfig, spawn};
    use crate::output::fake::Passthrough;
    use tokio::task::JoinHandle;

    struct Rig {
        fake: FakeReceiver,
        engine: CastEngine,
        streams: CastStreams,
        events: mpsc::UnboundedReceiver<EngineEvent>,
    }

    fn rig_with(encoder: Arc<dyn Encoder>) -> Rig {
        let fake = FakeReceiver::new();
        let (tx, events) = mpsc::unbounded_channel();
        let cfg = SessionConfig::new(fake.connector(), None, 8082);
        let (session, guard) = spawn(cfg, tx.clone(), Arc::new(|_| {}));
        let streams = CastStreams::default();
        let engine = CastEngine::new(session, encoder, streams.clone(), tx).with_guard(guard);
        Rig {
            fake,
            engine,
            streams,
            events,
        }
    }

    fn rig() -> Rig {
        rig_with(Arc::new(Passthrough))
    }

    fn reader() -> Box<dyn PcmReader> {
        Box::new(FakeReader {
            video_id: "a".into(),
        })
    }

    fn song() -> Song {
        Song {
            video_id: "a".into(),
            title: "Title A".into(),
            artists: "Artist A".into(),
            album: Some("Album A".into()),
            duration_secs: Some(200),
            artwork_url: Some("http://pinson.lan/a.jpg".into()),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn play_from_registers_stream_and_loads() {
        let mut r = rig();
        r.engine.song(&song());
        r.engine
            .play_from(reader(), 3, Duration::from_secs(42))
            .unwrap();
        assert!(r.streams.get(3).is_some());
        let load = r.fake.wait_for("LOAD").await;
        let m = &load.2["media"];
        assert_eq!(
            m["contentId"],
            "http://127.0.0.1:8082/api/cast/stream/3?fromMs=42000"
        );
        assert_eq!(m["metadata"]["title"], "Title A");
        assert_eq!(m["metadata"]["artist"], "Artist A");
        assert_eq!(m["metadata"]["albumName"], "Album A");
    }

    #[tokio::test(start_paused = true)]
    async fn stop_clears_stream_and_stops_media() {
        let mut r = rig();
        r.engine.song(&song());
        r.engine.play(reader(), 3).unwrap();
        r.fake.wait_for("LOAD").await;
        r.engine.stop().unwrap();
        assert!(r.streams.get(3).is_none());
        r.fake.wait_for("STOP").await;
    }

    #[tokio::test(start_paused = true)]
    async fn dropping_the_engine_clears_its_stream() {
        let mut r = rig();
        r.engine.play(reader(), 3).unwrap();
        drop(r.engine);
        assert!(r.streams.get(3).is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn a_newer_stream_survives_an_old_engine_drop() {
        let mut r = rig();
        r.engine.play(reader(), 3).unwrap();
        let newer = Arc::new(Stream::new(
            4,
            Duration::ZERO,
            StreamBuf::new(),
            tokio::spawn(async { Ok(()) }),
        ));
        r.streams.set(Some(newer));
        drop(r.engine);
        assert!(r.streams.get(4).is_some());
    }

    struct Failing;

    impl Encoder for Failing {
        fn start(
            &self,
            _reader: Box<dyn PcmReader>,
            _out: Arc<StreamBuf>,
        ) -> JoinHandle<anyhow::Result<()>> {
            tokio::spawn(async { anyhow::bail!("ffmpeg died") })
        }
    }

    #[tokio::test(start_paused = true)]
    async fn encoder_failure_reports_track_failed() {
        let mut r = rig_with(Arc::new(Failing));
        r.engine.play(reader(), 1).unwrap();
        match r.events.recv().await.unwrap() {
            EngineEvent::TrackFailed { generation, reason } => {
                assert_eq!(generation, 1);
                assert!(reason.contains("ffmpeg died"), "{reason}");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn reports_position_without_latency() {
        let r = rig();
        assert!(r.engine.reports_position());
        assert_eq!(r.engine.latency(), Duration::ZERO);
    }
}
