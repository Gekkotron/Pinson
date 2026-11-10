//! The engine behind the "This device" output: it encodes each song to MP3
//! and tells the player tab to load the stream.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;

use super::hub::{DeviceCommand, DeviceHub, stream_url};
use super::stream::{Encoder, Stream, StreamBuf, watch_encoder};
use crate::engine::{AudioEngine, EngineEvent};
use crate::source::PcmReader;

pub struct BrowserEngine {
    hub: DeviceHub,
    device_id: String,
    token: u64,
    encoder: Arc<dyn Encoder>,
    events: mpsc::UnboundedSender<EngineEvent>,
}

impl BrowserEngine {
    pub fn new(
        hub: DeviceHub,
        device_id: String,
        encoder: Arc<dyn Encoder>,
        events: mpsc::UnboundedSender<EngineEvent>,
    ) -> Self {
        let token = hub.attach(&device_id, events.clone());
        Self {
            hub,
            device_id,
            token,
            encoder,
            events,
        }
    }

    fn command(&self, cmd: DeviceCommand) {
        self.hub.command(self.token, cmd);
    }
}

impl AudioEngine for BrowserEngine {
    fn play(&mut self, reader: Box<dyn PcmReader>, generation: u64) -> anyhow::Result<()> {
        self.play_from(reader, generation, Duration::ZERO)
    }

    fn play_from(
        &mut self,
        reader: Box<dyn PcmReader>,
        generation: u64,
        start: Duration,
    ) -> anyhow::Result<()> {
        let buf = StreamBuf::new();
        let encoder_task = self.encoder.start(reader, buf.clone());
        let task = watch_encoder(generation, encoder_task, self.events.clone());
        let stream = Arc::new(Stream::new(generation, start, buf, task));
        let chained = self.hub.set_stream(self.token, stream);
        let start_ms = start.as_millis() as u64;
        self.command(DeviceCommand::Load {
            generation,
            url: stream_url(&self.device_id, generation, start_ms),
            start_ms,
            chained,
            paused: false,
        });
        Ok(())
    }

    fn pause(&mut self) -> anyhow::Result<()> {
        self.command(DeviceCommand::Pause);
        Ok(())
    }

    fn resume(&mut self) -> anyhow::Result<()> {
        self.command(DeviceCommand::Resume);
        Ok(())
    }

    fn stop(&mut self) -> anyhow::Result<()> {
        self.hub.clear_stream(self.token);
        self.command(DeviceCommand::Stop);
        Ok(())
    }

    fn latency(&self) -> Duration {
        Duration::ZERO
    }

    fn reports_position(&self) -> bool {
        true
    }
}

impl Drop for BrowserEngine {
    fn drop(&mut self) {
        self.command(DeviceCommand::Stop);
        self.hub.detach(self.token);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::engine::fake::{FakeEngine, FakeReader, FakeSource};
    use crate::history::History;
    use crate::output::browser::hub::Hello;
    use crate::output::fake::Passthrough;
    use crate::player::{Autoplay, spawn_player};
    use crate::radio::fake::FakeRadio;
    use tokio::task::JoinHandle;

    fn hub() -> DeviceHub {
        let (_tx, rx) = mpsc::unbounded_channel();
        let player = spawn_player(
            Some(Box::new(FakeEngine::new(Duration::ZERO))),
            rx,
            Arc::new(FakeSource::default()),
            Autoplay {
                radio: Arc::new(FakeRadio::default()),
                history: History::new(Db::in_memory()),
                enabled: false,
            },
        );
        DeviceHub::new(player, Duration::from_secs(10))
    }

    fn reader() -> Box<dyn PcmReader> {
        Box::new(FakeReader {
            video_id: "a".into(),
        })
    }

    fn connect(
        hub: &DeviceHub,
    ) -> (
        super::super::hub::SessionGuard,
        mpsc::UnboundedReceiver<DeviceCommand>,
    ) {
        connect_as(hub, "d")
    }

    fn connect_as(
        hub: &DeviceHub,
        device: &str,
    ) -> (
        super::super::hub::SessionGuard,
        mpsc::UnboundedReceiver<DeviceCommand>,
    ) {
        hub.connect(Hello {
            device_id: device.into(),
            name: "tab".into(),
            loaded: None,
        })
    }

    fn engine(hub: &DeviceHub) -> (BrowserEngine, mpsc::UnboundedReceiver<EngineEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            BrowserEngine::new(hub.clone(), "d".into(), Arc::new(Passthrough), tx),
            rx,
        )
    }

    async fn settle() {
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test]
    async fn play_sends_load_with_start() {
        let hub = hub();
        let (_g, mut session) = connect(&hub);
        let (mut e, _ev) = engine(&hub);
        e.play_from(reader(), 2, Duration::from_secs(42)).unwrap();
        assert_eq!(
            session.try_recv().unwrap(),
            DeviceCommand::Load {
                generation: 2,
                url: "/api/device/stream/2?device=d&fromMs=42000".into(),
                start_ms: 42000,
                chained: false,
                paused: false,
            }
        );
        assert!(hub.stream("d", 2).is_some());
    }

    #[tokio::test]
    async fn play_while_next_waits_is_chained() {
        let hub = hub();
        let (_g, mut session) = connect(&hub);
        let (mut e, _ev) = engine(&hub);
        e.play_from(reader(), 1, Duration::ZERO).unwrap();
        session.try_recv().unwrap();
        let h2 = hub.clone();
        let task = tokio::spawn(async move { h2.next("d", 1).await });
        settle().await;
        e.play_from(reader(), 2, Duration::ZERO).unwrap();
        assert!(matches!(
            session.try_recv().unwrap(),
            DeviceCommand::Load {
                generation: 2,
                chained: true,
                ..
            }
        ));
        assert_eq!(task.await.unwrap().unwrap().generation, 2);
    }

    #[tokio::test]
    async fn pause_resume_stop_send_commands() {
        let hub = hub();
        let (_g, mut session) = connect(&hub);
        let (mut e, _ev) = engine(&hub);
        e.play(reader(), 1).unwrap();
        session.try_recv().unwrap();
        e.pause().unwrap();
        e.resume().unwrap();
        e.stop().unwrap();
        assert_eq!(session.try_recv().unwrap(), DeviceCommand::Pause);
        assert_eq!(session.try_recv().unwrap(), DeviceCommand::Resume);
        assert_eq!(session.try_recv().unwrap(), DeviceCommand::Stop);
        assert!(session.try_recv().is_err());
        assert!(hub.stream("d", 1).is_none());
    }

    #[tokio::test]
    async fn drop_sends_stop() {
        let hub = hub();
        let (_g, mut session) = connect(&hub);
        let (e, _ev) = engine(&hub);
        drop(e);
        assert_eq!(session.try_recv().unwrap(), DeviceCommand::Stop);
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

    #[tokio::test]
    async fn encoder_failure_reports_track_failed() {
        let hub = hub();
        let (_g, _session) = connect(&hub);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut e = BrowserEngine::new(hub.clone(), "d".into(), Arc::new(Failing), tx);
        e.play(reader(), 1).unwrap();
        match rx.recv().await.unwrap() {
            EngineEvent::TrackFailed { generation, reason } => {
                assert_eq!(generation, 1);
                assert!(reason.contains("ffmpeg died"), "{reason}");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn reports_position_without_latency() {
        let hub = hub();
        let (e, _ev) = engine(&hub);
        assert!(e.reports_position());
        assert_eq!(e.latency(), Duration::ZERO);
    }

    #[tokio::test]
    async fn switching_devices_stops_old_tab() {
        let hub = hub();
        let (_g1, mut s1) = connect_as(&hub, "d1");
        let (_g2, mut s2) = connect_as(&hub, "d2");
        let (tx, _ev) = mpsc::unbounded_channel();
        let mut a = BrowserEngine::new(hub.clone(), "d1".into(), Arc::new(Passthrough), tx);
        a.play_from(reader(), 1, Duration::ZERO).unwrap();
        s1.try_recv().unwrap();
        let (tx, _ev2) = mpsc::unbounded_channel();
        let _b = BrowserEngine::new(hub.clone(), "d2".into(), Arc::new(Passthrough), tx);
        a.stop().unwrap();
        drop(a);
        assert_eq!(s1.try_recv().unwrap(), DeviceCommand::Stop);
        assert!(s2.try_recv().is_err());
        assert!(hub.stream("d1", 1).is_none());
    }

    #[tokio::test]
    async fn drop_after_newer_attach_keeps_new_attachment() {
        let hub = hub();
        let (_g1, _s1) = connect_as(&hub, "d1");
        let (_g2, mut s2) = connect_as(&hub, "d2");
        let (tx, _ev) = mpsc::unbounded_channel();
        let a = BrowserEngine::new(hub.clone(), "d1".into(), Arc::new(Passthrough), tx);
        let (tx, _ev2) = mpsc::unbounded_channel();
        let mut b = BrowserEngine::new(hub.clone(), "d2".into(), Arc::new(Passthrough), tx);
        drop(a);
        b.play_from(reader(), 1, Duration::ZERO).unwrap();
        assert!(matches!(
            s2.try_recv().unwrap(),
            DeviceCommand::Load { generation: 1, .. }
        ));
        assert!(hub.stream("d2", 1).is_some());
    }
}
