//! A session with one UPnP renderer: connects, loads Pinson's per-song stream over SOAP
//! and turns the polled transport state into `EngineEvent`s.
//!
//! One task owns all state (`Core`) and awaits one SOAP call at a time inside a `select!`
//! over commands, the poll timer, the retry timer and the shutdown signal. Commands only
//! change the *wanted* state (`Core::cur`, `wanted_paused`, `stop_pending`); `sync` then
//! makes the device match it. That is what lets a queued `Stop` + `Load` (a seek) become a
//! single stop, and a queued `Load` + `Pause` load without playing.
//!
//! Every reply is checked against the load it answers: the track URI must be ours and
//! name this load's start, and replies in the settle window after a command of ours are
//! not acted on, so a poll answer about the previous load never binds to the new one.

use std::future::Future;
use std::io;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use futures_util::future::BoxFuture;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::{Instant, sleep_until, timeout};

use super::control::{RendererControl, TransportInfo, TransportState};
use super::xml::{SoapError, didl};
use crate::engine::EngineEvent;
use crate::output::renderer::{External, SongMeta, Volume};

const BACKOFF_MIN: Duration = Duration::from_millis(500);
const BACKOFF_MAX: Duration = Duration::from_secs(5);
/// Every SOAP call gives up after this long.
const CALL_TIMEOUT: Duration = Duration::from_secs(3);
/// The stop sent when the guard drops gets this long.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);
const VOLUME_TIMEOUT: Duration = Duration::from_secs(3);
/// A stop this much before the end of a song is the user's, not the song ending.
const END_SLACK: Duration = Duration::from_secs(5);
/// A report this far from the predicted position moves the clock.
const DRIFT: Duration = Duration::from_millis(1500);
/// How long after its duration a song still "playing" counts as over.
const OVERDUE: Duration = Duration::from_secs(3);
const STREAM_PREFIX: &str = "/api/upnp/stream/";
const UNREACHABLE: &str = "Can't reach the renderer";

/// An open control connection to the renderer.
pub struct UpnpLink {
    pub control: Arc<dyn RendererControl>,
    /// Our address as the renderer sees it.
    pub local_ip: IpAddr,
    /// A Sonos: gets `x-rincon-mp3radio://` URLs.
    pub sonos: bool,
}

/// Opens connections to the renderer; called again after every loss.
pub trait UpnpConnector: Send + Sync + 'static {
    /// Shown in logs.
    fn target(&self) -> String;
    fn connect(&self) -> BoxFuture<'static, io::Result<UpnpLink>>;
}

#[derive(Clone, Debug, PartialEq)]
pub struct LoadReq {
    pub generation: u64,
    /// Like `/api/upnp/stream/3/song.mp3`.
    pub path: String,
    pub from: Duration,
    pub meta: Option<SongMeta>,
}

pub struct SessionConfig {
    pub connector: Arc<dyn UpnpConnector>,
    /// `MUSIC_PUBLIC_URL`; overrides the address the renderer is told to fetch from.
    pub public_url: Option<String>,
    pub port: u16,
    /// Poll interval while a song is loaded.
    pub poll_playing: Duration,
    /// Poll interval otherwise (a cheap liveness check).
    pub poll_idle: Duration,
    /// After a command of ours, state changes are not taken for the device's own.
    pub settle: Duration,
    /// A started song that is not `PLAYING` after this long can't be played.
    pub load_timeout: Duration,
}

impl SessionConfig {
    pub fn new(connector: Arc<dyn UpnpConnector>, public_url: Option<String>, port: u16) -> Self {
        Self {
            connector,
            public_url,
            port,
            poll_playing: Duration::from_secs(1),
            poll_idle: Duration::from_secs(5),
            settle: Duration::from_secs(3),
            load_timeout: Duration::from_secs(20),
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

/// A handle to the session task. Clones share the task.
#[derive(Clone)]
pub struct UpnpSession {
    cmds: mpsc::UnboundedSender<Cmd>,
    link: watch::Receiver<Option<bool>>,
    volume: watch::Receiver<Option<Volume>>,
    closed: watch::Receiver<bool>,
}

/// Dropping it stops the renderer (within 2 s) and ends the task, whatever handles remain.
pub struct SessionGuard {
    _shutdown: oneshot::Sender<()>,
}

impl UpnpSession {
    pub fn load(&self, req: LoadReq) {
        self.cmds.send(Cmd::Load(req)).ok();
    }
    pub fn pause(&self) {
        self.cmds.send(Cmd::Pause).ok();
    }
    pub fn resume(&self) {
        self.cmds.send(Cmd::Resume).ok();
    }
    /// Stops the renderer and forgets the song. Cheap to call right before a `load`: the
    /// two queued together are one stop.
    pub fn stop(&self) {
        self.cmds.send(Cmd::Stop).ok();
    }
    /// Sets the renderer's volume and/or mute; resolves when it has answered.
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
    /// `None` until the first attempt ends, then whether the renderer is reachable.
    pub fn link(&self) -> watch::Receiver<Option<bool>> {
        self.link.clone()
    }
    pub fn volume(&self) -> watch::Receiver<Option<Volume>> {
        self.volume.clone()
    }
    /// Becomes `true` once the task has ended, after its stop was sent (or gave up).
    pub fn closed(&self) -> watch::Receiver<bool> {
        self.closed.clone()
    }
}

pub fn spawn(
    cfg: SessionConfig,
    events: mpsc::UnboundedSender<EngineEvent>,
    external: Arc<dyn Fn(External) + Send + Sync>,
) -> (UpnpSession, SessionGuard) {
    let (cmds, cmd_rx) = mpsc::unbounded_channel();
    let (link_tx, link) = watch::channel(None);
    let (volume_tx, volume) = watch::channel(None);
    let (closed_tx, closed) = watch::channel(false);
    let (shutdown_tx, shutdown) = oneshot::channel();
    let core = Core {
        cfg,
        events,
        external,
        link_tx,
        volume_tx,
        closed_tx,
        shutdown,
        shutting: false,
        ctl: None,
        local_ip: None,
        sonos: false,
        retry_at: None,
        backoff: BACKOFF_MIN,
        cur: None,
        wanted_paused: false,
        stop_pending: false,
        settle_until: Instant::now(),
        next_poll: Instant::now(),
    };
    tokio::spawn(core.run(cmd_rx));
    (
        UpnpSession {
            cmds,
            link,
            volume,
            closed,
        },
        SessionGuard {
            _shutdown: shutdown_tx,
        },
    )
}

/// The song we want on the renderer.
struct Current {
    req: LoadReq,
    /// Song position the current load starts at; `RelTime` is relative to it.
    base: Duration,
    /// Last position the renderer reported while playing or paused (starts at `req.from`).
    last_pos: Duration,
    /// The URL last set on the renderer; `None` until a `SetAVTransportURI` succeeded.
    url: Option<String>,
    /// A (re)load is wanted: stop, set the URI.
    needs_load: bool,
    /// We last told the renderer to play (not pause).
    commanded_playing: bool,
    /// When we last told it to play: the load timeout counts from here.
    started_at: Instant,
    /// This load has reached `PLAYING` at least once.
    played: bool,
    /// Reloaded after an error already.
    reloaded: bool,
    /// Over (finished or failed): no more events, no reload.
    ended: bool,
    /// The renderer couldn't play it; `resume` loads it again.
    held: bool,
    /// The last `Playing` report: position and when. `None` means the next `PLAYING` is
    /// reported (a new load, or after a pause).
    anchor: Option<(Duration, Instant)>,
}

impl Current {
    fn new(req: LoadReq) -> Self {
        Self {
            base: req.from,
            last_pos: req.from,
            req,
            url: None,
            needs_load: true,
            commanded_playing: false,
            started_at: Instant::now(),
            played: false,
            reloaded: false,
            ended: false,
            held: false,
            anchor: None,
        }
    }

    /// On the renderer and being followed.
    fn active(&self) -> bool {
        self.url.is_some() && !self.needs_load && !self.held && !self.ended
    }

    /// Wants a load again at the last position.
    fn reload(&mut self) {
        self.base = self.last_pos;
        self.needs_load = true;
        self.played = false;
        self.anchor = None;
    }

    fn duration(&self) -> Option<Duration> {
        self.req.meta.as_ref().and_then(|m| m.duration)
    }
}

enum Hold {
    /// The renderer couldn't fetch or play the stream.
    Failed,
    /// Another source took the renderer.
    Takeover,
    /// Stopped on the device in the middle of the song.
    Stopped,
}

enum Kind {
    /// The link dropped: it is already handled.
    Lost,
    /// The session is shutting down.
    Closing,
    /// The renderer answered with an error.
    Fault,
}

enum Ev {
    Shutdown,
    Cmd(Option<Cmd>),
    Connected(io::Result<UpnpLink>),
    Retry,
    Poll,
}

struct Core {
    cfg: SessionConfig,
    events: mpsc::UnboundedSender<EngineEvent>,
    external: Arc<dyn Fn(External) + Send + Sync>,
    link_tx: watch::Sender<Option<bool>>,
    volume_tx: watch::Sender<Option<Volume>>,
    closed_tx: watch::Sender<bool>,
    shutdown: oneshot::Receiver<()>,
    /// The shutdown signal has been taken (by `rpc` or the loop): do not poll it again.
    shutting: bool,
    /// `Some` while connected.
    ctl: Option<Arc<dyn RendererControl>>,
    local_ip: Option<IpAddr>,
    sonos: bool,
    retry_at: Option<Instant>,
    backoff: Duration,
    cur: Option<Current>,
    wanted_paused: bool,
    /// A `Stop` is owed to the renderer (cleared by a `Load`, which stops itself).
    stop_pending: bool,
    /// Until then, state seen on the renderer may still be the effect of an earlier command.
    settle_until: Instant,
    next_poll: Instant,
}

impl Core {
    async fn run(mut self, mut cmd_rx: mpsc::UnboundedReceiver<Cmd>) {
        let mut cmds_open = true;
        let mut connecting: Option<BoxFuture<'static, io::Result<UpnpLink>>> =
            Some(self.cfg.connector.connect());
        while !self.shutting {
            let retry_at = self.retry_at;
            let poll_at = self.ctl.is_some().then_some(self.next_poll);
            let ev = tokio::select! {
                _ = &mut self.shutdown => Ev::Shutdown,
                cmd = cmd_rx.recv(), if cmds_open => Ev::Cmd(cmd),
                res = async { connecting.as_mut().unwrap().await }, if connecting.is_some() => Ev::Connected(res),
                _ = async { sleep_until(retry_at.unwrap()).await }, if retry_at.is_some() => Ev::Retry,
                _ = async { sleep_until(poll_at.unwrap()).await }, if poll_at.is_some() => Ev::Poll,
            };
            match ev {
                Ev::Shutdown => break,
                Ev::Cmd(None) => cmds_open = false,
                Ev::Cmd(Some(cmd)) => {
                    self.on_cmd(cmd).await;
                    // Commands sent together (stop then load, load then pause) all apply
                    // to the wanted state before anything is sent.
                    while let Ok(cmd) = cmd_rx.try_recv() {
                        self.on_cmd(cmd).await;
                    }
                }
                Ev::Connected(res) => {
                    connecting = None;
                    match res {
                        Ok(link) => self.on_connected(link).await,
                        Err(e) => {
                            tracing::warn!(
                                "UPnP {}: connect failed: {e}",
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
                Ev::Poll => self.poll().await,
            }
            if !self.shutting {
                self.sync().await;
            }
        }
        self.finish().await;
        self.closed_tx.send_replace(true);
    }

    /// Stops the renderer if we left something on it, within `CLOSE_TIMEOUT`.
    async fn finish(&mut self) {
        let owed = self.stop_pending || self.cur.as_ref().is_some_and(|c| c.url.is_some());
        if let (true, Some(ctl)) = (owed, self.ctl.clone()) {
            timeout(CLOSE_TIMEOUT, ctl.stop()).await.ok();
        }
    }

    // --- calls and link ---------------------------------------------------------------

    /// One SOAP call, bounded by `CALL_TIMEOUT` and cut short by shutdown. A timeout or
    /// shutdown comes back as a `Transport` error; `classify` tells them apart.
    async fn rpc<T>(
        &mut self,
        fut: impl Future<Output = Result<T, SoapError>>,
    ) -> Result<T, SoapError> {
        if self.shutting {
            return Err(SoapError::Transport("closing".into()));
        }
        tokio::select! {
            biased;
            _ = &mut self.shutdown => {
                self.shutting = true;
                Err(SoapError::Transport("closing".into()))
            }
            r = timeout(CALL_TIMEOUT, fut) => match r {
                Ok(r) => r,
                Err(_) => Err(SoapError::Transport("timed out".into())),
            },
        }
    }

    /// Looks at a failed call: a transport error drops the link (keeping the wanted song).
    fn classify(&mut self, e: &SoapError) -> Kind {
        if self.shutting {
            Kind::Closing
        } else if let SoapError::Transport(why) = e {
            self.on_lost(why);
            Kind::Lost
        } else {
            Kind::Fault
        }
    }

    fn on_lost(&mut self, why: &str) {
        tracing::warn!("UPnP {}: link lost ({why})", self.cfg.connector.target());
        self.ctl = None;
        self.link_tx.send_replace(Some(false));
        self.schedule_retry();
    }

    fn schedule_retry(&mut self) {
        self.retry_at = Some(Instant::now() + self.backoff);
        self.backoff = (self.backoff * 2).min(BACKOFF_MAX);
    }

    async fn on_connected(&mut self, link: UpnpLink) {
        self.local_ip = Some(link.local_ip);
        self.sonos = link.sonos;
        self.ctl = Some(link.control.clone());
        // The first call is what proves the renderer is there: only its success resets
        // the backoff and raises the link (a connect alone can succeed offline).
        let volume = match self.rpc(link.control.volume()).await {
            Ok(v) => v,
            Err(e) => match self.classify(&e) {
                Kind::Lost | Kind::Closing => return,
                Kind::Fault => None,
            },
        };
        self.backoff = BACKOFF_MIN;
        self.link_tx.send_replace(Some(true));
        self.volume_tx.send_replace(volume);
        self.rejoin().await;
        self.next_poll = Instant::now() + self.poll_every();
    }

    /// After a reconnect: if the renderer still has the URL we last sent, keep it; else
    /// load the wanted song again at its last position.
    async fn rejoin(&mut self) {
        let Some(ctl) = self.ctl.clone() else { return };
        let Some(sent) = self
            .cur
            .as_ref()
            .filter(|c| c.active())
            .and_then(|c| c.url.clone())
        else {
            return;
        };
        let same = match self.rpc(ctl.position_info()).await {
            Ok(pi) => pi.track_uri == sent,
            Err(e) => match self.classify(&e) {
                Kind::Lost | Kind::Closing => return,
                Kind::Fault => false,
            },
        };
        if !same && let Some(cur) = self.cur.as_mut() {
            cur.reload();
        }
    }

    // --- commands ---------------------------------------------------------------------

    async fn on_cmd(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Load(req) => {
                self.cur = Some(Current::new(req));
                self.wanted_paused = false;
                // The load stops the renderer itself.
                self.stop_pending = false;
            }
            Cmd::Pause => self.wanted_paused = true,
            Cmd::Resume => {
                self.wanted_paused = false;
                if let Some(cur) = self.cur.as_mut().filter(|c| c.held && !c.ended) {
                    cur.held = false;
                    cur.reload();
                }
            }
            Cmd::Stop => {
                // Owed only if something was ever sent; sent in `sync`, unless a `Load`
                // follows in the same batch.
                if self.cur.as_ref().is_some_and(|c| c.url.is_some()) {
                    self.stop_pending = true;
                }
                self.cur = None;
            }
            Cmd::Volume {
                level,
                muted,
                reply,
            } => {
                let res = self.set_volume(level, muted).await;
                reply.send(res).ok();
            }
        }
    }

    async fn set_volume(&mut self, level: Option<f32>, muted: Option<bool>) -> Result<(), String> {
        let Some(ctl) = self.ctl.clone() else {
            return Err(UNREACHABLE.into());
        };
        match self.rpc(ctl.set_volume(level, muted)).await {
            Ok(()) => {
                if let Ok(v) = self.rpc(ctl.volume()).await {
                    self.volume_tx.send_replace(v);
                }
                Ok(())
            }
            Err(e) => match self.classify(&e) {
                Kind::Fault => Err(e.to_string()),
                _ => Err(UNREACHABLE.into()),
            },
        }
    }

    // --- making the renderer match what is wanted -----------------------------------------

    fn settle(&mut self) {
        self.settle_until = Instant::now() + self.cfg.settle;
    }

    fn poll_every(&self) -> Duration {
        if self.cur.as_ref().is_some_and(|c| c.active()) {
            self.cfg.poll_playing
        } else {
            self.cfg.poll_idle
        }
    }

    fn base_url(&self) -> String {
        match (&self.cfg.public_url, self.local_ip) {
            (Some(url), _) => url.trim_end_matches('/').to_owned(),
            (None, Some(IpAddr::V4(ip))) => format!("http://{ip}:{}", self.cfg.port),
            (None, Some(IpAddr::V6(ip))) => format!("http://[{ip}]:{}", self.cfg.port),
            (None, None) => format!("http://localhost:{}", self.cfg.port),
        }
    }

    fn stream_url(&self, path: &str, from: Duration) -> String {
        let url = format!("{}{}?fromMs={}", self.base_url(), path, from.as_millis());
        match url.strip_prefix("http://") {
            Some(rest) if self.sonos => format!("x-rincon-mp3radio://{rest}"),
            _ => url,
        }
    }

    /// Sends what the wanted state needs: an owed stop, a load, then play or pause.
    async fn sync(&mut self) {
        let Some(ctl) = self.ctl.clone() else { return };
        if self.stop_pending {
            match self.rpc(ctl.stop()).await {
                Ok(()) => {
                    self.stop_pending = false;
                    self.settle();
                }
                Err(e) => match self.classify(&e) {
                    Kind::Fault => self.stop_pending = false,
                    Kind::Lost | Kind::Closing => return,
                },
            }
        }
        if self
            .cur
            .as_ref()
            .is_some_and(|c| c.needs_load && !c.held && !c.ended)
        {
            self.do_load(&ctl).await;
            if self.ctl.is_none() || self.shutting {
                return;
            }
        }
        self.reconcile(&ctl).await;
    }

    /// Stop, then set the URI. Playing is `reconcile`'s job. A fault holds the song.
    async fn do_load(&mut self, ctl: &Arc<dyn RendererControl>) {
        let Some(cur) = self.cur.as_ref() else { return };
        let url = self.stream_url(&cur.req.path, cur.base);
        let meta = didl(cur.req.meta.as_ref(), &url);
        // A fault on the stop is ignored.
        if let Err(e) = self.rpc(ctl.stop()).await
            && !matches!(self.classify(&e), Kind::Fault)
        {
            return;
        }
        match self.rpc(ctl.set_uri(&url, &meta)).await {
            Ok(()) => {
                self.settle();
                self.next_poll = Instant::now() + self.cfg.poll_playing;
                if let Some(cur) = self.cur.as_mut() {
                    cur.url = Some(url);
                    cur.needs_load = false;
                    cur.commanded_playing = false;
                    cur.played = false;
                    cur.anchor = None;
                }
            }
            Err(e) => {
                if matches!(self.classify(&e), Kind::Fault) {
                    self.hold(Hold::Failed);
                }
            }
        }
    }

    /// Makes the renderer play or pause as Pinson wants.
    async fn reconcile(&mut self, ctl: &Arc<dyn RendererControl>) {
        let want_play = !self.wanted_paused;
        let Some(cur) = self.cur.as_ref().filter(|c| c.active()) else {
            return;
        };
        if cur.commanded_playing == want_play {
            return;
        }
        if want_play {
            match self.rpc(ctl.play()).await {
                Ok(()) => {
                    self.settle();
                    if let Some(cur) = self.cur.as_mut() {
                        cur.commanded_playing = true;
                        cur.started_at = Instant::now();
                    }
                }
                Err(e) => {
                    if matches!(self.classify(&e), Kind::Fault) {
                        self.hold(Hold::Failed);
                    }
                }
            }
        } else {
            let res = self.rpc(ctl.pause()).await;
            if let Err(e) = &res
                && !matches!(self.classify(e), Kind::Fault)
            {
                return;
            }
            // A fault is not retried: the next poll follows whatever the device did.
            self.settle();
            if let Some(cur) = self.cur.as_mut() {
                cur.commanded_playing = false;
                cur.anchor = None;
            }
        }
    }

    /// Pauses Pinson and keeps the song so `resume` loads it again at the last position.
    fn hold(&mut self, why: Hold) {
        let Some(cur) = self.cur.as_mut().filter(|c| !c.ended && !c.held) else {
            return;
        };
        cur.held = true;
        cur.needs_load = false;
        cur.anchor = None;
        let (path, from) = (cur.req.path.clone(), cur.last_pos);
        let url = self.stream_url(&path, from);
        match why {
            Hold::Failed => tracing::warn!(
                "the renderer couldn't play {url}: check MUSIC_PUBLIC_URL and the firewall"
            ),
            Hold::Takeover => tracing::warn!("another source took the renderer"),
            Hold::Stopped => tracing::info!("the renderer was stopped on the device"),
        }
        self.wanted_paused = true;
        (self.external)(External::Pause);
    }

    // --- polling ----------------------------------------------------------------------

    async fn poll(&mut self) {
        let Some(ctl) = self.ctl.clone() else { return };
        let active = self.cur.as_ref().is_some_and(|c| c.active());
        let ti = match self.rpc(ctl.transport_info()).await {
            Ok(t) => t,
            Err(e) => {
                self.classify(&e);
                self.next_poll = Instant::now() + self.poll_every();
                return;
            }
        };
        if !active {
            // Only a liveness check while nothing is being followed.
            self.next_poll = Instant::now() + self.poll_every();
            return;
        }
        let pi = match self.rpc(ctl.position_info()).await {
            Ok(p) => p,
            Err(e) => {
                self.classify(&e);
                self.next_poll = Instant::now() + self.poll_every();
                return;
            }
        };
        // The wanted song may have changed during the calls.
        if self.cur.as_ref().is_some_and(|c| c.active()) {
            self.on_state(ti, pi.rel_time, &pi.track_uri);
        }
        self.next_poll = Instant::now() + self.poll_every();
    }

    fn on_state(&mut self, ti: TransportInfo, rel: Option<Duration>, uri: &str) {
        let now = Instant::now();
        let settling = now < self.settle_until;
        let Some(cur) = self.cur.as_ref() else { return };
        // Which load does this answer belong to?
        if uri.is_empty() {
            if settling {
                return;
            }
        } else if uri.contains(&cur.req.path) {
            if let Some(ms) = from_ms(uri)
                && ms != cur.base.as_millis()
            {
                return; // an earlier load of this same song
            }
        } else if uri.contains(STREAM_PREFIX) {
            return; // an earlier song of ours
        } else if settling {
            return;
        } else {
            self.hold(Hold::Takeover);
            return;
        }

        if ti.error && !settling {
            self.on_error();
            return;
        }
        match ti.state {
            TransportState::Playing => {
                if !settling && self.wanted_paused {
                    // Resumed on the device.
                    self.wanted_paused = false;
                    if let Some(cur) = self.cur.as_mut() {
                        cur.commanded_playing = true;
                        cur.anchor = None;
                    }
                    (self.external)(External::Resume);
                }
                let paused = self.wanted_paused;
                let Some(cur) = self.cur.as_mut() else { return };
                cur.played = true;
                if let Some(rel) = rel {
                    let pos = cur.base + rel;
                    cur.last_pos = pos;
                    let moved = match cur.anchor {
                        None => true,
                        Some((p, at)) => (p + (now - at)).abs_diff(pos) > DRIFT,
                    };
                    if moved && !paused {
                        cur.anchor = Some((pos, now));
                        let generation = cur.req.generation;
                        self.events
                            .send(EngineEvent::Playing {
                                generation,
                                position: pos,
                                at: now,
                            })
                            .ok();
                    }
                }
            }
            TransportState::Paused => {
                if let Some(cur) = self.cur.as_mut() {
                    cur.played = true;
                    if let Some(rel) = rel {
                        cur.last_pos = cur.base + rel;
                    }
                    cur.anchor = None;
                }
                if !settling && !self.wanted_paused {
                    // Paused on the device.
                    self.wanted_paused = true;
                    if let Some(cur) = self.cur.as_mut() {
                        cur.commanded_playing = false;
                    }
                    (self.external)(External::Pause);
                }
            }
            TransportState::Stopped | TransportState::NoMedia => {
                if !settling && !self.wanted_paused {
                    self.on_stopped(now);
                }
            }
            TransportState::Transitioning | TransportState::Other => {}
        }
        self.check_overdue(now);
    }

    fn on_stopped(&mut self, now: Instant) {
        let Some(cur) = self.cur.as_ref() else { return };
        if cur.played {
            if cur.duration().is_some_and(|d| cur.last_pos + END_SLACK < d) {
                self.hold(Hold::Stopped);
            } else {
                self.end(None);
            }
        } else if now.saturating_duration_since(cur.started_at) >= self.cfg.load_timeout {
            self.hold(Hold::Failed);
        }
    }

    fn on_error(&mut self) {
        let Some(cur) = self.cur.as_mut() else { return };
        if cur.reloaded {
            self.end(Some("the renderer couldn't play the stream".into()));
        } else if cur.played {
            cur.reloaded = true;
            cur.reload();
        } else {
            self.hold(Hold::Failed);
        }
    }

    /// A song still "playing" well past its duration is over (Sonos reconnecting to its
    /// radio stream, renderers that never report `STOPPED`).
    fn check_overdue(&mut self, now: Instant) {
        let Some(cur) = self.cur.as_ref().filter(|c| c.active()) else {
            return;
        };
        let (Some(d), Some((p, at)), false) = (cur.duration(), cur.anchor, self.wanted_paused)
        else {
            return;
        };
        if cur.played && p + (now - at) >= d + OVERDUE {
            self.end(None);
        }
    }

    /// The song is over: `TrackEnded`, or `TrackFailed` with a reason. At most once.
    fn end(&mut self, failed: Option<String>) {
        let Some(cur) = self.cur.as_mut().filter(|c| !c.ended) else {
            return;
        };
        cur.ended = true;
        let generation = cur.req.generation;
        self.events
            .send(match failed {
                None => EngineEvent::TrackEnded { generation },
                Some(reason) => EngineEvent::TrackFailed { generation, reason },
            })
            .ok();
    }
}

/// The `fromMs` query value in a track URI, if it names one.
fn from_ms(uri: &str) -> Option<u128> {
    let rest = &uri[uri.find("fromMs=")? + "fromMs=".len()..];
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    use async_trait::async_trait;
    use tokio::time::{sleep, timeout};

    use super::super::control::{PositionInfo, TransportInfo};
    use super::super::fake::FakeControl;
    use super::*;

    struct FakeConnector {
        control: Arc<dyn RendererControl>,
        sonos: AtomicBool,
        refuse: AtomicBool,
        attempts: Mutex<Vec<Instant>>,
    }

    impl UpnpConnector for FakeConnector {
        fn target(&self) -> String {
            "fake renderer".into()
        }
        fn connect(&self) -> BoxFuture<'static, io::Result<UpnpLink>> {
            self.attempts.lock().unwrap().push(Instant::now());
            let res = if self.refuse.load(Ordering::SeqCst) {
                Err(io::Error::new(io::ErrorKind::ConnectionRefused, "refused"))
            } else {
                Ok(UpnpLink {
                    control: self.control.clone(),
                    local_ip: "10.0.0.2".parse().unwrap(),
                    sonos: self.sonos.load(Ordering::SeqCst),
                })
            };
            Box::pin(async move { res })
        }
    }

    struct Rig {
        ctl: FakeControl,
        conn: Arc<FakeConnector>,
        session: UpnpSession,
        guard: SessionGuard,
        events: mpsc::UnboundedReceiver<EngineEvent>,
        externals: Arc<Mutex<Vec<External>>>,
        t0: Instant,
    }

    fn rig_with(control: Arc<dyn RendererControl>, ctl: FakeControl, public: Option<&str>) -> Rig {
        let conn = Arc::new(FakeConnector {
            control,
            sonos: AtomicBool::new(false),
            refuse: AtomicBool::new(false),
            attempts: Mutex::new(Vec::new()),
        });
        let (tx, events) = mpsc::unbounded_channel();
        let externals = Arc::new(Mutex::new(Vec::new()));
        let ext = externals.clone();
        let cfg = SessionConfig::new(conn.clone(), public.map(String::from), 8082);
        let t0 = Instant::now();
        let (session, guard) = spawn(cfg, tx, Arc::new(move |e| ext.lock().unwrap().push(e)));
        Rig {
            ctl,
            conn,
            session,
            guard,
            events,
            externals,
            t0,
        }
    }

    fn rig() -> Rig {
        let ctl = FakeControl::new();
        ctl.set_volume_state(Some(Volume {
            level: 0.5,
            muted: false,
        }));
        rig_with(Arc::new(ctl.clone()), ctl, None)
    }

    fn req(generation: u64, from_s: u64, duration_s: Option<u64>) -> LoadReq {
        LoadReq {
            generation,
            path: format!("/api/upnp/stream/{generation}/song.mp3"),
            from: Duration::from_secs(from_s),
            meta: Some(SongMeta {
                title: "Song".into(),
                artist: "Artist".into(),
                album: None,
                image: None,
                duration: duration_s.map(Duration::from_secs),
            }),
        }
    }

    fn url(generation: u64, from_ms: u64) -> String {
        format!("http://10.0.0.2:8082/api/upnp/stream/{generation}/song.mp3?fromMs={from_ms}")
    }

    /// The commands sent to the renderer, without the polls.
    fn ops(r: &Rig) -> Vec<String> {
        r.ctl
            .calls()
            .into_iter()
            .filter(|c| !c.starts_with("Get"))
            .collect()
    }

    fn externals(r: &Rig) -> Vec<External> {
        r.externals.lock().unwrap().clone()
    }

    /// Everything sent so far except `Playing` reports.
    fn terminal(r: &mut Rig) -> Vec<EngineEvent> {
        let mut out = Vec::new();
        while let Ok(e) = r.events.try_recv() {
            if !matches!(e, EngineEvent::Playing { .. }) {
                out.push(e);
            }
        }
        out
    }

    async fn next_event(r: &mut Rig) -> EngineEvent {
        timeout(Duration::from_secs(30), r.events.recv())
            .await
            .expect("no event")
            .expect("events closed")
    }

    async fn wait_link(r: &Rig, up: bool) {
        let mut l = r.session.link();
        timeout(Duration::from_secs(60), l.wait_for(|l| *l == Some(up)))
            .await
            .expect("link")
            .unwrap();
    }

    /// Loads `req` and lets the renderer settle into playing, with the position at `rel_s`.
    async fn playing(r: &mut Rig, req: LoadReq, rel_s: u64) {
        r.session.load(req);
        r.ctl.set_rel_time(Some(Duration::from_secs(rel_s)));
        sleep(Duration::from_secs(4)).await;
        while r.events.try_recv().is_ok() {}
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
        assert_eq!(r.conn.attempts.lock().unwrap().len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn refused_connect_sets_link_false_and_retries() {
        let r = rig();
        r.conn.refuse.store(true, Ordering::SeqCst);
        sleep(Duration::from_millis(3600)).await;
        assert_eq!(*r.session.link().borrow(), Some(false));
        let at: Vec<Duration> = r
            .conn
            .attempts
            .lock()
            .unwrap()
            .iter()
            .map(|t| *t - r.t0)
            .collect();
        let ms = |n| Duration::from_millis(n);
        assert_eq!(at, vec![ms(0), ms(500), ms(1500), ms(3500)]);
        r.conn.refuse.store(false, Ordering::SeqCst);
        wait_link(&r, true).await;
    }

    #[tokio::test(start_paused = true)]
    async fn load_stops_sets_uri_and_plays() {
        let r = rig();
        r.session.load(req(3, 42, None));
        sleep(Duration::from_millis(100)).await;
        assert_eq!(
            ops(&r),
            vec![
                "Stop".to_string(),
                format!("SetAVTransportURI {}", url(3, 42000)),
                "Play".to_string()
            ]
        );
        assert!(r.ctl.last_metadata().contains("Song"));
    }

    #[tokio::test(start_paused = true)]
    async fn public_url_override_used() {
        let ctl = FakeControl::new();
        let r = rig_with(Arc::new(ctl.clone()), ctl, Some("https://pinson.example/"));
        r.session.load(req(3, 0, None));
        sleep(Duration::from_millis(100)).await;
        assert_eq!(
            ops(&r)[1],
            "SetAVTransportURI https://pinson.example/api/upnp/stream/3/song.mp3?fromMs=0"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn sonos_gets_rincon_scheme() {
        let r = rig();
        r.conn.sonos.store(true, Ordering::SeqCst);
        r.session.load(req(3, 0, None));
        sleep(Duration::from_millis(100)).await;
        assert_eq!(
            ops(&r)[1],
            "SetAVTransportURI x-rincon-mp3radio://10.0.0.2:8082/api/upnp/stream/3/song.mp3?fromMs=0"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn load_while_paused_does_not_play_until_resume() {
        let r = rig();
        r.session.load(req(3, 0, None));
        r.session.pause();
        sleep(Duration::from_secs(2)).await;
        assert_eq!(
            ops(&r),
            vec![
                "Stop".to_string(),
                format!("SetAVTransportURI {}", url(3, 0))
            ]
        );
        r.session.resume();
        sleep(Duration::from_millis(100)).await;
        assert_eq!(ops(&r).last().unwrap(), "Play");
        assert_eq!(r.ctl.count("SetAVTransportURI"), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn playing_reports_position() {
        let mut r = rig();
        r.session.load(req(3, 42, None));
        r.ctl.set_rel_time(Some(Duration::from_secs(5)));
        match next_event(&mut r).await {
            EngineEvent::Playing {
                generation,
                position,
                ..
            } => {
                assert_eq!(generation, 3);
                assert_eq!(position, Duration::from_secs(47));
            }
            e => panic!("{e:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn small_drift_is_not_reported_large_drift_reanchors() {
        let mut r = rig();
        r.session.load(req(3, 0, None));
        r.ctl.set_rel_time(Some(Duration::from_secs(5)));
        assert!(
            matches!(next_event(&mut r).await, EngineEvent::Playing { position, .. } if position == Duration::from_secs(5))
        );
        // 0.5 s ahead of the predicted clock at every poll: no report.
        r.ctl.set_rel_time(Some(Duration::from_millis(6500)));
        sleep(Duration::from_millis(1500)).await;
        assert!(r.events.try_recv().is_err());
        // A seek on the device: re-anchored.
        r.ctl.set_rel_time(Some(Duration::from_secs(40)));
        assert!(
            matches!(next_event(&mut r).await, EngineEvent::Playing { position, .. } if position == Duration::from_secs(40))
        );
    }

    #[tokio::test(start_paused = true)]
    async fn stopped_at_end_sends_track_ended_once() {
        let mut r = rig();
        playing(&mut r, req(3, 0, None), 5).await;
        r.ctl.set_state(TransportState::Stopped);
        sleep(Duration::from_secs(10)).await;
        assert_eq!(
            terminal(&mut r),
            vec![EngineEvent::TrackEnded { generation: 3 }]
        );
        assert!(externals(&r).is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn stopped_mid_song_holds_and_resume_reloads() {
        let mut r = rig();
        playing(&mut r, req(3, 0, Some(200)), 30).await;
        r.ctl.set_state(TransportState::Stopped);
        sleep(Duration::from_secs(3)).await;
        assert_eq!(externals(&r), vec![External::Pause]);
        assert!(terminal(&mut r).is_empty());
        r.session.resume();
        sleep(Duration::from_millis(100)).await;
        assert_eq!(r.ctl.count("SetAVTransportURI"), 2);
        assert!(
            r.ctl
                .calls()
                .contains(&format!("SetAVTransportURI {}", url(3, 30000)))
        );
        assert_eq!(ops(&r).last().unwrap(), "Play");
    }

    #[tokio::test(start_paused = true)]
    async fn overdue_song_ends() {
        let mut r = rig();
        r.session.load(req(3, 0, Some(10)));
        // The renderer's clock runs on past the song's duration and never says STOPPED.
        let ctl = r.ctl.clone();
        let clock = tokio::spawn(async move {
            let start = Instant::now();
            loop {
                ctl.set_rel_time(Some(start.elapsed()));
                sleep(Duration::from_millis(100)).await;
            }
        });
        sleep(Duration::from_secs(12)).await;
        assert!(terminal(&mut r).is_empty());
        sleep(Duration::from_secs(3)).await;
        assert_eq!(
            terminal(&mut r),
            vec![EngineEvent::TrackEnded { generation: 3 }]
        );
        sleep(Duration::from_secs(10)).await;
        assert!(terminal(&mut r).is_empty());
        clock.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn set_uri_fault_holds() {
        let mut r = rig();
        r.ctl.fail_next(
            "SetAVTransportURI",
            SoapError::Fault {
                code: Some(716),
                description: "Resource not found".into(),
            },
        );
        r.session.load(req(3, 0, None));
        sleep(Duration::from_secs(3)).await;
        assert_eq!(externals(&r), vec![External::Pause]);
        assert!(terminal(&mut r).is_empty());
        assert_eq!(r.ctl.count("Play"), 0);
        r.session.resume();
        sleep(Duration::from_millis(100)).await;
        assert_eq!(r.ctl.count("SetAVTransportURI"), 2);
        assert_eq!(ops(&r).last().unwrap(), "Play");
    }

    #[tokio::test(start_paused = true)]
    async fn never_playing_holds() {
        let r = rig();
        r.session.load(req(3, 0, None));
        sleep(Duration::from_millis(100)).await;
        r.ctl.set_state(TransportState::Stopped);
        sleep(Duration::from_secs(19)).await;
        assert!(externals(&r).is_empty());
        sleep(Duration::from_secs(2)).await;
        assert_eq!(externals(&r), vec![External::Pause]);
    }

    #[tokio::test(start_paused = true)]
    async fn error_after_playing_reloads_then_fails() {
        let mut r = rig();
        playing(&mut r, req(3, 0, None), 5).await;
        r.ctl.set_error(true);
        sleep(Duration::from_millis(1500)).await;
        assert_eq!(r.ctl.count("SetAVTransportURI"), 2);
        assert!(
            r.ctl
                .calls()
                .contains(&format!("SetAVTransportURI {}", url(3, 5000)))
        );
        assert!(terminal(&mut r).is_empty());
        sleep(Duration::from_secs(10)).await;
        assert_eq!(
            terminal(&mut r),
            vec![EngineEvent::TrackFailed {
                generation: 3,
                reason: "the renderer couldn't play the stream".into()
            }]
        );
        assert_eq!(r.ctl.count("SetAVTransportURI"), 2);
        assert!(externals(&r).is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn error_before_playing_holds() {
        let mut r = rig();
        r.session.load(req(3, 0, None));
        sleep(Duration::from_millis(100)).await;
        r.ctl.set_state(TransportState::Stopped);
        r.ctl.set_error(true);
        sleep(Duration::from_secs(5)).await;
        assert_eq!(externals(&r), vec![External::Pause]);
        assert!(terminal(&mut r).is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn external_pause_and_resume() {
        let mut r = rig();
        playing(&mut r, req(3, 0, None), 5).await;
        r.ctl.set_state(TransportState::Paused);
        sleep(Duration::from_millis(1500)).await;
        assert_eq!(externals(&r), vec![External::Pause]);
        r.ctl.set_state(TransportState::Playing);
        sleep(Duration::from_millis(1500)).await;
        assert_eq!(externals(&r), vec![External::Pause, External::Resume]);
        // Followed, not commanded.
        assert_eq!(r.ctl.count("Pause"), 0);
        assert_eq!(r.ctl.count("Play"), 1);
        assert!(terminal(&mut r).is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn own_pause_is_not_external() {
        let r = rig();
        r.session.load(req(3, 0, None));
        sleep(Duration::from_secs(4)).await;
        r.session.pause();
        sleep(Duration::from_millis(100)).await;
        assert_eq!(ops(&r).last().unwrap(), "Pause");
        // The renderer is slow to pause.
        r.ctl.set_state(TransportState::Playing);
        sleep(Duration::from_secs(1)).await;
        assert!(externals(&r).is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn takeover_pauses_pinson() {
        let mut r = rig();
        playing(&mut r, req(3, 0, None), 5).await;
        r.ctl.set_track_uri("http://other.lan/song.flac");
        sleep(Duration::from_millis(1500)).await;
        assert_eq!(externals(&r), vec![External::Pause]);
        assert!(terminal(&mut r).is_empty());
        // Held: no more polling of it, nothing sent to the renderer.
        sleep(Duration::from_secs(10)).await;
        assert_eq!(externals(&r), vec![External::Pause]);
        assert_eq!(r.ctl.count("Pause"), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn stale_own_uri_is_not_takeover() {
        let mut r = rig();
        playing(&mut r, req(3, 0, None), 5).await;
        r.ctl.set_track_uri(&url(2, 0));
        sleep(Duration::from_secs(5)).await;
        assert!(externals(&r).is_empty());
        assert!(r.events.try_recv().is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn stale_reply_about_earlier_load_is_ignored() {
        let mut r = rig();
        playing(&mut r, req(3, 0, Some(200)), 5).await;
        // A new song: the renderer still reports the old URI and a finished state.
        r.session.load(req(4, 0, Some(200)));
        sleep(Duration::from_millis(100)).await;
        r.ctl.set_track_uri(&url(3, 0));
        r.ctl.set_state(TransportState::Stopped);
        sleep(Duration::from_secs(5)).await;
        assert!(terminal(&mut r).is_empty());
        assert!(externals(&r).is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn link_loss_then_rejoin() {
        let r = rig();
        r.session.load(req(3, 0, None));
        sleep(Duration::from_secs(4)).await;
        r.ctl.set_reachable(false);
        wait_link(&r, false).await;
        sleep(Duration::from_secs(7)).await;
        assert_eq!(*r.session.link().borrow(), Some(false));
        r.ctl.set_reachable(true);
        wait_link(&r, true).await;
        sleep(Duration::from_secs(2)).await;
        assert_eq!(r.ctl.count("SetAVTransportURI"), 1);
        assert_eq!(r.ctl.count("Stop"), 1);
        assert!(externals(&r).is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn reconnect_reloads_when_media_gone() {
        let r = rig();
        r.session.load(req(3, 0, None));
        r.ctl.set_rel_time(Some(Duration::from_secs(5)));
        sleep(Duration::from_secs(4)).await;
        r.ctl.set_reachable(false);
        wait_link(&r, false).await;
        r.ctl.set_track_uri("");
        r.ctl.set_state(TransportState::NoMedia);
        r.ctl.set_reachable(true);
        wait_link(&r, true).await;
        sleep(Duration::from_secs(1)).await;
        assert_eq!(r.ctl.count("SetAVTransportURI"), 2);
        assert!(
            r.ctl
                .calls()
                .contains(&format!("SetAVTransportURI {}", url(3, 5000)))
        );
        assert_eq!(ops(&r).last().unwrap(), "Play");
    }

    #[tokio::test(start_paused = true)]
    async fn load_before_connect_is_sent_once_connected() {
        let r = rig();
        r.conn.refuse.store(true, Ordering::SeqCst);
        r.session.load(req(3, 0, None));
        sleep(Duration::from_secs(2)).await;
        assert!(ops(&r).is_empty());
        r.conn.refuse.store(false, Ordering::SeqCst);
        wait_link(&r, true).await;
        sleep(Duration::from_millis(100)).await;
        assert_eq!(ops(&r).len(), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn stop_then_load_sends_one_stop() {
        let r = rig();
        r.session.load(req(3, 0, None));
        sleep(Duration::from_secs(4)).await;
        r.session.stop();
        r.session.load(req(3, 60, None));
        sleep(Duration::from_millis(100)).await;
        assert_eq!(
            ops(&r),
            vec![
                "Stop".to_string(),
                format!("SetAVTransportURI {}", url(3, 0)),
                "Play".into(),
                "Stop".into(),
                format!("SetAVTransportURI {}", url(3, 60000)),
                "Play".into(),
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn stop_alone_stops_and_forgets() {
        let mut r = rig();
        playing(&mut r, req(3, 0, None), 5).await;
        r.session.stop();
        sleep(Duration::from_secs(10)).await;
        assert_eq!(r.ctl.count("Stop"), 2);
        r.session.resume();
        sleep(Duration::from_secs(1)).await;
        assert_eq!(r.ctl.count("SetAVTransportURI"), 1);
        assert!(terminal(&mut r).is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn set_volume_round_trip() {
        let r = rig();
        wait_link(&r, true).await;
        r.session.set_volume(Some(0.3), Some(true)).await.unwrap();
        let mut vol = r.session.volume();
        let v = *vol.wait_for(|v| v.is_some_and(|v| v.muted)).await.unwrap();
        assert_eq!(
            v,
            Some(Volume {
                level: 0.3,
                muted: true
            })
        );
    }

    #[tokio::test(start_paused = true)]
    async fn set_volume_while_down_errors() {
        let r = rig();
        r.conn.refuse.store(true, Ordering::SeqCst);
        sleep(Duration::from_secs(1)).await;
        assert_eq!(
            r.session.set_volume(Some(0.3), None).await,
            Err("Can't reach the renderer".to_string())
        );
    }

    #[tokio::test(start_paused = true)]
    async fn drop_stops_and_closes() {
        let r = rig();
        r.session.load(req(3, 0, None));
        sleep(Duration::from_secs(1)).await;
        let session = r.session.clone();
        drop(r.guard);
        let mut closed = session.closed();
        timeout(Duration::from_secs(1), closed.wait_for(|c| *c))
            .await
            .expect("closed")
            .unwrap();
        assert_eq!(r.ctl.count("Stop"), 2);
    }

    /// A renderer whose `stop` never answers.
    struct HangingStop(FakeControl);

    #[async_trait]
    impl RendererControl for HangingStop {
        async fn set_uri(&self, u: &str, m: &str) -> Result<(), SoapError> {
            self.0.set_uri(u, m).await
        }
        async fn play(&self) -> Result<(), SoapError> {
            self.0.play().await
        }
        async fn pause(&self) -> Result<(), SoapError> {
            self.0.pause().await
        }
        async fn stop(&self) -> Result<(), SoapError> {
            if self.0.count("SetAVTransportURI") > 0 {
                std::future::pending().await
            }
            self.0.stop().await
        }
        async fn transport_info(&self) -> Result<TransportInfo, SoapError> {
            self.0.transport_info().await
        }
        async fn position_info(&self) -> Result<PositionInfo, SoapError> {
            self.0.position_info().await
        }
        async fn volume(&self) -> Result<Option<Volume>, SoapError> {
            self.0.volume().await
        }
        async fn set_volume(&self, l: Option<f32>, m: Option<bool>) -> Result<(), SoapError> {
            self.0.set_volume(l, m).await
        }
    }

    #[tokio::test(start_paused = true)]
    async fn drop_with_hung_stop_still_closes() {
        let ctl = FakeControl::new();
        let r = rig_with(Arc::new(HangingStop(ctl.clone())), ctl, None);
        r.session.load(req(3, 0, None));
        sleep(Duration::from_secs(1)).await;
        let session = r.session.clone();
        let t = Instant::now();
        drop(r.guard);
        let mut closed = session.closed();
        timeout(Duration::from_millis(2500), closed.wait_for(|c| *c))
            .await
            .expect("closed within budget")
            .unwrap();
        assert!(t.elapsed() <= Duration::from_millis(2100));
        drop(r.session);
    }

    #[tokio::test(start_paused = true)]
    async fn backoff_resets_only_after_a_call_succeeds() {
        let r = rig();
        wait_link(&r, true).await;
        // Connects fine but every call fails: the backoff keeps growing.
        r.ctl.set_reachable(false);
        wait_link(&r, false).await;
        sleep(Duration::from_secs(30)).await;
        let n = r.conn.attempts.lock().unwrap().len();
        assert!(n <= 10, "attempts {n}");
        assert_eq!(*r.session.link().borrow(), Some(false));
    }
}
