//! The engine behind a UPnP output: it encodes each song to MP3, offers it to the renderer
//! at `/api/upnp/stream/{generation}/song.mp3` and tells the session to load it.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;

use super::session::{LoadReq, SessionGuard, UpnpSession};
use crate::engine::{AudioEngine, EngineEvent};
use crate::output::browser::stream::watch_encoder;
use crate::output::browser::{Encoder, Stream, StreamBuf};
use crate::output::renderer::{SongMeta, SongStreams};
use crate::song::Song;
use crate::source::PcmReader;

pub struct UpnpEngine {
    session: UpnpSession,
    encoder: Arc<dyn Encoder>,
    streams: SongStreams,
    events: mpsc::UnboundedSender<EngineEvent>,
    /// What `song` announced for the next `play_from`.
    meta: Option<SongMeta>,
    /// The generation of the stream this engine registered, if any.
    generation: Option<u64>,
    /// Dropping it stops the renderer and ends the session task.
    _guard: Option<SessionGuard>,
}

impl UpnpEngine {
    pub fn new(
        session: UpnpSession,
        encoder: Arc<dyn Encoder>,
        streams: SongStreams,
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

impl AudioEngine for UpnpEngine {
    fn song(&mut self, song: &Song) {
        self.meta = Some(SongMeta {
            title: song.title.clone(),
            artist: song.artists.clone(),
            album: song.album.clone(),
            image: song.artwork_url.clone(),
            duration: song
                .duration_secs
                .map(|d| Duration::from_secs(u64::from(d))),
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
            path: format!("/api/upnp/stream/{generation}/song.mp3"),
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

    /// The player also stops before a seek, Previous or a skip: the session folds a stop
    /// followed by a load into one.
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

impl Drop for UpnpEngine {
    fn drop(&mut self) {
        self.clear_stream();
    }
}

#[cfg(test)]
mod tests {
    use std::io;

    use futures_util::future::BoxFuture;
    use tokio::task::JoinHandle;
    use tokio::time::sleep;

    use super::*;
    use crate::engine::fake::FakeReader;
    use crate::output::fake::Passthrough;
    use crate::output::upnp::fake::FakeControl;
    use crate::output::upnp::session::{SessionConfig, UpnpConnector, UpnpLink, spawn};

    struct FakeConnector(FakeControl);

    impl UpnpConnector for FakeConnector {
        fn target(&self) -> String {
            "fake renderer".into()
        }
        fn connect(&self) -> BoxFuture<'static, io::Result<UpnpLink>> {
            let link = UpnpLink {
                control: Arc::new(self.0.clone()),
                local_ip: "10.0.0.2".parse().unwrap(),
                sonos: false,
            };
            Box::pin(async move { Ok(link) })
        }
    }

    struct Rig {
        ctl: FakeControl,
        engine: UpnpEngine,
        streams: SongStreams,
        events: mpsc::UnboundedReceiver<EngineEvent>,
    }

    fn rig_with(encoder: Arc<dyn Encoder>) -> Rig {
        let ctl = FakeControl::new();
        let (tx, events) = mpsc::unbounded_channel();
        let cfg = SessionConfig::new(Arc::new(FakeConnector(ctl.clone())), None, 8082);
        let (session, guard) = spawn(cfg, tx.clone(), Arc::new(|_| {}));
        let streams = SongStreams::default();
        let engine = UpnpEngine::new(session, encoder, streams.clone(), tx).with_guard(guard);
        Rig {
            ctl,
            engine,
            streams,
            events,
        }
    }

    fn rig() -> Rig {
        rig_with(Arc::new(Passthrough))
    }

    /// Lets the session task run until the fake has seen `n` calls starting with `prefix`.
    async fn wait_for(ctl: &FakeControl, prefix: &str, n: usize) {
        for _ in 0..200 {
            if ctl.count(prefix) >= n {
                return;
            }
            sleep(Duration::from_millis(50)).await;
        }
        panic!("no {prefix} call: {:?}", ctl.calls());
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
        wait_for(&r.ctl, "SetAVTransportURI", 1).await;
        let call = r
            .ctl
            .calls()
            .into_iter()
            .find(|c| c.starts_with("SetAVTransportURI"))
            .unwrap();
        assert_eq!(
            call,
            "SetAVTransportURI http://10.0.0.2:8082/api/upnp/stream/3/song.mp3?fromMs=42000"
        );
        let didl = r.ctl.last_metadata();
        assert!(didl.contains("Title A"), "{didl}");
        assert!(didl.contains("Artist A"), "{didl}");
    }

    #[tokio::test(start_paused = true)]
    async fn stop_clears_stream() {
        let mut r = rig();
        r.engine.song(&song());
        r.engine.play(reader(), 3).unwrap();
        wait_for(&r.ctl, "SetAVTransportURI", 1).await;
        r.engine.stop().unwrap();
        assert!(r.streams.get(3).is_none());
        wait_for(&r.ctl, "Stop", 1).await;
    }

    #[tokio::test(start_paused = true)]
    async fn dropping_the_engine_clears_its_stream() {
        let mut r = rig();
        r.engine.play(reader(), 3).unwrap();
        drop(r.engine);
        assert!(r.streams.get(3).is_none());
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
