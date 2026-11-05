//! The player actor: owns the `Queue` and the `AudioEngine`.
//!
//! The HTTP layer talks to it through a `PlayerHandle` (commands in, state
//! out on a `watch` channel, notices on a `broadcast` channel).

use std::sync::Arc;
use std::time::Duration;

use anyhow::anyhow;
use serde::Serialize;
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::engine::{AudioEngine, EngineEvent};
use crate::queue::{EntryId, PlayNextOutcome, Queue, QueueEntry};
use crate::song::Song;
use crate::source::{PcmReader, PcmSource};

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Status {
    Playing,
    Paused,
    Stopped,
}

/// Time played on the server clock for the current song.
#[derive(Clone, Copy, Debug, Default)]
struct Clock {
    played_before: Duration,
    resumed_at: Option<Instant>,
}

#[derive(Clone, Debug)]
pub struct PlayerState {
    pub current: Option<QueueEntry>,
    pub upcoming: Vec<QueueEntry>,
    pub status: Status,
    pub latency: Duration,
    pub version: u64,
    clock: Clock,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StateView {
    pub current: Option<QueueEntry>,
    pub status: Status,
    pub position_ms: u64,
    pub upcoming: Vec<QueueEntry>,
    pub version: u64,
}

impl Clock {
    fn played(&self, now: Instant) -> Duration {
        self.played_before + self.resumed_at.map_or(Duration::ZERO, |t| now - t)
    }

    fn running(&self) -> bool {
        self.resumed_at.is_some()
    }

    fn pause(&mut self, now: Instant) {
        self.played_before = self.played(now);
        self.resumed_at = None;
    }
}

impl PlayerState {
    pub fn view(&self, now: Instant) -> StateView {
        let position = self.clock.played(now).saturating_sub(self.latency);
        StateView {
            current: self.current.clone(),
            status: self.status,
            position_ms: position.as_millis() as u64,
            upcoming: self.upcoming.clone(),
            version: self.version,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum Notice {
    SongFailed {
        entry_id: EntryId,
        title: String,
        reason: String,
    },
}

enum Command {
    PlayNext(Song, oneshot::Sender<PlayNextOutcome>),
    Remove(EntryId, oneshot::Sender<bool>),
    Pause(oneshot::Sender<()>),
    Resume(oneshot::Sender<()>),
    Skip(Option<EntryId>, oneshot::Sender<bool>),
    Previous(Option<EntryId>, oneshot::Sender<bool>),
}

#[derive(Clone)]
pub struct PlayerHandle {
    commands: mpsc::UnboundedSender<Command>,
    state: watch::Receiver<PlayerState>,
    notices: broadcast::Sender<Notice>,
}

impl PlayerHandle {
    async fn ask<T>(&self, make: impl FnOnce(oneshot::Sender<T>) -> Command) -> anyhow::Result<T> {
        let (tx, rx) = oneshot::channel();
        self.commands
            .send(make(tx))
            .map_err(|_| anyhow!("player has stopped"))?;
        rx.await.map_err(|_| anyhow!("player has stopped"))
    }

    pub async fn play_next(&self, song: Song) -> anyhow::Result<PlayNextOutcome> {
        self.ask(|tx| Command::PlayNext(song, tx)).await
    }
    pub async fn remove(&self, id: EntryId) -> anyhow::Result<bool> {
        self.ask(|tx| Command::Remove(id, tx)).await
    }
    pub async fn pause(&self) -> anyhow::Result<()> {
        self.ask(Command::Pause).await
    }
    pub async fn resume(&self) -> anyhow::Result<()> {
        self.ask(Command::Resume).await
    }
    /// Moves to the next song. With `expected`, only if that entry is still
    /// current, so a skip meant for a song that just ended can't skip the
    /// next one too. Returns whether it acted.
    pub async fn skip(&self, expected: Option<EntryId>) -> anyhow::Result<bool> {
        self.ask(|tx| Command::Skip(expected, tx)).await
    }
    /// Restarts the current song; the queue is untouched.
    /// Restarts the current song; `expected` as for [`skip`](Self::skip).
    pub async fn previous(&self, expected: Option<EntryId>) -> anyhow::Result<bool> {
        self.ask(|tx| Command::Previous(expected, tx)).await
    }
    pub fn state(&self) -> PlayerState {
        self.state.borrow().clone()
    }
    pub fn subscribe(&self) -> watch::Receiver<PlayerState> {
        self.state.clone()
    }
    pub fn notices(&self) -> broadcast::Receiver<Notice> {
        self.notices.subscribe()
    }
}

/// Open the next song's source when the current one has this much left.
pub const PREFETCH_LEAD: Duration = Duration::from_secs(15);

/// A source being opened in the background. Dropping it cancels the open
/// (or drops the reader, killing its processes).
struct Opening {
    entry_id: EntryId,
    task: JoinHandle<anyhow::Result<Box<dyn PcmReader>>>,
}

impl Opening {
    fn spawn(source: &Arc<dyn PcmSource>, entry: &QueueEntry) -> Self {
        let source = source.clone();
        let video_id = entry.song.video_id.clone();
        let task = tokio::spawn(async move { source.open(&video_id).await });
        Self {
            entry_id: entry.id,
            task,
        }
    }
}

impl Drop for Opening {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// The open that will start the current song.
struct Pending {
    generation: u64,
    opening: Opening,
}

struct Opened {
    generation: u64,
    result: anyhow::Result<Box<dyn PcmReader>>,
}

async fn wait_pending(pending: &mut Option<Pending>) -> Opened {
    let Some(p) = pending.as_mut() else {
        return std::future::pending().await;
    };
    let joined = (&mut p.opening.task).await;
    let p = pending.take().expect("pending was Some");
    let result = joined.unwrap_or_else(|e| Err(anyhow!("open task failed: {e}")));
    Opened {
        generation: p.generation,
        result,
    }
}

async fn sleep_until_opt(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

struct Actor {
    queue: Queue,
    engine: Box<dyn AudioEngine>,
    source: Arc<dyn PcmSource>,
    state: watch::Sender<PlayerState>,
    notices: broadcast::Sender<Notice>,
    /// Bumped on every `play`; engine events for older generations are stale.
    generation: u64,
    /// The engine is playing (or paused on) the current generation.
    started: bool,
    /// The engine holds a reader (any generation): it may still be making
    /// sound, so it must be stopped before a slow open.
    engine_busy: bool,
    paused: bool,
    clock: Clock,
    version: u64,
    pending: Option<Pending>,
    prefetch: Option<Opening>,
    prefetch_at: Option<Instant>,
}

enum Wake {
    Command(Option<Command>),
    Engine(EngineEvent),
    Opened(Opened),
    PrefetchDue,
}

impl Actor {
    async fn run(
        mut self,
        mut commands: mpsc::UnboundedReceiver<Command>,
        mut events: mpsc::UnboundedReceiver<EngineEvent>,
    ) {
        loop {
            let wake = tokio::select! {
                cmd = commands.recv() => Wake::Command(cmd),
                Some(ev) = events.recv() => Wake::Engine(ev),
                opened = wait_pending(&mut self.pending) => Wake::Opened(opened),
                () = sleep_until_opt(self.prefetch_at) => Wake::PrefetchDue,
            };
            let now = Instant::now();
            match wake {
                Wake::Command(None) => break,
                Wake::Command(Some(cmd)) => self.on_command(cmd, now),
                Wake::Engine(ev) => self.on_engine_event(ev, now),
                Wake::Opened(opened) => self.on_opened(opened, now),
                Wake::PrefetchDue => self.update_prefetch(now),
            }
        }
        let _ = self.engine.stop();
    }

    fn on_command(&mut self, cmd: Command, now: Instant) {
        match cmd {
            Command::PlayNext(song, reply) => {
                let outcome = self.queue.play_next(song);
                if outcome.started {
                    self.paused = false;
                    self.start_current(now);
                } else {
                    self.update_prefetch(now);
                    self.publish(now);
                }
                let _ = reply.send(outcome);
            }
            Command::Remove(id, reply) => {
                let removed = self.queue.remove(id);
                if removed {
                    self.update_prefetch(now);
                    self.publish(now);
                }
                let _ = reply.send(removed);
            }
            Command::Pause(reply) => {
                if self.queue.current().is_some() && !self.paused {
                    self.paused = true;
                    if self.started {
                        self.engine_call("pause", |e| e.pause());
                        self.clock.pause(now);
                    }
                    self.update_prefetch(now);
                    self.publish(now);
                }
                let _ = reply.send(());
            }
            Command::Resume(reply) => {
                if self.queue.current().is_some() && self.paused {
                    self.paused = false;
                    if self.started {
                        self.engine_call("resume", |e| e.resume());
                        self.clock.resumed_at = Some(now);
                    }
                    self.update_prefetch(now);
                    self.publish(now);
                }
                let _ = reply.send(());
            }
            Command::Skip(expected, reply) => {
                let act = self.is_current(expected);
                if act {
                    self.queue.advance();
                    self.paused = false;
                    self.start_current(now);
                }
                let _ = reply.send(act);
            }
            Command::Previous(expected, reply) => {
                let act = self.is_current(expected);
                if act {
                    self.paused = false;
                    self.start_current(now);
                }
                let _ = reply.send(act);
            }
        }
    }

    /// Something is current and, if the caller named an entry, it's that one.
    fn is_current(&self, expected: Option<EntryId>) -> bool {
        match (self.queue.current(), expected) {
            (None, _) => false,
            (Some(entry), Some(id)) => entry.id == id,
            (Some(_), None) => true,
        }
    }

    fn on_engine_event(&mut self, ev: EngineEvent, now: Instant) {
        match ev {
            EngineEvent::TrackEnded { generation } if generation == self.generation => {
                self.engine_busy = false;
                self.queue.advance();
                self.start_current(now);
            }
            EngineEvent::TrackFailed { generation, reason } if generation == self.generation => {
                self.engine_busy = false;
                self.fail_current(reason, now);
            }
            _ => {} // stale: from a song we already moved away from
        }
    }

    fn on_opened(&mut self, opened: Opened, now: Instant) {
        if opened.generation != self.generation {
            return;
        }
        let reader = match opened.result {
            Ok(reader) => reader,
            Err(e) => return self.fail_current(format!("{e:#}"), now),
        };
        if let Err(e) = self.engine.play(reader, self.generation) {
            return self.fail_current(format!("{e:#}"), now);
        }
        self.started = true;
        self.engine_busy = true;
        self.clock = Clock {
            played_before: Duration::ZERO,
            resumed_at: Some(now),
        };
        if self.paused {
            self.engine_call("pause", |e| e.pause());
            self.clock.pause(now);
        }
        self.update_prefetch(now);
        self.publish(now);
    }

    /// Starts whatever is now `current` (or goes idle).
    fn start_current(&mut self, now: Instant) {
        self.generation += 1;
        self.pending = None;
        self.clock = Clock::default();
        self.started = false;
        match self.queue.current().cloned() {
            None => {
                self.paused = false;
                self.engine_busy = false;
                self.engine_call("stop", |e| e.stop());
            }
            Some(entry) => {
                let opening = match self.prefetch.take() {
                    Some(p) if p.entry_id == entry.id => p,
                    other => {
                        self.prefetch = other;
                        Opening::spawn(&self.source, &entry)
                    }
                };
                // Don't let the old song keep playing while the new one opens.
                if self.engine_busy && !opening.task.is_finished() {
                    self.engine_busy = false;
                    self.engine_call("stop", |e| e.stop());
                }
                self.pending = Some(Pending {
                    generation: self.generation,
                    opening,
                });
            }
        }
        self.update_prefetch(now);
        self.publish(now);
    }

    fn fail_current(&mut self, reason: String, now: Instant) {
        if let Some(entry) = self.queue.current() {
            tracing::warn!(title = %entry.song.title, %reason, "song failed");
            let _ = self.notices.send(Notice::SongFailed {
                entry_id: entry.id,
                title: entry.song.title.clone(),
                reason,
            });
        }
        self.queue.advance();
        self.start_current(now);
    }

    /// Keeps `prefetch` on the head of `upcoming`, opening it once the
    /// current song has `PREFETCH_LEAD` or less left.
    fn update_prefetch(&mut self, now: Instant) {
        self.prefetch_at = None;
        let Some(head) = self.queue.upcoming().front().cloned() else {
            self.prefetch = None;
            return;
        };
        if self
            .prefetch
            .as_ref()
            .is_some_and(|p| p.entry_id != head.id)
        {
            self.prefetch = None;
        }
        if self.prefetch.is_some() {
            return;
        }
        let Some(current) = self.queue.current() else {
            return;
        };
        let due = match current.song.duration_secs {
            None => true,
            Some(secs) => {
                let left = Duration::from_secs(secs.into()).saturating_sub(self.clock.played(now));
                if left > PREFETCH_LEAD && self.clock.running() {
                    self.prefetch_at = Some(now + (left - PREFETCH_LEAD));
                }
                left <= PREFETCH_LEAD
            }
        };
        if due {
            self.prefetch = Some(Opening::spawn(&self.source, &head));
        }
    }

    fn engine_call(
        &mut self,
        what: &str,
        f: impl FnOnce(&mut dyn AudioEngine) -> anyhow::Result<()>,
    ) {
        if let Err(e) = f(self.engine.as_mut()) {
            tracing::warn!("engine {what} failed: {e:#}");
        }
    }

    fn status(&self) -> Status {
        match (self.queue.current(), self.paused) {
            (None, _) => Status::Stopped,
            (Some(_), true) => Status::Paused,
            (Some(_), false) => Status::Playing,
        }
    }

    fn publish(&mut self, _now: Instant) {
        self.version += 1;
        self.state.send_replace(PlayerState {
            current: self.queue.current().cloned(),
            upcoming: self.queue.upcoming().iter().cloned().collect(),
            status: self.status(),
            latency: self.engine.latency(),
            version: self.version,
            clock: self.clock,
        });
    }
}

pub fn spawn_player(
    engine: Box<dyn AudioEngine>,
    events: mpsc::UnboundedReceiver<EngineEvent>,
    source: Arc<dyn PcmSource>,
) -> PlayerHandle {
    let initial = PlayerState {
        current: None,
        upcoming: Vec::new(),
        status: Status::Stopped,
        latency: engine.latency(),
        version: 0,
        clock: Clock::default(),
    };
    let (state_tx, state_rx) = watch::channel(initial);
    let (notices, _) = broadcast::channel(64);
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let actor = Actor {
        queue: Queue::new(),
        engine,
        source,
        state: state_tx,
        notices: notices.clone(),
        generation: 0,
        started: false,
        engine_busy: false,
        paused: false,
        clock: Clock::default(),
        version: 0,
        pending: None,
        prefetch: None,
        prefetch_at: None,
    };
    tokio::spawn(actor.run(cmd_rx, events));
    PlayerHandle {
        commands: cmd_tx,
        state: state_rx,
        notices,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::fake::{FakeEngine, FakeSource};

    struct Rig {
        player: PlayerHandle,
        engine: FakeEngine,
        source: FakeSource,
        events: mpsc::UnboundedSender<EngineEvent>,
    }

    fn rig_with(source: FakeSource, latency: Duration) -> Rig {
        let engine = FakeEngine::new(latency);
        let (tx, rx) = mpsc::unbounded_channel();
        let player = spawn_player(Box::new(engine.clone()), rx, Arc::new(source.clone()));
        Rig {
            player,
            engine,
            source,
            events: tx,
        }
    }

    fn rig() -> Rig {
        rig_with(FakeSource::default(), Duration::ZERO)
    }

    fn song(id: &str, duration_secs: Option<u32>) -> Song {
        Song {
            video_id: id.into(),
            title: id.into(),
            artists: "artist".into(),
            album: None,
            duration_secs,
            artwork_url: None,
        }
    }

    /// Lets spawned tasks run without moving the (paused) clock.
    async fn settle() {
        for _ in 0..100 {
            tokio::task::yield_now().await;
        }
    }

    /// Waits (in virtual time) until `call` has been recorded `n` times;
    /// returns how long that took.
    async fn wait_call_n(engine: &FakeEngine, call: &str, n: usize) -> Duration {
        let start = Instant::now();
        for _ in 0..60_000 {
            settle().await;
            if engine.calls().iter().filter(|c| *c == call).count() >= n {
                return start.elapsed();
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        panic!("{call} ×{n} never happened; calls: {:?}", engine.calls());
    }

    async fn wait_call(engine: &FakeEngine, call: &str) -> Duration {
        wait_call_n(engine, call, 1).await
    }

    fn view(r: &Rig) -> StateView {
        r.player.state().view(Instant::now())
    }

    fn current_id(r: &Rig) -> Option<String> {
        view(r).current.map(|e| e.song.video_id)
    }

    async fn end_track(r: &Rig) {
        let generation = r.engine.generation();
        r.events
            .send(EngineEvent::TrackEnded { generation })
            .unwrap();
        settle().await;
    }

    #[tokio::test(start_paused = true)]
    async fn first_song_plays() {
        let r = rig();
        let out = r.player.play_next(song("A", Some(200))).await.unwrap();
        assert!(out.started);
        wait_call(&r.engine, "play:A").await;
        let v = view(&r);
        assert_eq!(v.status, Status::Playing);
        assert_eq!(v.current.unwrap().id, out.id);
    }

    #[tokio::test(start_paused = true)]
    async fn track_end_advances_and_end_of_queue_stops() {
        let r = rig();
        r.player.play_next(song("A", Some(200))).await.unwrap();
        r.player.play_next(song("B", Some(200))).await.unwrap();
        wait_call(&r.engine, "play:A").await;
        end_track(&r).await;
        wait_call(&r.engine, "play:B").await;
        assert_eq!(current_id(&r).as_deref(), Some("B"));
        end_track(&r).await;
        assert_eq!(r.engine.calls().last().map(String::as_str), Some("stop"));
        let v = view(&r);
        assert_eq!(v.status, Status::Stopped);
        assert!(v.current.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn pause_freezes_position() {
        let r = rig();
        r.player.play_next(song("A", Some(200))).await.unwrap();
        wait_call(&r.engine, "play:A").await;
        let started = Instant::now();
        tokio::time::sleep(Duration::from_secs(3)).await;
        r.player.pause().await.unwrap();
        let paused_at = started.elapsed().as_millis() as u64;
        tokio::time::sleep(Duration::from_secs(5)).await;
        let v = view(&r);
        assert_eq!(v.status, Status::Paused);
        assert_eq!(v.position_ms, paused_at);
        assert_eq!(r.engine.calls().last().map(String::as_str), Some("pause"));
    }

    #[tokio::test(start_paused = true)]
    async fn resume_continues() {
        let r = rig();
        r.player.play_next(song("A", Some(200))).await.unwrap();
        wait_call(&r.engine, "play:A").await;
        tokio::time::sleep(Duration::from_secs(3)).await;
        r.player.pause().await.unwrap();
        tokio::time::sleep(Duration::from_secs(5)).await;
        r.player.resume().await.unwrap();
        tokio::time::sleep(Duration::from_secs(2)).await;
        let v = view(&r);
        assert_eq!(v.status, Status::Playing);
        assert_eq!(v.position_ms, 5000);
        assert_eq!(r.engine.calls().last().map(String::as_str), Some("resume"));
    }

    #[tokio::test(start_paused = true)]
    async fn skip_moves_on() {
        let r = rig();
        r.player.play_next(song("A", Some(200))).await.unwrap();
        r.player.play_next(song("X", Some(200))).await.unwrap();
        wait_call(&r.engine, "play:A").await;
        r.player.skip(None).await.unwrap();
        wait_call(&r.engine, "play:X").await;
        assert_eq!(current_id(&r).as_deref(), Some("X"));
        assert_eq!(view(&r).status, Status::Playing);
    }

    #[tokio::test(start_paused = true)]
    async fn previous_restarts_same_entry() {
        let r = rig();
        let a = r.player.play_next(song("A", Some(200))).await.unwrap();
        r.player.play_next(song("X", Some(200))).await.unwrap();
        wait_call(&r.engine, "play:A").await;
        tokio::time::sleep(Duration::from_secs(30)).await;
        r.player.previous(None).await.unwrap();
        wait_call_n(&r.engine, "play:A", 2).await;
        let v = view(&r);
        assert_eq!(v.current.unwrap().id, a.id);
        assert_eq!(v.position_ms, 0);
        assert_eq!(v.upcoming.len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn transition_uses_prefetched_source() {
        let source = FakeSource {
            delay: Duration::from_secs(2),
            ..FakeSource::default()
        };
        let r = rig_with(source, Duration::ZERO);
        r.player.play_next(song("A", Some(30))).await.unwrap();
        wait_call(&r.engine, "play:A").await;
        r.player.play_next(song("X", Some(30))).await.unwrap();
        tokio::time::sleep(Duration::from_secs(20)).await;
        assert_eq!(r.source.opens(), ["A", "X"], "X is prefetched at 15 s left");
        end_track(&r).await;
        let took = wait_call(&r.engine, "play:X").await;
        assert!(took <= Duration::from_millis(10), "switch took {took:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn prefetch_follows_queue_head() {
        let source = FakeSource {
            delay: Duration::from_secs(2),
            ..FakeSource::default()
        };
        let r = rig_with(source, Duration::ZERO);
        r.player.play_next(song("A", Some(30))).await.unwrap();
        wait_call(&r.engine, "play:A").await;
        let x = r.player.play_next(song("X", Some(30))).await.unwrap();
        r.player.play_next(song("Y", Some(30))).await.unwrap();
        // 15 s left at t = 15 s: the prefetch of X starts; it's still opening at 16 s.
        tokio::time::sleep(Duration::from_secs(16)).await;
        assert_eq!(r.source.opens(), ["A", "X"]);
        // Play next keeps X at the head, so change the head by removing X.
        assert!(r.player.remove(x.id).await.unwrap());
        settle().await;
        assert_eq!(
            r.source.opens(),
            ["A", "X", "Y"],
            "Y is prefetched as the new head"
        );
        tokio::time::sleep(Duration::from_secs(5)).await;
        end_track(&r).await;
        let took = wait_call(&r.engine, "play:Y").await;
        assert!(took <= Duration::from_millis(10), "switch took {took:?}");
        assert!(!r.engine.calls().contains(&"play:X".to_owned()));
    }

    #[tokio::test(start_paused = true)]
    async fn failed_song_is_skipped_with_notice() {
        let source = FakeSource {
            failing: ["X".to_owned()].into(),
            ..FakeSource::default()
        };
        let r = rig_with(source, Duration::ZERO);
        let mut notices = r.player.notices();
        r.player.play_next(song("A", Some(200))).await.unwrap();
        let x = r.player.play_next(song("X", Some(200))).await.unwrap();
        r.player.play_next(song("Y", Some(200))).await.unwrap();
        wait_call(&r.engine, "play:A").await;
        end_track(&r).await;
        wait_call(&r.engine, "play:Y").await;
        let Notice::SongFailed {
            entry_id,
            title,
            reason,
        } = notices.try_recv().unwrap();
        assert_eq!(entry_id, x.id);
        assert_eq!(title, "X");
        assert!(reason.contains("Video unavailable"), "{reason}");
        assert_eq!(current_id(&r).as_deref(), Some("Y"));
    }

    #[tokio::test(start_paused = true)]
    async fn all_failing_songs_end_in_stopped() {
        let failing = ["A", "B", "C"].map(String::from).into();
        let source = FakeSource {
            failing,
            ..FakeSource::default()
        };
        let r = rig_with(source, Duration::ZERO);
        let mut notices = r.player.notices();
        r.player.play_next(song("A", Some(200))).await.unwrap();
        r.player.play_next(song("B", Some(200))).await.unwrap();
        r.player.play_next(song("C", Some(200))).await.unwrap();
        tokio::time::sleep(Duration::from_secs(1)).await;
        settle().await;
        assert_eq!(view(&r).status, Status::Stopped);
        for _ in 0..3 {
            assert!(notices.try_recv().is_ok());
        }
        assert!(notices.try_recv().is_err());
        assert!(!r.engine.calls().iter().any(|c| c.starts_with("play:")));
        assert_eq!(r.source.opens(), ["A", "B", "C"]);
    }

    #[tokio::test(start_paused = true)]
    async fn track_ended_after_skip_is_ignored() {
        let r = rig();
        r.player.play_next(song("A", Some(200))).await.unwrap();
        r.player.play_next(song("X", Some(200))).await.unwrap();
        r.player.play_next(song("Y", Some(200))).await.unwrap();
        wait_call(&r.engine, "play:A").await;
        let stale = r.engine.generation();
        r.player.skip(None).await.unwrap();
        wait_call(&r.engine, "play:X").await;
        r.events
            .send(EngineEvent::TrackEnded { generation: stale })
            .unwrap();
        settle().await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(current_id(&r).as_deref(), Some("X"));
        assert!(!r.engine.calls().contains(&"play:Y".to_owned()));
    }

    #[tokio::test(start_paused = true)]
    async fn skip_for_a_song_that_already_ended_is_ignored() {
        let r = rig();
        let a = r.player.play_next(song("A", Some(200))).await.unwrap();
        r.player.play_next(song("X", Some(200))).await.unwrap();
        wait_call(&r.engine, "play:A").await;
        // A ends naturally just before a phone's skip (meant for A) arrives.
        end_track(&r).await;
        wait_call(&r.engine, "play:X").await;
        assert!(!r.player.skip(Some(a.id)).await.unwrap());
        settle().await;
        assert_eq!(current_id(&r).as_deref(), Some("X"));
        assert!(!r.player.previous(Some(a.id)).await.unwrap());
        assert_eq!(
            r.engine.calls().iter().filter(|c| *c == "play:X").count(),
            1,
            "previous for A must not restart X"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn skip_onto_a_failed_prefetch_stops_the_old_song() {
        let source = FakeSource {
            delay: Duration::from_secs(1),
            failing: ["X".to_owned()].into(),
            ..FakeSource::default()
        };
        let r = rig_with(source, Duration::ZERO);
        r.player.play_next(song("A", Some(20))).await.unwrap();
        wait_call(&r.engine, "play:A").await;
        r.player.play_next(song("X", Some(200))).await.unwrap();
        r.player.play_next(song("Y", Some(200))).await.unwrap();
        // X's prefetch starts at 15 s left and has failed by 10 s in.
        tokio::time::sleep(Duration::from_secs(10)).await;
        assert_eq!(r.source.opens(), ["A", "X"]);
        r.player.skip(None).await.unwrap();
        settle().await;
        assert_eq!(
            r.engine.calls(),
            ["play:A", "stop"],
            "A stops while Y opens"
        );
        wait_call(&r.engine, "play:Y").await;
    }

    #[tokio::test(start_paused = true)]
    async fn position_subtracts_latency() {
        let r = rig_with(FakeSource::default(), Duration::from_millis(750));
        r.player.play_next(song("A", Some(200))).await.unwrap();
        wait_call(&r.engine, "play:A").await;
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert_eq!(view(&r).position_ms, 4250);
    }

    #[tokio::test(start_paused = true)]
    async fn state_changes_bump_version() {
        let r = rig();
        let mut rx = r.player.subscribe();
        let v0 = rx.borrow_and_update().version;
        r.player.play_next(song("A", Some(200))).await.unwrap();
        rx.changed().await.unwrap();
        assert!(rx.borrow().version > v0);
    }
}
