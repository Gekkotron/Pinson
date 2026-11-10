//! Which browser tabs are connected, which one plays, and what happens when
//! the player tab goes away.
//!
//! Tabs say hello over a WebSocket (one `SessionGuard` each). The output
//! engine attaches to one device at a time; commands go to that device's
//! newest session. If that tab disappears for longer than the grace period,
//! the hub pauses Pinson; when the device comes back it is the player again.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::Instant;

use super::stream::Stream;
use crate::engine::EngineEvent;
use crate::player::{PlayerHandle, Status};

/// How long `next` waits for the player to pick the following song.
pub const NEXT_WAIT: Duration = Duration::from_secs(30);

/// What a tab says when its WebSocket opens.
pub struct Hello {
    pub device_id: String,
    pub name: String,
    pub loaded: Option<Loaded>,
}

/// What the tab's `<audio>` element holds.
pub struct Loaded {
    pub generation: Option<u64>,
    pub position_ms: u64,
    pub playing: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "type")]
pub enum DeviceCommand {
    #[serde(rename = "device.load", rename_all = "camelCase")]
    Load {
        generation: u64,
        url: String,
        start_ms: u64,
        chained: bool,
        paused: bool,
    },
    #[serde(rename = "device.pause")]
    Pause,
    #[serde(rename = "device.resume")]
    Resume,
    #[serde(rename = "device.stop")]
    Stop,
}

/// What the tab reports back.
pub enum DeviceReport {
    Position { generation: u64, position_ms: u64 },
    Failed { generation: u64, reason: String },
}

/// `/api/device/stream/{generation}?device=..&fromMs=..`, with the device id
/// percent-encoded.
pub fn stream_url(device_id: &str, generation: u64, from_ms: u64) -> String {
    let mut id = String::new();
    for b in device_id.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            id.push(b as char);
        } else {
            id.push_str(&format!("%{b:02X}"));
        }
    }
    format!("/api/device/stream/{generation}?device={id}&fromMs={from_ms}")
}

struct Session {
    id: u64,
    device: String,
    tx: mpsc::UnboundedSender<DeviceCommand>,
}

struct Waiter {
    after: u64,
    tx: oneshot::Sender<Option<Arc<Stream>>>,
}

struct Attached {
    token: u64,
    device: String,
    events: mpsc::UnboundedSender<EngineEvent>,
    stream: Option<Arc<Stream>>,
    /// The generation `TrackEnded` was last sent for.
    ended_sent: Option<u64>,
    waiter: Option<Waiter>,
}

#[derive(Default)]
struct State {
    counter: u64,
    /// In hello order: the last session of a device is its target.
    sessions: Vec<Session>,
    links: HashMap<String, watch::Sender<Option<bool>>>,
    attached: Option<Attached>,
    /// The generation last handed to each device by `stream` or `next`.
    served: HashMap<String, u64>,
    /// Bumped whenever a device connects, loses its last session, or asks for
    /// a stream. A grace timer pauses only if it has not moved.
    epoch: HashMap<String, u64>,
    paused_by_hub: bool,
}

impl State {
    fn bump(&mut self, device: &str) -> u64 {
        let e = self.epoch.entry(device.to_string()).or_default();
        *e += 1;
        *e
    }

    fn link_tx(&mut self, device: &str) -> &watch::Sender<Option<bool>> {
        self.links
            .entry(device.to_string())
            .or_insert_with(|| watch::channel(Some(false)).0)
    }

    fn has_session(&self, device: &str) -> bool {
        self.sessions.iter().any(|s| s.device == device)
    }

    fn target(&self, device: &str) -> Option<&Session> {
        self.sessions.iter().rev().find(|s| s.device == device)
    }

    fn attached_to(&mut self, device: &str) -> Option<&mut Attached> {
        self.attached.as_mut().filter(|a| a.device == device)
    }
}

#[derive(Clone)]
pub struct DeviceHub {
    state: Arc<Mutex<State>>,
    player: PlayerHandle,
    grace: Duration,
}

/// Dropping it disconnects the session.
pub struct SessionGuard {
    hub: DeviceHub,
    id: u64,
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        self.hub.disconnect(self.id);
    }
}

impl DeviceHub {
    pub fn new(player: PlayerHandle, grace: Duration) -> Self {
        Self {
            state: Arc::new(Mutex::new(State::default())),
            player,
            grace,
        }
    }

    fn position_ms(&self) -> u64 {
        self.player.state().view(Instant::now()).position_ms
    }

    pub fn connect(&self, hello: Hello) -> (SessionGuard, mpsc::UnboundedReceiver<DeviceCommand>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let device = hello.device_id.clone();
        let position = self.position_ms();
        let mut resume = false;
        let id = {
            let mut st = self.state.lock().unwrap();
            st.counter += 1;
            let id = st.counter;
            st.bump(&device);
            st.sessions.push(Session {
                id,
                device: device.clone(),
                tx: tx.clone(),
            });
            st.link_tx(&device).send_replace(Some(true));

            let served = st.served.get(&device).copied();
            let paused_by_hub = st.paused_by_hub;
            let pinson_playing = self.player.state().status == Status::Playing;
            let mut cleared = false;
            if let Some(att) = st.attached_to(&device)
                && let Some(cur) = att.stream.clone()
            {
                let tab_gen = hello.loaded.as_ref().and_then(|l| l.generation).or(served);
                if tab_gen == Some(cur.generation) {
                    let loaded = hello.loaded.as_ref();
                    let will_resume = loaded.is_some_and(|l| l.playing) && paused_by_hub;
                    let _ = tx.send(DeviceCommand::Load {
                        generation: cur.generation,
                        url: stream_url(&device, cur.generation, position),
                        start_ms: loaded.map_or(position, |l| l.position_ms),
                        chained: true,
                        paused: !(pinson_playing || will_resume),
                    });
                    if will_resume {
                        resume = true;
                        cleared = true;
                    }
                } else {
                    let _ = tx.send(DeviceCommand::Load {
                        generation: cur.generation,
                        url: stream_url(&device, cur.generation, position),
                        start_ms: position,
                        chained: false,
                        paused: true,
                    });
                }
            }
            if cleared {
                st.paused_by_hub = false;
            }
            id
        };
        if resume {
            let player = self.player.clone();
            tokio::spawn(async move {
                let _ = player.resume().await;
            });
        }
        (
            SessionGuard {
                hub: self.clone(),
                id,
            },
            rx,
        )
    }

    fn disconnect(&self, id: u64) {
        let (device, epoch) = {
            let mut st = self.state.lock().unwrap();
            let Some(i) = st.sessions.iter().position(|s| s.id == id) else {
                return;
            };
            let device = st.sessions.remove(i).device;
            if st.has_session(&device) {
                return;
            }
            st.link_tx(&device).send_replace(Some(false));
            let epoch = st.bump(&device);
            (device, epoch)
        };
        let hub = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(hub.grace).await;
            hub.grace_over(&device, epoch);
        });
    }

    fn grace_over(&self, device: &str, epoch: u64) {
        let mut st = self.state.lock().unwrap();
        let unchanged = st.epoch.get(device) == Some(&epoch) && !st.has_session(device);
        let attached = st.attached.as_ref().is_some_and(|a| a.device == device);
        if !(unchanged && attached) {
            return;
        }
        if self.player.state().status != Status::Playing {
            return;
        }
        st.paused_by_hub = true;
        drop(st);
        let player = self.player.clone();
        tokio::spawn(async move {
            let _ = player.pause().await;
        });
    }

    pub fn report(&self, device_id: &str, report: DeviceReport) {
        let mut st = self.state.lock().unwrap();
        let Some(att) = st.attached_to(device_id) else {
            return;
        };
        let event = match report {
            DeviceReport::Position {
                generation,
                position_ms,
            } => EngineEvent::Playing {
                generation,
                position: Duration::from_millis(position_ms),
                at: Instant::now(),
            },
            DeviceReport::Failed { generation, reason } => {
                EngineEvent::TrackFailed { generation, reason }
            }
        };
        let _ = att.events.send(event);
    }

    /// `Some(true)` while the device has at least one session.
    pub fn link(&self, device_id: &str) -> watch::Receiver<Option<bool>> {
        self.state.lock().unwrap().link_tx(device_id).subscribe()
    }

    pub fn stream(&self, device_id: &str, generation: u64) -> Option<Arc<Stream>> {
        let mut st = self.state.lock().unwrap();
        st.bump(device_id);
        let stream = st
            .attached_to(device_id)?
            .stream
            .clone()
            .filter(|s| s.generation == generation)?;
        st.served.insert(device_id.to_string(), generation);
        Some(stream)
    }

    /// The stream after `after`: tells the player `after` ended, then waits
    /// (at most `NEXT_WAIT`) for the following one.
    pub async fn next(&self, device_id: &str, after: u64) -> Option<Arc<Stream>> {
        let rx = {
            let mut st = self.state.lock().unwrap();
            st.bump(device_id);
            let att = st.attached_to(device_id)?;
            let cur = att.stream.clone()?;
            if cur.generation > after {
                st.served.insert(device_id.to_string(), cur.generation);
                return Some(cur);
            }
            if cur.generation < after {
                return None;
            }
            if att.ended_sent != Some(after) {
                att.ended_sent = Some(after);
                let _ = att
                    .events
                    .send(EngineEvent::TrackEnded { generation: after });
            }
            let (tx, rx) = oneshot::channel();
            att.waiter = Some(Waiter { after, tx });
            rx
        };
        let stream = tokio::time::timeout(NEXT_WAIT, rx).await.ok()?.ok()??;
        self.state
            .lock()
            .unwrap()
            .served
            .insert(device_id.to_string(), stream.generation);
        Some(stream)
    }

    /// Ties the engine's events to `device_id`; returns the token the engine
    /// uses from now on.
    pub(crate) fn attach(
        &self,
        device_id: &str,
        events: mpsc::UnboundedSender<EngineEvent>,
    ) -> u64 {
        let mut st = self.state.lock().unwrap();
        st.counter += 1;
        let token = st.counter;
        st.paused_by_hub = false;
        st.attached = Some(Attached {
            token,
            device: device_id.to_string(),
            events,
            stream: None,
            ended_sent: None,
            waiter: None,
        });
        token
    }

    pub(crate) fn detach(&self, token: u64) {
        let mut st = self.state.lock().unwrap();
        if st.attached.as_ref().is_some_and(|a| a.token == token) {
            st.attached = None;
        }
    }

    /// Sends `cmd` to the attached device's newest session.
    pub(crate) fn command(&self, token: u64, cmd: DeviceCommand) {
        let st = self.state.lock().unwrap();
        let Some(att) = st.attached.as_ref().filter(|a| a.token == token) else {
            return;
        };
        if let Some(session) = st.target(&att.device) {
            let _ = session.tx.send(cmd);
        }
    }

    /// Makes `stream` current; true when a waiting `next` takes it (the tab
    /// already plays it).
    pub(crate) fn set_stream(&self, token: u64, stream: Arc<Stream>) -> bool {
        let mut st = self.state.lock().unwrap();
        let Some(att) = st.attached.as_mut().filter(|a| a.token == token) else {
            return false;
        };
        att.stream = Some(stream.clone());
        match att.waiter.take() {
            Some(w) if stream.generation > w.after => w.tx.send(Some(stream)).is_ok(),
            _ => false,
        }
    }

    pub(crate) fn clear_stream(&self, token: u64) {
        let mut st = self.state.lock().unwrap();
        if let Some(att) = st.attached.as_mut().filter(|a| a.token == token) {
            att.stream = None;
            att.waiter = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::engine::fake::{FakeEngine, FakeSource};
    use crate::history::History;
    use crate::output::browser::stream::StreamBuf;
    use crate::player::{Autoplay, spawn_player};
    use crate::radio::fake::FakeRadio;
    use crate::song::Song;

    const GRACE: Duration = Duration::from_millis(50);

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
        DeviceHub::new(player, GRACE)
    }

    fn song(id: &str) -> Song {
        Song {
            video_id: id.into(),
            title: id.into(),
            artists: "artist".into(),
            album: None,
            duration_secs: Some(200),
            artwork_url: None,
        }
    }

    fn hello(device: &str, loaded: Option<Loaded>) -> Hello {
        Hello {
            device_id: device.into(),
            name: "tab".into(),
            loaded,
        }
    }

    fn stream(generation: u64) -> Arc<Stream> {
        Arc::new(Stream::new(
            generation,
            Duration::ZERO,
            StreamBuf::new(),
            tokio::spawn(async { Ok(()) }),
        ))
    }

    async fn settle() {
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
    }

    async fn pass(d: Duration) {
        settle().await;
        tokio::time::advance(d).await;
        settle().await;
    }

    fn paused(h: &DeviceHub) -> bool {
        h.player.state().status == Status::Paused
    }

    /// A hub whose player plays a song, attached to device "d" with stream 3.
    async fn playing() -> (DeviceHub, u64, mpsc::UnboundedReceiver<EngineEvent>) {
        let h = hub();
        h.player.play_all(vec![song("a")]).await.unwrap();
        settle().await;
        assert_eq!(h.player.state().status, Status::Playing);
        let (etx, erx) = mpsc::unbounded_channel();
        let token = h.attach("d", etx);
        h.set_stream(token, stream(3));
        (h, token, erx)
    }

    #[tokio::test(start_paused = true)]
    async fn commands_go_to_newest_session() {
        let h = hub();
        let (_g1, mut r1) = h.connect(hello("d", None));
        let (_g2, mut r2) = h.connect(hello("d", None));
        let token = h.attach("d", mpsc::unbounded_channel().0);
        h.command(token, DeviceCommand::Pause);
        assert_eq!(r2.try_recv().unwrap(), DeviceCommand::Pause);
        assert!(r1.try_recv().is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn link_follows_sessions() {
        let h = hub();
        let link = h.link("d");
        assert_eq!(*link.borrow(), Some(false));
        let (g1, _r1) = h.connect(hello("d", None));
        let (g2, _r2) = h.connect(hello("d", None));
        assert_eq!(*link.borrow(), Some(true));
        drop(g1);
        assert_eq!(*link.borrow(), Some(true));
        drop(g2);
        assert_eq!(*link.borrow(), Some(false));
    }

    #[tokio::test(start_paused = true)]
    async fn position_report_becomes_playing_event() {
        let h = hub();
        let (etx, mut erx) = mpsc::unbounded_channel();
        h.attach("d", etx);
        h.report(
            "other",
            DeviceReport::Position {
                generation: 3,
                position_ms: 9,
            },
        );
        assert!(erx.try_recv().is_err());
        h.report(
            "d",
            DeviceReport::Position {
                generation: 3,
                position_ms: 1500,
            },
        );
        match erx.try_recv().unwrap() {
            EngineEvent::Playing {
                generation,
                position,
                ..
            } => {
                assert_eq!(generation, 3);
                assert_eq!(position, Duration::from_millis(1500));
            }
            other => panic!("unexpected {other:?}"),
        }
        h.report(
            "d",
            DeviceReport::Failed {
                generation: 3,
                reason: "bad".into(),
            },
        );
        assert!(matches!(
            erx.try_recv().unwrap(),
            EngineEvent::TrackFailed { generation: 3, .. }
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn next_sends_ended_once_and_takes_next_stream() {
        let h = hub();
        let (etx, mut erx) = mpsc::unbounded_channel();
        let token = h.attach("d", etx);
        h.set_stream(token, stream(3));
        let h2 = h.clone();
        let task = tokio::spawn(async move { h2.next("d", 3).await });
        settle().await;
        assert!(matches!(
            erx.try_recv().unwrap(),
            EngineEvent::TrackEnded { generation: 3 }
        ));
        assert!(erx.try_recv().is_err());
        assert!(h.set_stream(token, stream(4)));
        assert_eq!(task.await.unwrap().unwrap().generation, 4);
        // Already newer: returned at once.
        assert_eq!(h.next("d", 3).await.unwrap().generation, 4);
    }

    #[tokio::test(start_paused = true)]
    async fn next_waiter_is_dropped_when_a_newer_stream_is_set_unchained() {
        let h = hub();
        let token = h.attach("d", mpsc::unbounded_channel().0);
        h.set_stream(token, stream(3));
        let h2 = h.clone();
        let task = tokio::spawn(async move { h2.next("d", 3).await });
        settle().await;
        h.clear_stream(token);
        assert!(task.await.unwrap().is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn next_times_out() {
        let h = hub();
        let token = h.attach("d", mpsc::unbounded_channel().0);
        h.set_stream(token, stream(3));
        let h2 = h.clone();
        let task = tokio::spawn(async move { h2.next("d", 3).await });
        settle().await;
        pass(NEXT_WAIT + Duration::from_secs(1)).await;
        assert!(task.await.unwrap().is_none());
        assert!(!h.set_stream(token, stream(4)));
    }

    #[tokio::test(start_paused = true)]
    async fn target_disconnect_pauses_after_grace() {
        let (h, _token, _erx) = playing().await;
        let (g, _r) = h.connect(hello("d", None));
        drop(g);
        pass(GRACE / 2).await;
        assert!(!paused(&h));
        pass(GRACE).await;
        assert!(paused(&h));
    }

    #[tokio::test(start_paused = true)]
    async fn reconnect_within_grace_does_not_pause() {
        let (h, _token, _erx) = playing().await;
        let (g, _r) = h.connect(hello("d", None));
        drop(g);
        pass(GRACE / 2).await;
        let (_g2, _r2) = h.connect(hello("d", None));
        pass(GRACE * 2).await;
        assert!(!paused(&h));
    }

    #[tokio::test(start_paused = true)]
    async fn stream_request_within_grace_does_not_pause() {
        let (h, _token, _erx) = playing().await;
        let (g, _r) = h.connect(hello("d", None));
        drop(g);
        pass(GRACE / 2).await;
        assert!(h.stream("d", 3).is_some());
        pass(GRACE * 2).await;
        assert!(!paused(&h));
    }

    #[tokio::test(start_paused = true)]
    async fn closing_non_target_session_does_not_pause() {
        let (h, _token, _erx) = playing().await;
        let (g1, _r1) = h.connect(hello("d", None));
        let (_g2, _r2) = h.connect(hello("d", None));
        drop(g1);
        pass(GRACE * 2).await;
        assert!(!paused(&h));
    }

    #[tokio::test(start_paused = true)]
    async fn hello_after_hub_pause_resumes_when_tab_still_playing() {
        let (h, _token, _erx) = playing().await;
        let (g, _r) = h.connect(hello("d", None));
        drop(g);
        pass(GRACE * 2).await;
        assert!(paused(&h));
        let loaded = Loaded {
            generation: Some(3),
            position_ms: 1000,
            playing: true,
        };
        let (_g2, mut r2) = h.connect(hello("d", Some(loaded)));
        settle().await;
        assert!(!paused(&h));
        assert!(matches!(
            r2.try_recv().unwrap(),
            DeviceCommand::Load {
                generation: 3,
                chained: true,
                paused: false,
                ..
            }
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn hello_while_pinson_paused_tells_playing_tab_to_pause() {
        let (h, _token, _erx) = playing().await;
        let (g, _r) = h.connect(hello("d", None));
        h.player.pause().await.unwrap();
        drop(g);
        pass(GRACE * 2).await;
        let loaded = Loaded {
            generation: Some(3),
            position_ms: 1000,
            playing: true,
        };
        let (_g2, mut r2) = h.connect(hello("d", Some(loaded)));
        settle().await;
        assert!(paused(&h));
        assert!(matches!(
            r2.try_recv().unwrap(),
            DeviceCommand::Load {
                chained: true,
                paused: true,
                ..
            }
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn hello_without_current_stream_loads_paused_at_position() {
        let (h, _token, _erx) = playing().await;
        let (g, _r) = h.connect(hello("d", None));
        drop(g);
        pass(GRACE * 2).await;
        let (_g2, mut r2) = h.connect(hello("d", None));
        match r2.try_recv().unwrap() {
            DeviceCommand::Load {
                url,
                paused,
                chained,
                ..
            } => {
                assert!(paused);
                assert!(!chained);
                assert!(url.contains("fromMs="), "{url}");
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(paused(&h));
    }

    #[test]
    fn stream_url_encodes_the_device() {
        assert_eq!(
            stream_url("a b/c", 7, 1200),
            "/api/device/stream/7?device=a%20b%2Fc&fromMs=1200"
        );
    }
}
