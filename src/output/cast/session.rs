//! A session with one Cast device: connects, launches the Default Media Receiver, loads
//! Pinson's stream and turns media status into `EngineEvent`s.
//!
//! One task owns all state (`Core`) and runs a `select!` loop over commands, inbound
//! frames, the heartbeat and poll ticks, the silence deadline and (re)connect attempts.
//! Reading and writing happen in small tasks per connection so the loop never waits on
//! the socket. Task 4 extends `Core` (lost-link rejoin, takeover, volume).

use std::io;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use tokio::io::{AsyncWriteExt, split};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::{Instant, MissedTickBehavior, interval_at, sleep_until, timeout};

use super::proto::*;
use super::tls::CastLink;
use crate::engine::EngineEvent;

const BACKOFF_MIN: Duration = Duration::from_millis(500);
const BACKOFF_MAX: Duration = Duration::from_secs(5);
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);
/// After `stop` with no new LOAD for this long, the receiver app is stopped too, so the
/// device goes back to idle when the queue ends. Seeks and skips load again well before.
const IDLE_GRACE: Duration = Duration::from_secs(20);
/// A request with no reply after this long no longer counts as awaiting one.
const PENDING_TTL: Duration = Duration::from_secs(5);
/// At most this many request ids are remembered per list.
const MAX_TRACKED: usize = 32;

/// Opens connections to the device; called again after every drop.
pub trait CastConnector: Send + Sync + 'static {
    /// Shown in logs, e.g. `192.168.1.50:8009`.
    fn target(&self) -> String;
    fn connect(&self) -> BoxFuture<'static, io::Result<CastLink>>;
}

#[derive(Clone, Debug, PartialEq)]
pub struct CastMeta {
    pub title: String,
    pub artist: String,
    pub album: Option<String>,
    pub image: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LoadReq {
    pub generation: u64,
    /// Like `/api/cast/stream/3`.
    pub path: String,
    pub from: Duration,
    pub meta: Option<CastMeta>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Volume {
    pub level: f32,
    pub muted: bool,
}

/// Something that happened on the device which the player should mirror.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum External {
    Pause,
    Resume,
}

pub struct SessionConfig {
    pub connector: Arc<dyn CastConnector>,
    /// `MUSIC_PUBLIC_URL`; overrides the address the device is told to fetch from.
    pub public_url: Option<String>,
    pub port: u16,
    pub heartbeat: Duration,
    pub silence: Duration,
    pub poll: Duration,
}

impl SessionConfig {
    pub fn new(connector: Arc<dyn CastConnector>, public_url: Option<String>, port: u16) -> Self {
        Self {
            connector,
            public_url,
            port,
            heartbeat: Duration::from_secs(5),
            silence: Duration::from_secs(15),
            poll: Duration::from_secs(5),
        }
    }
}

enum Cmd {
    Load(LoadReq),
    Pause,
    Resume,
    Stop,
    Volume {
        level: Option<f32>,
        muted: Option<bool>,
        reply: oneshot::Sender<Result<(), String>>,
    },
}

const UNREACHABLE: &str = "Can't reach the Chromecast";
const VOLUME_TIMEOUT: Duration = Duration::from_secs(2);

/// A `set_volume` waiting for the device's answers.
struct VolumeReq {
    rids: Vec<u64>,
    reply: oneshot::Sender<Result<(), String>>,
}

/// A handle to the session task. Clones share the task.
#[derive(Clone)]
pub struct CastSession {
    cmds: mpsc::UnboundedSender<Cmd>,
    link: watch::Receiver<Option<bool>>,
    volume: watch::Receiver<Option<Volume>>,
    closed: watch::Receiver<bool>,
}

/// Dropping it stops the app on the device and ends the task, whatever handles remain.
pub struct SessionGuard {
    _shutdown: oneshot::Sender<()>,
}

impl CastSession {
    pub fn load(&self, req: LoadReq) {
        self.cmds.send(Cmd::Load(req)).ok();
    }
    pub fn pause(&self) {
        self.cmds.send(Cmd::Pause).ok();
    }
    pub fn resume(&self) {
        self.cmds.send(Cmd::Resume).ok();
    }
    /// Stops the media and keeps the receiver app for a quick next LOAD (a seek or a
    /// skip); the app is stopped after `IDLE_GRACE` with no LOAD.
    pub fn stop(&self) {
        self.cmds.send(Cmd::Stop).ok();
    }
    /// Sets the device volume and/or mute; resolves when the device has answered.
    pub async fn set_volume(&self, level: Option<f32>, muted: Option<bool>) -> Result<(), String> {
        if *self.link.borrow() != Some(true) {
            return Err(UNREACHABLE.into());
        }
        let (reply, rx) = oneshot::channel();
        self.cmds
            .send(Cmd::Volume {
                level,
                muted,
                reply,
            })
            .map_err(|_| UNREACHABLE.to_string())?;
        match timeout(VOLUME_TIMEOUT, rx).await {
            Ok(Ok(res)) => res,
            _ => Err(UNREACHABLE.into()),
        }
    }
    /// `None` until the first attempt ends, then whether the device is reachable.
    pub fn link(&self) -> watch::Receiver<Option<bool>> {
        self.link.clone()
    }
    pub fn volume(&self) -> watch::Receiver<Option<Volume>> {
        self.volume.clone()
    }
    /// Becomes `true` once the task has ended, after its STOPs were flushed. The sender
    /// is also gone then, so `wait_for` ends either way.
    pub fn closed(&self) -> watch::Receiver<bool> {
        self.closed.clone()
    }
}

pub fn spawn(
    cfg: SessionConfig,
    events: mpsc::UnboundedSender<EngineEvent>,
    external: Arc<dyn Fn(External) + Send + Sync>,
) -> (CastSession, SessionGuard) {
    let (cmds, cmd_rx) = mpsc::unbounded_channel();
    let (link_tx, link) = watch::channel(None);
    let (volume_tx, volume) = watch::channel(None);
    let (closed_tx, closed) = watch::channel(false);
    let (shutdown, shutdown_rx) = oneshot::channel();
    let core = Core {
        cfg,
        events,
        external,
        link_tx,
        volume_tx,
        closed_tx,
        next_id: 1,
        conn: None,
        inbound: None,
        last_inbound: Instant::now(),
        retry_at: None,
        fresh: false,
        backoff: BACKOFF_MIN,
        idle_at: None,
        app: None,
        launch_req: None,
        current: None,
        retired: Vec::new(),
        wanted_paused: false,
        stopped: Vec::new(),
        rejoin: None,
        awaiting_status: false,
        rejoin_req: None,
        pending: Vec::new(),
        polls: Vec::new(),
        orphan_loads: Vec::new(),
        volume_reqs: Vec::new(),
    };
    tokio::spawn(core.run(cmd_rx, shutdown_rx));
    (
        CastSession {
            cmds,
            link,
            volume,
            closed,
        },
        SessionGuard {
            _shutdown: shutdown,
        },
    )
}

enum Inbound {
    Msg(CastMessage),
    Closed,
}

/// An open connection: a writer task fed by `out`, a reader task feeding `Core::inbound`.
struct Conn {
    out: mpsc::UnboundedSender<CastMessage>,
    writer: JoinHandle<()>,
    reader: JoinHandle<()>,
    local_ip: IpAddr,
}

impl Conn {
    fn kill(self) {
        self.writer.abort();
        self.reader.abort();
    }
}

/// The app running on the device.
struct App {
    session_id: String,
    transport_id: String,
    /// We sent CONNECT to `transport_id`.
    connected: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum Player {
    Unknown,
    Buffering,
    Playing,
    Paused,
    Idle,
}

/// The song we want on the device.
struct Current {
    req: LoadReq,
    /// Song position the last LOAD started at; status positions are relative to it.
    base: Duration,
    /// Last reported song position (starts at `req.from`).
    last_pos: Duration,
    /// This generation has reached PLAYING at least once.
    played: bool,
    /// It has been reloaded after an error already.
    reloaded: bool,
    /// Ended (finished or failed): no more events, no reload on resume.
    ended: bool,
    /// The device couldn't play it; `resume` sends the LOAD again.
    held: bool,
    /// A LOAD is out (or answered) for the current connection.
    sent: bool,
    /// Request id of the outstanding LOAD.
    load_req: Option<u64>,
    /// The `mediaSessionId` bound to this generation.
    bound: Option<i64>,
    player: Player,
}

impl Current {
    fn new(req: LoadReq) -> Self {
        Self {
            base: req.from,
            last_pos: req.from,
            req,
            played: false,
            reloaded: false,
            ended: false,
            held: false,
            sent: false,
            load_req: None,
            bound: None,
            player: Player::Unknown,
        }
    }
}

enum Ev {
    Shutdown,
    Cmd(Option<Cmd>),
    Connected(io::Result<CastLink>),
    Retry,
    Inbound(Option<Inbound>),
    Heartbeat,
    Poll,
    Silence,
    Idle,
}

struct Core {
    cfg: SessionConfig,
    events: mpsc::UnboundedSender<EngineEvent>,
    external: Arc<dyn Fn(External) + Send + Sync>,
    link_tx: watch::Sender<Option<bool>>,
    volume_tx: watch::Sender<Option<Volume>>,
    closed_tx: watch::Sender<bool>,
    next_id: u64,
    conn: Option<Conn>,
    inbound: Option<mpsc::UnboundedReceiver<Inbound>>,
    last_inbound: Instant,
    /// When to try connecting again; `None` while connected or attempting.
    retry_at: Option<Instant>,
    /// The connection is up but nothing has come from the device yet.
    fresh: bool,
    backoff: Duration,
    /// When to stop the idle receiver app (armed by `Cmd::Stop`).
    idle_at: Option<Instant>,
    app: Option<App>,
    /// Request id of the outstanding LAUNCH.
    launch_req: Option<u64>,
    current: Option<Current>,
    /// Media sessions of earlier loads: their statuses are stale.
    retired: Vec<i64>,
    wanted_paused: bool,
    /// Sessions we stopped: a late status that still lists them must not be adopted.
    stopped: Vec<String>,
    /// After a lost link: our session and the media session in it, to rejoin.
    rejoin: Option<(String, Option<i64>)>,
    /// Reconnected with a `rejoin`: waiting for the first RECEIVER_STATUS.
    awaiting_status: bool,
    /// Request id of the media GET_STATUS that checks a rejoined media session.
    rejoin_req: Option<u64>,
    /// Control requests of ours (PAUSE, PLAY, LOAD, STOP) whose reply has not come,
    /// with when they were sent: while one is out, status changes are not external.
    pending: Vec<(u64, Instant)>,
    /// Media GET_STATUS requests of ours whose reply has not come.
    polls: Vec<(u64, Instant)>,
    /// LOADs superseded before they were answered: the media sessions their answers name
    /// are retired, and until then a `requestId` 0 status binds nothing.
    orphan_loads: Vec<u64>,
    volume_reqs: Vec<VolumeReq>,
}

impl Core {
    async fn run(mut self, cmd_rx: mpsc::UnboundedReceiver<Cmd>, shutdown: oneshot::Receiver<()>) {
        let mut cmd_rx = cmd_rx;
        let mut shutdown = shutdown;
        let mut cmds_open = true;
        let (hb, poll) = (self.cfg.heartbeat, self.cfg.poll);
        let mut hb_tick = interval_at(Instant::now() + hb, hb);
        hb_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut poll_tick = interval_at(Instant::now() + poll, poll);
        poll_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut connecting: Option<BoxFuture<'static, io::Result<CastLink>>> =
            Some(self.cfg.connector.connect());
        loop {
            let connected = self.conn.is_some();
            let retry_at = self.retry_at;
            let idle_at = self.idle_at;
            let deadline = self.last_inbound + self.cfg.silence;
            let ev = tokio::select! {
                _ = &mut shutdown => Ev::Shutdown,
                cmd = cmd_rx.recv(), if cmds_open => Ev::Cmd(cmd),
                res = async { connecting.as_mut().unwrap().await }, if connecting.is_some() => Ev::Connected(res),
                _ = async { sleep_until(retry_at.unwrap()).await }, if retry_at.is_some() => Ev::Retry,
                msg = async { self.inbound.as_mut().unwrap().recv().await }, if connected => Ev::Inbound(msg),
                _ = hb_tick.tick(), if connected => Ev::Heartbeat,
                _ = poll_tick.tick(), if connected => Ev::Poll,
                _ = sleep_until(deadline), if connected => Ev::Silence,
                _ = async { sleep_until(idle_at.unwrap()).await }, if idle_at.is_some() => Ev::Idle,
            };
            match ev {
                Ev::Shutdown => break,
                Ev::Cmd(None) => cmds_open = false,
                Ev::Cmd(Some(cmd)) => {
                    self.on_cmd(cmd);
                    // Commands sent together (a paused seek's load then pause) all apply
                    // before the LOAD goes out, so it carries the right `autoplay`.
                    while let Ok(cmd) = cmd_rx.try_recv() {
                        self.on_cmd(cmd);
                    }
                }
                Ev::Connected(res) => {
                    connecting = None;
                    match res {
                        Ok(link) => self.on_connected(link),
                        Err(e) => {
                            tracing::warn!(
                                "Cast {}: connect failed: {e}",
                                self.cfg.connector.target()
                            );
                            self.link_tx.send_replace(Some(false));
                            self.schedule_retry();
                        }
                    }
                }
                Ev::Retry => {
                    self.retry_at = None;
                    connecting = Some(self.cfg.connector.connect());
                }
                Ev::Inbound(Some(Inbound::Msg(msg))) => {
                    self.last_inbound = Instant::now();
                    if std::mem::take(&mut self.fresh) {
                        // The device talks: only now is the link up and the backoff reset.
                        self.backoff = BACKOFF_MIN;
                        self.link_tx.send_replace(Some(true));
                    }
                    self.on_message(msg);
                }
                Ev::Inbound(_) => self.on_lost("connection closed"),
                Ev::Heartbeat => self.send(PLATFORM_ID, NS_HEARTBEAT, json!({"type": "PING"})),
                Ev::Poll => self.poll(),
                Ev::Silence => self.on_lost("no message from the device"),
                Ev::Idle => self.on_idle(),
            }
            self.ensure_load();
        }
        self.shutdown().await;
        self.closed_tx.send_replace(true);
    }

    // --- connection -------------------------------------------------------------------

    fn on_connected(&mut self, link: CastLink) {
        let (mut rd, mut wr) = split(link.io);
        let (in_tx, in_rx) = mpsc::unbounded_channel();
        let (out, mut out_rx) = mpsc::unbounded_channel::<CastMessage>();
        let reader_tx = in_tx.clone();
        let reader = tokio::spawn(async move {
            loop {
                match read_frame(&mut rd).await {
                    Ok(msg) => {
                        if reader_tx.send(Inbound::Msg(msg)).is_err() {
                            break;
                        }
                    }
                    Err(_) => {
                        reader_tx.send(Inbound::Closed).ok();
                        break;
                    }
                }
            }
        });
        let writer = tokio::spawn(async move {
            while let Some(msg) = out_rx.recv().await {
                if !matches!(
                    timeout(WRITE_TIMEOUT, write_frame(&mut wr, &msg)).await,
                    Ok(Ok(()))
                ) {
                    in_tx.send(Inbound::Closed).ok();
                    return;
                }
            }
            wr.shutdown().await.ok();
        });
        self.conn = Some(Conn {
            out,
            writer,
            reader,
            local_ip: link.local_ip,
        });
        self.inbound = Some(in_rx);
        self.last_inbound = Instant::now();
        self.fresh = true;
        self.awaiting_status = self.rejoin.is_some();
        self.send(PLATFORM_ID, NS_CONNECTION, json!({"type": "CONNECT"}));
        let id = self.req_id();
        self.send(
            PLATFORM_ID,
            NS_RECEIVER,
            json!({"type": "GET_STATUS", "requestId": id}),
        );
        self.ensure_load();
    }

    fn schedule_retry(&mut self) {
        self.retry_at = Some(Instant::now() + self.backoff);
        self.backoff = (self.backoff * 2).min(BACKOFF_MAX);
    }

    /// The link is gone. Keeps the wanted song; it is sent again after reconnecting.
    fn on_lost(&mut self, why: &str) {
        tracing::warn!("Cast {}: link lost ({why})", self.cfg.connector.target());
        if let Some(conn) = self.conn.take() {
            conn.kill();
        }
        self.inbound = None;
        self.fresh = false;
        self.launch_req = None;
        self.rejoin_req = None;
        self.pending.clear();
        self.polls.clear();
        self.orphan_loads.clear();
        self.volume_reqs.clear();
        // Keep the session and media session so a reconnect can rejoin them. Lost again
        // before the device said anything: the earlier rejoin still stands.
        let session = self.app.take().map(|a| a.session_id);
        let media = self.bound();
        self.rejoin = match (session, self.current.is_some()) {
            (Some(s), true) => Some((s, media)),
            (None, true) => self.rejoin.take(),
            _ => None,
        };
        if self.rejoin.is_none() {
            self.retired.clear();
        }
        let rejoin_media = self.rejoin.as_ref().and_then(|r| r.1);
        if let Some(cur) = self.current.as_mut() {
            cur.load_req = None;
            cur.bound = None;
            cur.player = Player::Unknown;
            if rejoin_media.is_none() {
                cur.sent = false;
            }
        }
        self.link_tx.send_replace(Some(false));
        self.schedule_retry();
    }

    /// Stops the app (if any) and closes the connection, giving the writer a moment to flush.
    async fn shutdown(&mut self) {
        self.stop_remote();
        if let Some(conn) = self.conn.take() {
            let Conn {
                out,
                writer,
                reader,
                ..
            } = conn;
            drop(out);
            timeout(CLOSE_TIMEOUT, writer).await.ok();
            reader.abort();
        }
    }

    // --- sending ----------------------------------------------------------------------

    fn req_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn send(&self, destination: &str, namespace: &str, payload: Value) {
        if let Some(conn) = &self.conn {
            conn.out
                .send(json_msg(destination, namespace, &payload))
                .ok();
        }
    }

    /// A media request (`PAUSE`, `PLAY`, `STOP`, `GET_STATUS`) for `media_session_id`.
    fn send_media(&mut self, ty: &str, media_session_id: i64) {
        let id = self.req_id();
        if let Some(transport) = self.app.as_ref().map(|a| a.transport_id.clone()) {
            self.send(
                &transport,
                NS_MEDIA,
                json!({"type": ty, "mediaSessionId": media_session_id, "requestId": id}),
            );
            if ty == "GET_STATUS" {
                track(&mut self.polls, id);
            } else {
                track(&mut self.pending, id);
            }
        }
    }

    /// Forgets requests whose reply is overdue.
    fn prune(&mut self) {
        let now = Instant::now();
        self.pending.retain(|(_, at)| now - *at < PENDING_TTL);
        self.polls.retain(|(_, at)| now - *at < PENDING_TTL);
    }

    /// Remembers the current song's LOAD as an orphan if it is out and unanswered.
    fn orphan_current_load(&mut self) {
        if let Some(rid) = self
            .current
            .as_ref()
            .filter(|c| c.bound.is_none())
            .and_then(|c| c.load_req)
        {
            self.orphan_loads.push(rid);
            if self.orphan_loads.len() > MAX_TRACKED {
                self.orphan_loads.remove(0);
            }
        }
    }

    fn base_url(&self) -> String {
        match (&self.cfg.public_url, &self.conn) {
            (Some(url), _) => url.trim_end_matches('/').to_owned(),
            (None, Some(c)) => match c.local_ip {
                IpAddr::V4(ip) => format!("http://{ip}:{}", self.cfg.port),
                IpAddr::V6(ip) => format!("http://[{ip}]:{}", self.cfg.port),
            },
            (None, None) => format!("http://localhost:{}", self.cfg.port),
        }
    }

    fn stream_url(&self, req: &LoadReq, from: Duration) -> String {
        format!(
            "{}{}?fromMs={}",
            self.base_url(),
            req.path,
            from.as_millis()
        )
    }

    /// Launches the app if needed and sends the wanted song's LOAD, when it is time to.
    fn ensure_load(&mut self) {
        let Some(cur) = &self.current else { return };
        if cur.held || cur.sent || cur.ended || self.conn.is_none() || self.awaiting_status {
            return;
        }
        let Some(app) = self.app.as_mut() else {
            if self.launch_req.is_none() {
                let id = self.req_id();
                self.launch_req = Some(id);
                self.send(
                    PLATFORM_ID,
                    NS_RECEIVER,
                    json!({"type": "LAUNCH", "appId": APP_ID, "requestId": id}),
                );
            }
            return;
        };
        if !app.connected {
            app.connected = true;
            let transport = app.transport_id.clone();
            self.send(&transport, NS_CONNECTION, json!({"type": "CONNECT"}));
        }
        self.send_load();
    }

    fn send_load(&mut self) {
        let id = self.req_id();
        let Some(cur) = self.current.as_mut() else {
            return;
        };
        cur.base = cur.last_pos;
        cur.sent = true;
        cur.load_req = Some(id);
        cur.bound = None;
        cur.player = Player::Unknown;
        let (req, base) = (cur.req.clone(), cur.base);
        let mut media = json!({
            "contentId": self.stream_url(&req, base),
            "contentType": "audio/mpeg",
            "streamType": "BUFFERED",
        });
        if let Some(meta) = &req.meta {
            let mut md = json!({"metadataType": 3, "title": meta.title, "artist": meta.artist});
            if let Some(album) = &meta.album {
                md["albumName"] = json!(album);
            }
            if let Some(image) = &meta.image {
                md["images"] = json!([{"url": image}]);
            }
            media["metadata"] = md;
        }
        let Some((transport, session_id)) = self
            .app
            .as_ref()
            .map(|a| (a.transport_id.clone(), a.session_id.clone()))
        else {
            return;
        };
        track(&mut self.pending, id);
        self.send(
            &transport,
            NS_MEDIA,
            json!({
                "type": "LOAD",
                "requestId": id,
                "sessionId": session_id,
                "autoplay": !self.wanted_paused,
                "currentTime": 0,
                "media": media,
            }),
        );
    }

    fn poll(&mut self) {
        let playing = self
            .current
            .as_ref()
            .and_then(|c| (c.player == Player::Playing).then_some(c.bound).flatten());
        if let Some(m) = playing {
            self.send_media("GET_STATUS", m);
        }
    }

    // --- commands ---------------------------------------------------------------------

    fn bound(&self) -> Option<i64> {
        self.current.as_ref().and_then(|c| c.bound)
    }

    fn on_cmd(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Load(req) => {
                // Sent at the end of this loop turn, after any queued pause.
                self.idle_at = None;
                self.orphan_current_load();
                self.retire_bound();
                if let Some(r) = self.rejoin.as_mut() {
                    r.1 = None;
                }
                self.current = Some(Current::new(req));
                self.wanted_paused = false;
            }
            Cmd::Pause => {
                self.wanted_paused = true;
                if let Some(m) = self.bound() {
                    self.send_media("PAUSE", m);
                }
            }
            Cmd::Resume => {
                self.wanted_paused = false;
                // A held or unsent load goes out at the end of this loop turn.
                if let Some(cur) = self.current.as_mut().filter(|c| c.held) {
                    cur.held = false;
                } else if let Some(m) = self.bound() {
                    self.send_media("PLAY", m);
                }
            }
            Cmd::Stop => {
                // The media only: a seek or skip loads again in a moment.
                if let Some(m) = self.bound() {
                    self.send_media("STOP", m);
                }
                self.orphan_current_load();
                self.retire_bound();
                self.current = None;
                if let Some(r) = self.rejoin.as_mut() {
                    r.1 = None;
                }
                self.idle_at = Some(Instant::now() + IDLE_GRACE);
            }
            Cmd::Volume {
                level,
                muted,
                reply,
            } => self.set_volume(level, muted, reply),
        }
    }

    fn set_volume(
        &mut self,
        level: Option<f32>,
        muted: Option<bool>,
        reply: oneshot::Sender<Result<(), String>>,
    ) {
        if self.conn.is_none() {
            reply.send(Err(UNREACHABLE.into())).ok();
            return;
        }
        let mut volumes = Vec::new();
        if let Some(l) = level {
            volumes.push(json!({"level": l.clamp(0.0, 1.0)}));
        }
        if let Some(m) = muted {
            volumes.push(json!({"muted": m}));
        }
        let mut rids = Vec::new();
        for volume in volumes {
            let id = self.req_id();
            rids.push(id);
            self.send(
                PLATFORM_ID,
                NS_RECEIVER,
                json!({"type": "SET_VOLUME", "volume": volume, "requestId": id}),
            );
        }
        if rids.is_empty() {
            reply.send(Ok(())).ok();
        } else {
            self.volume_reqs.push(VolumeReq { rids, reply });
        }
    }

    /// Sends media STOP and receiver STOP for what we have running.
    fn stop_remote(&mut self) {
        if let Some(m) = self.bound() {
            self.send_media("STOP", m);
        }
        if let Some(session_id) = self.app.as_ref().map(|a| a.session_id.clone()) {
            self.stopped.push(session_id.clone());
            let id = self.req_id();
            self.send(
                PLATFORM_ID,
                NS_RECEIVER,
                json!({"type": "STOP", "sessionId": session_id, "requestId": id}),
            );
        }
    }

    fn retire_bound(&mut self) {
        if let Some(m) = self.current.as_mut().and_then(|c| c.bound.take()) {
            self.retire(m);
        }
    }

    /// Marks media session `m` stale. The app now lives across songs, so only the most
    /// recent ones are kept: older sessions have long gone quiet.
    fn retire(&mut self, m: i64) {
        if !self.retired.contains(&m) {
            self.retired.push(m);
            if self.retired.len() > MAX_TRACKED {
                self.retired.remove(0);
            }
        }
    }

    /// No LOAD since `stop` for `IDLE_GRACE`: stop the app so the device goes idle.
    fn on_idle(&mut self) {
        self.idle_at = None;
        if self.current.is_some() {
            return;
        }
        self.stop_remote();
        self.app = None;
        self.launch_req = None;
        self.rejoin = None;
        self.retired.clear();
        self.orphan_loads.clear();
    }

    // --- inbound ----------------------------------------------------------------------

    fn on_message(&mut self, msg: CastMessage) {
        let Ok(v) = serde_json::from_str::<Value>(&msg.payload) else {
            return;
        };
        let ty = v["type"].as_str().unwrap_or("");
        match msg.namespace.as_str() {
            NS_HEARTBEAT if ty == "PING" => {
                self.send(&msg.source, NS_HEARTBEAT, json!({"type": "PONG"}))
            }
            NS_RECEIVER => self.on_receiver(ty, &v),
            NS_MEDIA => self.on_media(ty, &v),
            _ => {}
        }
    }

    fn on_receiver(&mut self, ty: &str, v: &Value) {
        let rid = v["requestId"].as_u64();
        match ty {
            "RECEIVER_STATUS" => {
                if let Some(rid) = rid {
                    let reqs = std::mem::take(&mut self.volume_reqs);
                    for mut req in reqs {
                        req.rids.retain(|r| *r != rid);
                        if req.rids.is_empty() {
                            req.reply.send(Ok(())).ok();
                        } else {
                            self.volume_reqs.push(req);
                        }
                    }
                }
                let status = &v["status"];
                if let (Some(level), Some(muted)) = (
                    status["volume"]["level"].as_f64(),
                    status["volume"]["muted"].as_bool(),
                ) {
                    let vol = Volume {
                        level: level as f32,
                        muted,
                    };
                    self.volume_tx.send_if_modified(|cur| {
                        let changed = *cur != Some(vol);
                        *cur = Some(vol);
                        changed
                    });
                }
                self.on_apps(status["applications"].as_array(), rid.unwrap_or(0));
            }
            "LAUNCH_ERROR" => {
                tracing::warn!("Cast launch failed: {}", v["reason"]);
                self.launch_req = None;
                self.hold();
            }
            "INVALID_REQUEST" if rid.is_some() && rid == self.launch_req => {
                tracing::warn!("Cast launch rejected: {}", v["reason"]);
                self.launch_req = None;
                self.hold();
            }
            _ => {}
        }
    }

    /// `rid` is 0 for a status nobody asked for.
    fn on_apps(&mut self, apps: Option<&Vec<Value>>, rid: u64) {
        let ours = apps
            .into_iter()
            .flatten()
            .find(|a| a["appId"] == APP_ID)
            .and_then(|a| Some((a["sessionId"].as_str()?, a["transportId"].as_str()?)))
            .filter(|(sid, _)| !self.stopped.iter().any(|s| s == sid));
        let waiting = std::mem::take(&mut self.awaiting_status);
        let rejoin = if waiting { self.rejoin.take() } else { None };
        let Some((session_id, transport_id)) = ours else {
            if waiting {
                // Our app is gone: relaunch and load again.
                self.retired.clear();
                if let Some(cur) = self.current.as_mut() {
                    cur.sent = false;
                    cur.bound = None;
                }
                self.ensure_load();
            } else if rid == 0 && self.launch_req.is_none() && self.app.is_some() {
                self.taken_over();
            }
            return;
        };
        if let Some((sid, media)) = &rejoin
            && sid == session_id
        {
            self.app = Some(App {
                session_id: session_id.to_owned(),
                transport_id: transport_id.to_owned(),
                connected: true,
            });
            self.send(transport_id, NS_CONNECTION, json!({"type": "CONNECT"}));
            let media = media.filter(|_| self.current.as_ref().is_some_and(|c| c.sent));
            if let Some(m) = media {
                if let Some(cur) = self.current.as_mut() {
                    cur.bound = Some(m);
                }
                self.send_media("GET_STATUS", m);
                self.rejoin_req = Some(self.next_id - 1);
            }
            self.ensure_load();
            return;
        }
        // Only adopt the app when we launched it or want to play: after a stop
        // the device may still report the dying app.
        if self.launch_req.is_none() && self.current.is_none() {
            return;
        }
        self.launch_req = None;
        if self.app.as_ref().is_none_or(|a| a.session_id != session_id) {
            self.retired.clear();
            self.orphan_loads.clear();
            self.retire_bound();
            if let Some(cur) = self.current.as_mut() {
                cur.sent = false;
            }
            self.app = Some(App {
                session_id: session_id.to_owned(),
                transport_id: transport_id.to_owned(),
                connected: false,
            });
        }
        self.ensure_load();
    }

    /// Someone else is using the device: forget our app, hold the song, pause Pinson.
    fn taken_over(&mut self) {
        if let Some(app) = self.app.take() {
            self.stopped.push(app.session_id);
        }
        self.orphan_current_load();
        self.retire_bound();
        let mut held = false;
        if let Some(cur) = self.current.as_mut().filter(|c| !c.ended) {
            cur.held = true;
            cur.sent = false;
            cur.load_req = None;
            held = true;
        }
        if held && !self.wanted_paused {
            tracing::info!(
                "Cast {}: another app took over",
                self.cfg.connector.target()
            );
            self.wanted_paused = true;
            (self.external)(External::Pause);
        }
    }

    fn on_media(&mut self, ty: &str, v: &Value) {
        let rid = v["requestId"].as_u64().unwrap_or(0);
        self.prune();
        self.pending.retain(|p| p.0 != rid);
        let poll_reply = rid != 0 && self.polls.iter().any(|p| p.0 == rid);
        self.polls.retain(|p| p.0 != rid);
        let orphan = rid != 0 && self.orphan_loads.contains(&rid);
        self.orphan_loads.retain(|r| *r != rid);
        match ty {
            "MEDIA_STATUS" => {
                let entries = v["status"].as_array().map_or(&[][..], |a| &a[..]);
                if orphan {
                    // The answer to a superseded LOAD: its media session is stale.
                    let bound = self.bound();
                    for m in entries.iter().filter_map(|e| e["mediaSessionId"].as_i64()) {
                        if Some(m) != bound {
                            self.retire(m);
                        }
                    }
                }
                // Device-side changes are followed only while no control request of
                // ours is awaiting its reply: unsolicited statuses and poll replies.
                let follow = (rid == 0 || poll_reply) && self.pending.is_empty();
                for entry in entries {
                    self.on_entry(rid, follow, entry);
                }
                if rid != 0 && self.rejoin_req == Some(rid) {
                    self.rejoin_req = None;
                    let bound = self.bound();
                    if bound.is_some_and(|b| !entries.iter().any(|e| e["mediaSessionId"] == b)) {
                        // The media session did not survive: load the song again.
                        if let Some(cur) = self.current.as_mut() {
                            cur.bound = None;
                            cur.sent = false;
                        }
                        self.ensure_load();
                    }
                }
            }
            "LOAD_FAILED" | "INVALID_REQUEST"
                if rid != 0
                    && self
                        .current
                        .as_ref()
                        .is_some_and(|c| c.load_req == Some(rid)) =>
            {
                if ty == "INVALID_REQUEST" {
                    tracing::warn!("Cast device rejected the load: {}", v["reason"]);
                }
                self.play_problem();
            }
            _ => {}
        }
    }

    /// `follow`: a state that contradicts the wanted one was changed on the device.
    fn on_entry(&mut self, rid: u64, follow: bool, e: &Value) {
        let Some(m) = e["mediaSessionId"].as_i64() else {
            return;
        };
        let retired = self.retired.contains(&m);
        // While a superseded LOAD is unanswered, a broadcast may be about its session.
        let broadcast_binds = rid == 0 && self.orphan_loads.is_empty();
        let Some(cur) = self.current.as_mut() else {
            return;
        };
        let mut just_bound = false;
        if cur.bound != Some(m) {
            let ours = cur.bound.is_none()
                && cur.sent
                && !retired
                && (broadcast_binds || (rid != 0 && Some(rid) == cur.load_req));
            if !ours {
                return;
            }
            cur.bound = Some(m);
            just_bound = true;
        }
        if cur.ended {
            return;
        }
        let current_time = e["currentTime"]
            .as_f64()
            .filter(|t| t.is_finite() && *t >= 0.0)
            .map(Duration::from_secs_f64);
        match e["playerState"].as_str() {
            Some(state @ ("PLAYING" | "PAUSED")) => {
                cur.player = if state == "PLAYING" {
                    Player::Playing
                } else {
                    Player::Paused
                };
                cur.played = true;
                // The user paused or resumed on the device: follow.
                if follow && !just_bound {
                    if state == "PAUSED" && !self.wanted_paused {
                        self.wanted_paused = true;
                        (self.external)(External::Pause);
                    } else if state == "PLAYING" && self.wanted_paused {
                        self.wanted_paused = false;
                        (self.external)(External::Resume);
                    }
                }
                cur.last_pos = current_time.map_or(cur.last_pos, |t| cur.base + t);
                self.events
                    .send(EngineEvent::Playing {
                        generation: cur.req.generation,
                        position: cur.last_pos,
                        at: Instant::now(),
                    })
                    .ok();
            }
            Some("BUFFERING") => cur.player = Player::Buffering,
            Some("IDLE") => {
                cur.player = Player::Idle;
                match e["idleReason"].as_str() {
                    Some("FINISHED") => {
                        cur.ended = true;
                        self.events
                            .send(EngineEvent::TrackEnded {
                                generation: cur.req.generation,
                            })
                            .ok();
                    }
                    Some("ERROR") => self.play_problem(),
                    _ => {}
                }
            }
            _ => {}
        }
        // A pause/resume asked for before the media session was known.
        if just_bound {
            let paused = self.current.as_ref().map(|c| c.player);
            match paused {
                Some(Player::Playing) if self.wanted_paused => self.send_media("PAUSE", m),
                Some(Player::Paused) if !self.wanted_paused => self.send_media("PLAY", m),
                _ => {}
            }
        }
    }

    /// The device couldn't play the current load (Decision 4).
    fn play_problem(&mut self) {
        let Some(cur) = self.current.as_ref().filter(|c| !c.ended) else {
            return;
        };
        if !cur.played {
            self.hold();
        } else if !cur.reloaded {
            let cur = self.current.as_mut().unwrap();
            cur.reloaded = true;
            cur.sent = false;
            cur.load_req = None;
            self.retire_bound();
            self.ensure_load();
        } else {
            let cur = self.current.as_mut().unwrap();
            cur.ended = true;
            self.events
                .send(EngineEvent::TrackFailed {
                    generation: cur.req.generation,
                    reason: "the Cast device couldn't play the stream".into(),
                })
                .ok();
        }
    }

    /// Pauses Pinson and keeps the load so `resume` sends it again.
    fn hold(&mut self) {
        let Some(cur) = self.current.as_mut().filter(|c| !c.ended) else {
            return;
        };
        cur.held = true;
        cur.sent = false;
        cur.load_req = None;
        let (req, from) = (cur.req.clone(), cur.last_pos);
        self.retire_bound();
        let url = self.stream_url(&req, from);
        tracing::warn!(
            "the Cast device couldn't play {url}: check MUSIC_PUBLIC_URL and the firewall"
        );
        (self.external)(External::Pause);
    }
}

/// Remembers request `id` as sent now, keeping at most `MAX_TRACKED`.
fn track(list: &mut Vec<(u64, Instant)>, id: u64) {
    list.push((id, Instant::now()));
    if list.len() > MAX_TRACKED {
        list.remove(0);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use serde_json::json;
    use tokio::time::{sleep, timeout};

    use super::super::fake::FakeReceiver;
    use super::*;

    struct Rig {
        fake: FakeReceiver,
        session: CastSession,
        guard: SessionGuard,
        events: mpsc::UnboundedReceiver<EngineEvent>,
        externals: Arc<Mutex<Vec<External>>>,
    }

    fn rig_with(public_url: Option<&str>) -> Rig {
        let fake = FakeReceiver::new();
        let (tx, events) = mpsc::unbounded_channel();
        let externals = Arc::new(Mutex::new(Vec::new()));
        let ext = externals.clone();
        let cfg = SessionConfig::new(fake.connector(), public_url.map(String::from), 8082);
        let (session, guard) = spawn(cfg, tx, Arc::new(move |e| ext.lock().unwrap().push(e)));
        Rig {
            fake,
            session,
            guard,
            events,
            externals,
        }
    }

    fn rig() -> Rig {
        rig_with(None)
    }

    fn req(generation: u64, from_s: u64) -> LoadReq {
        LoadReq {
            generation,
            path: format!("/api/cast/stream/{generation}"),
            from: Duration::from_secs(from_s),
            meta: Some(CastMeta {
                title: "Song".into(),
                artist: "Artist".into(),
                album: Some("Album".into()),
                image: Some("http://pinson.lan/cover.jpg".into()),
            }),
        }
    }

    async fn next_event(r: &mut Rig) -> EngineEvent {
        timeout(Duration::from_secs(30), r.events.recv())
            .await
            .expect("no event")
            .expect("events closed")
    }

    async fn assert_quiet(r: &mut Rig) {
        sleep(Duration::from_secs(2)).await;
        assert_eq!(
            r.events.try_recv().err(),
            Some(mpsc::error::TryRecvError::Empty)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn connects_and_reports_link_and_volume() {
        let r = rig();
        let mut link = r.session.link();
        assert_eq!(*link.borrow(), None);
        link.wait_for(|l| *l == Some(true)).await.unwrap();
        let mut vol = r.session.volume();
        let v = *vol.wait_for(|v| v.is_some()).await.unwrap();
        assert_eq!(
            v,
            Some(Volume {
                level: 0.5,
                muted: false
            })
        );
        assert_eq!(r.fake.connects(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn refused_connect_sets_link_false_and_retries() {
        let r = rig();
        r.fake.set_refuse(true);
        sleep(Duration::from_secs(20)).await;
        assert!(r.fake.connects() >= 4, "connects {}", r.fake.connects());
        assert_eq!(*r.session.link().borrow(), Some(false));
        r.fake.set_refuse(false);
        r.session
            .link()
            .wait_for(|l| *l == Some(true))
            .await
            .unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn load_launches_then_loads() {
        let r = rig();
        r.session.load(req(3, 42));
        let load = r.fake.wait_for("LOAD").await;
        let seq: Vec<(String, String)> = r
            .fake
            .requests()
            .into_iter()
            .filter(|q| q.2["type"] != "PING")
            .map(|q| (q.0, q.2["type"].as_str().unwrap().to_owned()))
            .collect();
        let want = [
            ("receiver-0", "CONNECT"),
            ("receiver-0", "GET_STATUS"),
            ("receiver-0", "LAUNCH"),
            ("t1", "CONNECT"),
            ("t1", "LOAD"),
        ];
        assert_eq!(
            seq,
            want.iter()
                .map(|(a, b)| (a.to_string(), b.to_string()))
                .collect::<Vec<_>>()
        );
        assert_eq!(r.fake.requests_of("LAUNCH")[0].2["appId"], "CC1AD845");
        let p = &load.2;
        assert_eq!(load.1, NS_MEDIA);
        assert_eq!(p["sessionId"], "s1");
        assert_eq!(p["autoplay"], true);
        assert_eq!(p["currentTime"], 0);
        let m = &p["media"];
        assert_eq!(
            m["contentId"],
            "http://127.0.0.1:8082/api/cast/stream/3?fromMs=42000"
        );
        assert_eq!(m["contentType"], "audio/mpeg");
        assert_eq!(m["streamType"], "BUFFERED");
        assert_eq!(m["metadata"]["metadataType"], 3);
        assert_eq!(m["metadata"]["title"], "Song");
        assert_eq!(m["metadata"]["artist"], "Artist");
        assert_eq!(m["metadata"]["albumName"], "Album");
        assert_eq!(
            m["metadata"]["images"][0]["url"],
            "http://pinson.lan/cover.jpg"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn public_url_override_used() {
        let r = rig_with(Some("http://pinson.lan:9/"));
        r.session.load(req(1, 0));
        let load = r.fake.wait_for("LOAD").await;
        let id = load.2["media"]["contentId"].as_str().unwrap().to_owned();
        assert!(
            id.starts_with("http://pinson.lan:9/api/cast/stream/"),
            "{id}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn second_load_reuses_app() {
        let r = rig();
        r.session.load(req(1, 0));
        r.fake.wait_for("LOAD").await;
        r.session.load(req(2, 0));
        r.fake.wait_for_n("LOAD", 2).await;
        assert_eq!(r.fake.count("LAUNCH"), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn playing_status_reports_position() {
        let mut r = rig();
        r.session.load(req(3, 42));
        assert!(matches!(
            next_event(&mut r).await,
            EngineEvent::Playing { generation: 3, position, .. } if position == Duration::from_secs(42)
        ));
        let m = r.fake.media_session_id();
        r.fake.push_media_status(m, "PLAYING", None, 1.5);
        assert!(matches!(
            next_event(&mut r).await,
            EngineEvent::Playing { generation: 3, position, .. } if position == Duration::from_millis(43_500)
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn finished_sends_track_ended_once() {
        let mut r = rig();
        r.session.load(req(3, 0));
        next_event(&mut r).await;
        let m = r.fake.media_session_id();
        r.fake.push_media_status(m, "IDLE", Some("FINISHED"), 0.0);
        r.fake.push_media_status(m, "IDLE", Some("FINISHED"), 0.0);
        assert_eq!(
            next_event(&mut r).await,
            EngineEvent::TrackEnded { generation: 3 }
        );
        assert_quiet(&mut r).await;
    }

    #[tokio::test(start_paused = true)]
    async fn load_failed_pauses_instead_of_failing() {
        let mut r = rig();
        r.fake.fail_next_load();
        r.session.load(req(3, 7));
        r.fake.wait_for("LOAD").await;
        while r.externals.lock().unwrap().is_empty() {
            sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(*r.externals.lock().unwrap(), vec![External::Pause]);
        assert_quiet(&mut r).await;
        r.session.resume();
        let second = r.fake.wait_for_n("LOAD", 2).await;
        assert_eq!(
            second.2["media"]["contentId"],
            "http://127.0.0.1:8082/api/cast/stream/3?fromMs=7000"
        );
        assert!(matches!(
            next_event(&mut r).await,
            EngineEvent::Playing { generation: 3, .. }
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn error_after_playing_reloads_then_fails() {
        let mut r = rig();
        r.session.load(req(3, 0));
        next_event(&mut r).await;
        let m = r.fake.media_session_id();
        r.fake.push_media_status(m, "IDLE", Some("ERROR"), 0.0);
        r.fake.wait_for_n("LOAD", 2).await;
        assert!(matches!(
            next_event(&mut r).await,
            EngineEvent::Playing { generation: 3, .. }
        ));
        assert!(r.externals.lock().unwrap().is_empty());
        let m2 = r.fake.media_session_id();
        assert_ne!(m, m2);
        r.fake.push_media_status(m2, "IDLE", Some("ERROR"), 0.0);
        assert_eq!(
            next_event(&mut r).await,
            EngineEvent::TrackFailed {
                generation: 3,
                reason: "the Cast device couldn't play the stream".into()
            }
        );
        assert_eq!(r.fake.count("LOAD"), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn old_media_session_ignored() {
        let mut r = rig();
        r.session.load(req(1, 0));
        next_event(&mut r).await;
        let m1 = r.fake.media_session_id();
        r.session.load(req(2, 0));
        assert!(matches!(
            next_event(&mut r).await,
            EngineEvent::Playing { generation: 2, .. }
        ));
        r.fake.push_media_status(m1, "IDLE", Some("FINISHED"), 0.0);
        assert_quiet(&mut r).await;
        let m2 = r.fake.media_session_id();
        r.fake.push_media_status(m2, "IDLE", Some("FINISHED"), 0.0);
        assert_eq!(
            next_event(&mut r).await,
            EngineEvent::TrackEnded { generation: 2 }
        );
    }

    #[tokio::test(start_paused = true)]
    async fn pause_resume_send_media_commands() {
        let mut r = rig();
        r.session.load(req(1, 0));
        next_event(&mut r).await;
        r.session.pause();
        let p = r.fake.wait_for("PAUSE").await;
        assert_eq!((p.0.as_str(), p.1.as_str()), ("t1", NS_MEDIA));
        assert_eq!(p.2["mediaSessionId"], r.fake.media_session_id());
        r.session.resume();
        let p = r.fake.wait_for("PLAY").await;
        assert_eq!(p.2["mediaSessionId"], r.fake.media_session_id());
    }

    #[tokio::test(start_paused = true)]
    async fn heartbeat_pings_and_pongs() {
        let r = rig();
        r.session
            .link()
            .wait_for(|l| *l == Some(true))
            .await
            .unwrap();
        let ping = r.fake.wait_for("PING").await;
        assert_eq!(
            (ping.0.as_str(), ping.1.as_str()),
            ("receiver-0", NS_HEARTBEAT)
        );
        r.fake.push(NS_HEARTBEAT, json!({"type": "PING"}));
        r.fake.wait_for("PONG").await;
        // Silence is only fatal without replies: the fake answers every PING.
        sleep(Duration::from_secs(60)).await;
        assert_eq!(*r.session.link().borrow(), Some(true));
        assert_eq!(r.fake.connects(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn drop_stops_app_and_ends_task() {
        let mut r = rig();
        r.session.load(req(1, 0));
        next_event(&mut r).await;
        let mut closed = r.session.closed();
        assert!(!*closed.borrow());
        drop(r.guard);
        let media_stop = r.fake.wait_for("STOP").await;
        assert_eq!(
            (media_stop.0.as_str(), media_stop.1.as_str()),
            ("t1", NS_MEDIA)
        );
        let recv_stop = r.fake.wait_for_n("STOP", 2).await;
        assert_eq!(
            (recv_stop.0.as_str(), recv_stop.1.as_str()),
            ("receiver-0", NS_RECEIVER)
        );
        assert_eq!(recv_stop.2["sessionId"], "s1");
        r.fake.wait_closed(1).await;
        timeout(Duration::from_secs(5), closed.wait_for(|c| *c))
            .await
            .expect("closed never fired")
            .unwrap();
        drop(r.session);
    }

    async fn link_cycle(r: &Rig) {
        let mut link = r.session.link();
        link.wait_for(|l| *l == Some(false)).await.unwrap();
        link.wait_for(|l| *l == Some(true)).await.unwrap();
    }

    async fn externals_len(r: &Rig, n: usize) {
        for _ in 0..3000 {
            if r.externals.lock().unwrap().len() >= n {
                return;
            }
            sleep(Duration::from_millis(10)).await;
        }
        panic!("expected {n} external calls");
    }

    async fn playing_at(r: &mut Rig, generation: u64, position: Duration) {
        loop {
            if let EngineEvent::Playing {
                generation: g,
                position: p,
                ..
            } = next_event(r).await
                && g == generation
                && p == position
            {
                return;
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn reconnect_rejoins_running_app() {
        let mut r = rig();
        r.session.load(req(3, 42));
        next_event(&mut r).await;
        r.fake.drop_link();
        link_cycle(&r).await;
        sleep(Duration::from_secs(2)).await;
        assert_eq!(r.fake.count("LAUNCH"), 1);
        assert_eq!(r.fake.count("LOAD"), 1);
        assert_eq!(r.fake.count("CONNECT"), 4);
        let m = r.fake.media_session_id();
        r.fake.push_media_status(m, "PLAYING", None, 2.0);
        playing_at(&mut r, 3, Duration::from_secs(44)).await;
    }

    #[tokio::test(start_paused = true)]
    async fn reconnect_reloads_when_app_gone() {
        let mut r = rig();
        r.session.load(req(3, 42));
        next_event(&mut r).await;
        let m = r.fake.media_session_id();
        r.fake.push_media_status(m, "PLAYING", None, 10.0);
        playing_at(&mut r, 3, Duration::from_secs(52)).await;
        r.fake.set_apps(vec![]);
        r.fake.drop_link();
        link_cycle(&r).await;
        let load = r.fake.wait_for_n("LOAD", 2).await;
        assert_eq!(r.fake.count("LAUNCH"), 2);
        assert_eq!(
            load.2["media"]["contentId"],
            "http://127.0.0.1:8082/api/cast/stream/3?fromMs=52000"
        );
        playing_at(&mut r, 3, Duration::from_secs(52)).await;
    }

    #[tokio::test(start_paused = true)]
    async fn silence_timeout_reconnects() {
        let r = rig();
        r.session
            .link()
            .wait_for(|l| *l == Some(true))
            .await
            .unwrap();
        r.fake.set_silent(true);
        sleep(Duration::from_secs(16)).await;
        assert_eq!(r.fake.connects(), 2);
        r.fake.set_silent(false);
        r.session
            .link()
            .wait_for(|l| *l == Some(true))
            .await
            .unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn takeover_pauses_pinson() {
        let mut r = rig();
        r.session.load(req(3, 42));
        next_event(&mut r).await;
        let m = r.fake.media_session_id();
        r.fake.push_media_status(m, "PLAYING", None, 5.0);
        playing_at(&mut r, 3, Duration::from_secs(47)).await;
        r.fake.push(
            NS_RECEIVER,
            json!({"type": "RECEIVER_STATUS", "status": {"applications": [
                {"appId": "233637DE", "sessionId": "yt", "transportId": "ty", "displayName": "YouTube"}
            ]}}),
        );
        externals_len(&r, 1).await;
        sleep(Duration::from_secs(2)).await;
        assert_eq!(*r.externals.lock().unwrap(), vec![External::Pause]);
        r.session.resume();
        let load = r.fake.wait_for_n("LOAD", 2).await;
        assert_eq!(r.fake.count("LAUNCH"), 2);
        assert_eq!(
            load.2["media"]["contentId"],
            "http://127.0.0.1:8082/api/cast/stream/3?fromMs=47000"
        );
        assert_eq!(load.2["autoplay"], true);
    }

    #[tokio::test(start_paused = true)]
    async fn external_pause_and_resume() {
        let mut r = rig();
        r.session.load(req(3, 0));
        next_event(&mut r).await;
        let m = r.fake.media_session_id();
        r.fake.push_media_status(m, "PAUSED", None, 1.0);
        externals_len(&r, 1).await;
        r.fake.push_media_status(m, "PLAYING", None, 1.0);
        externals_len(&r, 2).await;
        sleep(Duration::from_secs(1)).await;
        assert_eq!(
            *r.externals.lock().unwrap(),
            vec![External::Pause, External::Resume]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn own_reply_is_not_external() {
        let mut r = rig();
        r.session.load(req(3, 0));
        next_event(&mut r).await;
        r.session.pause();
        r.fake.wait_for("PAUSE").await;
        sleep(Duration::from_secs(2)).await;
        r.session.resume();
        r.fake.wait_for("PLAY").await;
        sleep(Duration::from_secs(2)).await;
        assert!(r.externals.lock().unwrap().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn stale_status_after_load_ignored() {
        let mut r = rig();
        r.session.load(req(1, 0));
        next_event(&mut r).await;
        let m1 = r.fake.media_session_id();
        r.session.load(req(2, 0));
        next_event(&mut r).await;
        r.fake.push_media_status(m1, "PAUSED", None, 3.0);
        assert_quiet(&mut r).await;
        assert!(r.externals.lock().unwrap().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn set_volume_round_trip() {
        let r = rig();
        r.session
            .link()
            .wait_for(|l| *l == Some(true))
            .await
            .unwrap();
        r.session.set_volume(Some(0.3), None).await.unwrap();
        let q = r.fake.wait_for("SET_VOLUME").await;
        assert_eq!((q.0.as_str(), q.1.as_str()), ("receiver-0", NS_RECEIVER));
        assert!((q.2["volume"]["level"].as_f64().unwrap() - 0.3).abs() < 1e-6);
        assert!(q.2["volume"].get("muted").is_none());
        assert_eq!(r.session.volume().borrow().unwrap().level, 0.3);
        r.session.set_volume(Some(0.6), Some(true)).await.unwrap();
        assert_eq!(r.fake.count("SET_VOLUME"), 3);
        let v = r.session.volume().borrow().unwrap();
        assert!(v.muted && v.level == 0.6);
    }

    #[tokio::test(start_paused = true)]
    async fn set_volume_while_down_errors() {
        let r = rig();
        r.fake.set_refuse(true);
        sleep(Duration::from_secs(3)).await;
        assert_eq!(
            r.session.set_volume(Some(0.3), None).await,
            Err("Can't reach the Chromecast".to_string())
        );
    }

    #[tokio::test(start_paused = true)]
    async fn set_volume_times_out_without_answer() {
        let r = rig();
        r.session
            .link()
            .wait_for(|l| *l == Some(true))
            .await
            .unwrap();
        r.fake.set_silent(true);
        assert_eq!(
            r.session.set_volume(None, Some(true)).await,
            Err("Can't reach the Chromecast".to_string())
        );
    }

    #[tokio::test(start_paused = true)]
    async fn device_volume_change_updates_watch() {
        let r = rig();
        let mut vol = r.session.volume();
        vol.wait_for(|v| v.is_some()).await.unwrap();
        r.fake.push(
            NS_RECEIVER,
            json!({"type": "RECEIVER_STATUS", "status": {"volume": {"level": 0.8, "muted": true}}}),
        );
        let v = *vol.wait_for(|v| v.is_some_and(|v| v.muted)).await.unwrap();
        assert_eq!(
            v,
            Some(Volume {
                level: 0.8,
                muted: true
            })
        );
    }

    #[tokio::test(start_paused = true)]
    async fn launch_error_pauses_instead_of_failing() {
        let mut r = rig();
        r.fake.fail_next_launch();
        r.session.load(req(3, 0));
        externals_len(&r, 1).await;
        assert_quiet(&mut r).await;
        assert_eq!(*r.externals.lock().unwrap(), vec![External::Pause]);
        assert_eq!(r.fake.count("LOAD"), 0);
        r.session.resume();
        r.fake.wait_for("LOAD").await;
        assert_eq!(r.fake.count("LAUNCH"), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn stopped_app_is_not_adopted() {
        let mut r = rig();
        r.session.load(req(1, 0));
        next_event(&mut r).await;
        r.session.stop();
        // The idle grace ends: the app is stopped.
        r.fake.wait_for_n("STOP", 2).await;
        r.fake.set_silent(true);
        r.session.load(req(2, 0));
        r.fake.wait_for_n("LAUNCH", 2).await;
        let stale = json!({"appId": APP_ID, "sessionId": "s1", "transportId": "t1"});
        r.fake.push(
            NS_RECEIVER,
            json!({"type": "RECEIVER_STATUS", "status": {"applications": [stale]}}),
        );
        sleep(Duration::from_secs(1)).await;
        assert_eq!(r.fake.count("LOAD"), 1, "adopted the stopped app");
        let rid = r.fake.requests_of("LAUNCH")[1].2["requestId"].clone();
        let fresh = json!({"appId": APP_ID, "sessionId": "s2", "transportId": "t2"});
        r.fake.push(
            NS_RECEIVER,
            json!({"type": "RECEIVER_STATUS", "requestId": rid, "status": {"applications": [fresh]}}),
        );
        let load = r.fake.wait_for_n("LOAD", 2).await;
        assert_eq!(load.0, "t2");
        assert_eq!(load.2["sessionId"], "s2");
    }

    fn receiver_stops(r: &Rig) -> usize {
        r.fake
            .requests_of("STOP")
            .iter()
            .filter(|q| q.1 == NS_RECEIVER)
            .count()
    }

    #[tokio::test(start_paused = true)]
    async fn seek_keeps_the_app() {
        let mut r = rig();
        r.session.load(req(1, 0));
        next_event(&mut r).await;
        // A seek: the player stops the engine, then plays from the new position.
        r.session.stop();
        r.session.load(req(2, 30));
        let load = r.fake.wait_for_n("LOAD", 2).await;
        assert_eq!(load.0, "t1");
        assert!(matches!(
            next_event(&mut r).await,
            EngineEvent::Playing { generation: 2, .. }
        ));
        sleep(Duration::from_secs(30)).await;
        assert_eq!(r.fake.count("LAUNCH"), 1);
        assert_eq!(receiver_stops(&r), 0);
        let media_stops = r.fake.requests_of("STOP");
        assert_eq!(media_stops.len(), 1);
        assert_eq!(media_stops[0].1, NS_MEDIA);
    }

    #[tokio::test(start_paused = true)]
    async fn idle_grace_stops_the_app() {
        let mut r = rig();
        r.session.load(req(1, 0));
        next_event(&mut r).await;
        r.session.stop();
        r.fake.wait_for("STOP").await;
        // A Load within the grace keeps the app.
        sleep(Duration::from_secs(10)).await;
        r.session.load(req(2, 0));
        r.fake.wait_for_n("LOAD", 2).await;
        sleep(Duration::from_secs(30)).await;
        assert_eq!(receiver_stops(&r), 0);
        assert_eq!(r.fake.count("LAUNCH"), 1);
        // The queue ends: after 20 s with no Load the app is stopped.
        r.session.stop();
        sleep(Duration::from_secs(19)).await;
        assert_eq!(receiver_stops(&r), 0);
        sleep(Duration::from_secs(2)).await;
        assert_eq!(receiver_stops(&r), 1);
        let stop = r.fake.requests_of("STOP").pop().unwrap();
        assert_eq!(
            (stop.0.as_str(), stop.2["sessionId"].as_str()),
            ("receiver-0", Some("s1"))
        );
        // The next song launches the app again.
        r.session.load(req(3, 0));
        r.fake.wait_for_n("LOAD", 3).await;
        assert_eq!(r.fake.count("LAUNCH"), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn load_then_pause_sends_autoplay_false() {
        let mut r = rig();
        r.session.load(req(1, 0));
        next_event(&mut r).await;
        // A paused seek: play_from then pause, back to back, with the app running.
        r.session.stop();
        r.session.load(req(2, 30));
        r.session.pause();
        let load = r.fake.wait_for_n("LOAD", 2).await;
        assert_eq!(load.2["autoplay"], false);
    }

    #[tokio::test(start_paused = true)]
    async fn orphan_load_status_is_not_bound() {
        let mut r = rig();
        r.fake.hold_loads(true);
        r.session.load(req(3, 0));
        r.fake.wait_for("LOAD").await;
        r.session.load(req(4, 0));
        r.fake.wait_for_n("LOAD", 2).await;
        // The device answers LOAD 3 late, then interrupts it, then answers LOAD 4.
        let m1 = r.fake.answer_load();
        r.fake
            .push_media_status(m1, "IDLE", Some("INTERRUPTED"), 0.0);
        let m2 = r.fake.answer_load();
        assert_ne!(m1, m2);
        r.fake.push_media_status(m2, "PLAYING", None, 1.0);
        playing_at(&mut r, 4, Duration::from_secs(1)).await;
        r.fake.push_media_status(m1, "IDLE", Some("FINISHED"), 0.0);
        assert_quiet(&mut r).await;
        r.fake.push_media_status(m2, "IDLE", Some("FINISHED"), 0.0);
        assert_eq!(
            next_event(&mut r).await,
            EngineEvent::TrackEnded { generation: 4 }
        );
    }

    #[tokio::test(start_paused = true)]
    async fn accept_then_drop_backs_off() {
        let r = rig();
        r.fake.set_drop_on_accept(true);
        let mut link = r.session.link();
        let saw_up = tokio::spawn(async move { link.wait_for(|l| *l == Some(true)).await.is_ok() });
        sleep(Duration::from_secs(40)).await;
        assert!(!saw_up.is_finished(), "link went up without a message");
        assert_eq!(*r.session.link().borrow(), Some(false));
        let times = r.fake.connect_times();
        let gaps: Vec<u64> = times
            .windows(2)
            .map(|w| (w[1] - w[0]).as_millis() as u64)
            .collect();
        let want = [500, 1000, 2000, 4000, 5000, 5000, 5000];
        assert!(gaps.len() >= want.len(), "gaps {gaps:?}");
        for (gap, want) in gaps.iter().zip(want) {
            assert!((want..want + 100).contains(gap), "gaps {gaps:?}");
        }
    }

    async fn connects_reach(r: &Rig, n: usize) {
        for _ in 0..3000 {
            if r.fake.connects() >= n {
                return;
            }
            sleep(Duration::from_millis(10)).await;
        }
        panic!("expected {n} connects");
    }

    #[tokio::test(start_paused = true)]
    async fn two_drops_before_status_still_rejoin() {
        let mut r = rig();
        r.session.load(req(3, 42));
        next_event(&mut r).await;
        // The second connection gets no RECEIVER_STATUS before it drops too.
        r.fake.set_silent(true);
        r.fake.drop_link();
        connects_reach(&r, 2).await;
        sleep(Duration::from_millis(100)).await;
        r.fake.drop_link();
        r.fake.set_silent(false);
        connects_reach(&r, 3).await;
        r.session
            .link()
            .wait_for(|l| *l == Some(true))
            .await
            .unwrap();
        sleep(Duration::from_secs(2)).await;
        assert_eq!(r.fake.count("LAUNCH"), 1);
        assert_eq!(r.fake.count("LOAD"), 1);
        let m = r.fake.media_session_id();
        r.fake.push_media_status(m, "PLAYING", None, 2.0);
        playing_at(&mut r, 3, Duration::from_secs(44)).await;
    }

    #[tokio::test(start_paused = true)]
    async fn external_pause_during_poll() {
        let mut r = rig();
        r.session.load(req(3, 0));
        next_event(&mut r).await;
        // The first media poll goes out and gets no answer.
        r.fake.set_silent(true);
        r.fake.wait_for_n("GET_STATUS", 2).await;
        let m = r.fake.media_session_id();
        r.fake.push_media_status(m, "PAUSED", None, 4.0);
        externals_len(&r, 1).await;
        assert_eq!(*r.externals.lock().unwrap(), vec![External::Pause]);
    }

    #[tokio::test(start_paused = true)]
    async fn poll_reply_contradiction_is_external() {
        let mut r = rig();
        r.session.load(req(3, 0));
        next_event(&mut r).await;
        // Paused on the device, but the unsolicited status was lost: the poll finds it.
        r.fake.set_media_state("PAUSED");
        externals_len(&r, 1).await;
        sleep(Duration::from_secs(6)).await;
        assert_eq!(*r.externals.lock().unwrap(), vec![External::Pause]);
    }

    #[tokio::test(start_paused = true)]
    async fn unanswered_request_ages_out() {
        let mut r = rig();
        r.session.load(req(3, 0));
        next_event(&mut r).await;
        r.fake.set_silent(true);
        r.session.pause();
        r.fake.wait_for("PAUSE").await;
        sleep(Duration::from_secs(6)).await;
        r.fake.set_silent(false);
        let m = r.fake.media_session_id();
        r.fake.push_media_status(m, "PLAYING", None, 1.0);
        externals_len(&r, 1).await;
        assert_eq!(r.externals.lock().unwrap()[0], External::Resume);
    }
}
