//! The player actor: owns the `Queue` and the `AudioEngine`.
//!
//! The HTTP layer talks to it through a `PlayerHandle` (commands in, state
//! out on a `watch` channel, notices on a `broadcast` channel).

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use anyhow::anyhow;
use serde::Serialize;
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::autoplay::{RECENT_FILTER, pick_suggestions};
use crate::engine::{AudioEngine, EngineEvent};
use crate::history::History;
use crate::queue::{AddAt, AddOutcome, EntryId, PlayNextOutcome, Queue, QueueEntry};
use crate::radio::Radio;
use crate::song::Song;
use crate::source::{PcmReader, PcmSource};

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Status {
    Playing,
    Paused,
    Stopped,
}

/// A song counts as played once it has been heard this long. Songs shorter
/// than this (or of unknown length) count when they end instead; for longer
/// songs, seeking to the end does not count.
pub const PLAYED_AFTER: Duration = Duration::from_secs(30);

/// Time played on the server clock for the current song.
#[derive(Clone, Copy, Debug, Default)]
struct Clock {
    /// Where in the song the engine's reader started (non-zero after a seek).
    offset: Duration,
    played_before: Duration,
    resumed_at: Option<Instant>,
}

/// Autoplay suggestions as shown to clients.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AutoplayView {
    /// Mirrors the switch, even when nothing is playing.
    pub enabled: bool,
    /// The song the suggestions come from.
    pub based_on: Option<Song>,
    pub songs: Vec<Song>,
}

/// What the player needs to suggest songs.
pub struct Autoplay {
    pub radio: Arc<dyn Radio>,
    pub history: History,
    pub enabled: bool,
}

#[derive(Clone, Debug)]
pub struct PlayerState {
    pub current: Option<QueueEntry>,
    pub upcoming: Vec<QueueEntry>,
    pub status: Status,
    pub latency: Duration,
    pub version: u64,
    pub autoplay: AutoplayView,
    clock: Clock,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StateView {
    pub current: Option<QueueEntry>,
    pub status: Status,
    pub position_ms: u64,
    /// Whether `position_ms` is advancing: playing and past the speaker latency.
    pub position_running: bool,
    pub upcoming: Vec<QueueEntry>,
    pub version: u64,
    pub autoplay: AutoplayView,
}

impl Clock {
    /// How much the engine has played since its reader started.
    fn played(&self, now: Instant) -> Duration {
        self.played_before + self.resumed_at.map_or(Duration::ZERO, |t| now - t)
    }

    /// How far into the song the engine has written.
    fn song_time(&self, now: Instant) -> Duration {
        self.offset + self.played(now)
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
        let position = self.clock.offset + self.clock.played(now).saturating_sub(self.latency);
        StateView {
            current: self.current.clone(),
            status: self.status,
            position_ms: position.as_millis() as u64,
            position_running: self.clock.running() && self.clock.played(now) >= self.latency,
            upcoming: self.upcoming.clone(),
            version: self.version,
            autoplay: self.autoplay.clone(),
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
    /// The autoplay radio could not be fetched. Sent once until a fetch
    /// succeeds again.
    AutoplayFailed,
}

enum Command {
    PlayNext(Song, oneshot::Sender<PlayNextOutcome>),
    PlayAll(Vec<Song>, oneshot::Sender<usize>),
    Add(Vec<Song>, AddAt, oneshot::Sender<AddOutcome>),
    Remove(EntryId, oneshot::Sender<bool>),
    Pause(oneshot::Sender<()>),
    Resume(oneshot::Sender<()>),
    Skip(Option<EntryId>, oneshot::Sender<bool>),
    Previous(Option<EntryId>, oneshot::Sender<bool>),
    Seek(Duration, Option<EntryId>, oneshot::Sender<bool>),
    SetAutoplay(bool, oneshot::Sender<()>),
    PlaySuggestion(String, oneshot::Sender<bool>),
}

#[derive(Clone)]
pub struct PlayerHandle {
    commands: mpsc::UnboundedSender<Command>,
    state: watch::Receiver<PlayerState>,
    notices: broadcast::Sender<Notice>,
    plays: broadcast::Sender<Song>,
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
    /// Replaces the whole queue with `songs` and starts the first one at once
    /// (not paused); the song that was playing stops. Returns how many songs
    /// were queued; an empty list changes nothing and returns 0.
    pub async fn play_all(&self, songs: Vec<Song>) -> anyhow::Result<usize> {
        self.ask(|tx| Command::PlayAll(songs, tx)).await
    }
    /// Adds songs after the current one (`Next`) or at the end; starts the
    /// first one at once when nothing was playing.
    pub async fn add(&self, songs: Vec<Song>, at: AddAt) -> anyhow::Result<AddOutcome> {
        self.ask(|tx| Command::Add(songs, at, tx)).await
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
    /// Restarts the current song; `expected` as for [`skip`](Self::skip).
    pub async fn previous(&self, expected: Option<EntryId>) -> anyhow::Result<bool> {
        self.ask(|tx| Command::Previous(expected, tx)).await
    }
    /// Restarts the current song `position` into it, keeping play/pause;
    /// past the end it lands on the last second. `expected` as for
    /// [`skip`](Self::skip).
    pub async fn seek(
        &self,
        position: Duration,
        expected: Option<EntryId>,
    ) -> anyhow::Result<bool> {
        self.ask(|tx| Command::Seek(position, expected, tx)).await
    }
    /// Turns autoplay suggestions on or off (on fetches them again).
    pub async fn set_autoplay(&self, on: bool) -> anyhow::Result<()> {
        self.ask(|tx| Command::SetAutoplay(on, tx)).await
    }
    /// Plays a suggested song now, keeping the queue. `false` (nothing
    /// changes) unless it is among the current suggestions.
    pub async fn play_suggestion(&self, video_id: String) -> anyhow::Result<bool> {
        self.ask(|tx| Command::PlaySuggestion(video_id, tx)).await
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
    /// Songs as they count as played (see [`PLAYED_AFTER`]). Server-side only:
    /// never forwarded to clients.
    pub fn plays(&self) -> broadcast::Receiver<Song> {
        self.plays.subscribe()
    }
}

/// Open the next song's source when the current one has this much left.
pub const PREFETCH_LEAD: Duration = Duration::from_secs(15);

/// A source being opened in the background. Dropping it cancels the open
/// (or drops the reader, killing its processes).
struct Opening {
    task: JoinHandle<anyhow::Result<Box<dyn PcmReader>>>,
}

impl Opening {
    fn spawn(source: &Arc<dyn PcmSource>, video_id: &str, start: Duration) -> Self {
        let source = source.clone();
        let video_id = video_id.to_owned();
        let task = tokio::spawn(async move { source.open(&video_id, start).await });
        Self { task }
    }
}

/// What a prefetched source is for.
#[derive(Clone, Debug, PartialEq)]
enum PrefetchFor {
    /// A queued entry.
    Entry(EntryId),
    /// The first autoplay suggestion (by video id), not queued yet.
    Suggestion(String),
}

/// A source opened ahead of time for the song expected to play next.
struct Prefetch {
    target: PrefetchFor,
    opening: Opening,
}

impl Drop for Opening {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// The open that will start the current song.
struct Pending {
    generation: u64,
    /// Where in the song the reader starts.
    start: Duration,
    opening: Opening,
}

struct Opened {
    generation: u64,
    start: Duration,
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
        start: p.start,
        result,
    }
}

/// The radio fetch for one seed. Dropping it cancels the fetch.
struct RadioFetch {
    seed: EntryId,
    seed_song: Song,
    task: JoinHandle<anyhow::Result<(Vec<Song>, HashSet<String>)>>,
}

impl RadioFetch {
    /// Fetches the radio for `seed`, then the recent history (blocking
    /// SQLite read off the actor and off the async workers).
    fn spawn(radio: &Arc<dyn Radio>, history: &History, seed: &QueueEntry) -> Self {
        let radio = radio.clone();
        let history = history.clone();
        let video_id = seed.song.video_id.clone();
        let task = tokio::spawn(async move {
            let songs = radio.radio(&video_id).await?;
            let recent =
                match tokio::task::spawn_blocking(move || history.recent(RECENT_FILTER)).await {
                    Ok(Ok(songs)) => songs.into_iter().map(|s| s.video_id).collect(),
                    Ok(Err(e)) => {
                        tracing::warn!("reading history for autoplay failed: {e:#}");
                        HashSet::new()
                    }
                    Err(e) => {
                        tracing::warn!("history task failed: {e}");
                        HashSet::new()
                    }
                };
            Ok((songs, recent))
        });
        Self {
            seed: seed.id,
            seed_song: seed.song.clone(),
            task,
        }
    }
}

impl Drop for RadioFetch {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct RadioLoaded {
    seed: EntryId,
    seed_song: Song,
    result: anyhow::Result<(Vec<Song>, HashSet<String>)>,
}

async fn wait_radio(fetch: &mut Option<RadioFetch>) -> RadioLoaded {
    let Some(f) = fetch.as_mut() else {
        return std::future::pending().await;
    };
    let joined = (&mut f.task).await;
    let f = fetch.take().expect("fetch was Some");
    let result = joined.unwrap_or_else(|e| Err(anyhow!("radio task failed: {e}")));
    RadioLoaded {
        seed: f.seed,
        seed_song: f.seed_song.clone(),
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
    plays: broadcast::Sender<Song>,
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
    prefetch: Option<Prefetch>,
    prefetch_at: Option<Instant>,
    /// When the position starts moving (the clock reaches the latency).
    position_at: Option<Instant>,
    /// The current song has already been reported on `plays` (cleared when it
    /// restarts from zero; a seek elsewhere keeps it).
    played_recorded: bool,
    /// When the current song will have been heard long enough.
    played_at: Option<Instant>,
    autoplay_on: bool,
    radio: Arc<dyn Radio>,
    history: History,
    /// The queue entry the suggestions are (being) fetched for.
    seed: Option<EntryId>,
    /// The last radio fetched and the recent history it was filtered with.
    radio_songs: Vec<Song>,
    recent: HashSet<String>,
    based_on: Option<Song>,
    fetch: Option<RadioFetch>,
    /// A failure notice was sent and no fetch has succeeded since.
    failure_shown: bool,
    /// The current entry when the last fetch failed; once another song is
    /// current, the same seed is fetched again.
    failed_during: Option<EntryId>,
}

enum Wake {
    Command(Option<Command>),
    Engine(EngineEvent),
    Opened(Opened),
    PrefetchDue,
    PositionDue,
    PlayedDue,
    RadioLoaded(RadioLoaded),
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
                () = sleep_until_opt(self.position_at) => Wake::PositionDue,
                () = sleep_until_opt(self.played_at) => Wake::PlayedDue,
                loaded = wait_radio(&mut self.fetch) => Wake::RadioLoaded(loaded),
            };
            let now = Instant::now();
            match wake {
                Wake::Command(None) => break,
                Wake::Command(Some(cmd)) => self.on_command(cmd, now),
                Wake::Engine(ev) => self.on_engine_event(ev, now),
                Wake::Opened(opened) => self.on_opened(opened, now),
                Wake::PrefetchDue => self.update_prefetch(now),
                Wake::PositionDue => self.publish(now),
                Wake::PlayedDue => self.update_played(now),
                Wake::RadioLoaded(loaded) => self.on_radio_loaded(loaded, now),
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
            Command::PlayAll(songs, reply) => {
                let count = songs.len();
                if self.queue.play_all(songs).is_some() {
                    self.paused = false;
                    self.start_current(now);
                }
                let _ = reply.send(count);
            }
            Command::Add(songs, at, reply) => {
                let outcome = self.queue.add_many(songs, at);
                if outcome.started {
                    self.paused = false;
                    self.start_current(now);
                } else if outcome.count > 0 {
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
                    self.advance();
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
            Command::Seek(position, expected, reply) => {
                let act = self.is_current(expected);
                if act {
                    let duration = self.queue.current().and_then(|e| e.song.duration_secs);
                    let last_second = duration.map(|secs| {
                        Duration::from_secs(secs.into()).saturating_sub(Duration::from_secs(1))
                    });
                    let start = last_second.map_or(position, |last| position.min(last));
                    self.start_current_at(start, now);
                }
                let _ = reply.send(act);
            }
            Command::SetAutoplay(on, reply) => {
                self.autoplay_on = on;
                if on {
                    self.seed = None;
                } else {
                    self.clear_autoplay();
                }
                self.update_prefetch(now);
                self.publish(now);
                let _ = reply.send(());
            }
            Command::PlaySuggestion(video_id, reply) => {
                let song = self
                    .suggestions()
                    .into_iter()
                    .find(|s| s.video_id == video_id);
                let found = song.is_some();
                if let Some(song) = song {
                    self.queue.replace_current(song);
                    if let Some(at) = self.radio_songs.iter().position(|s| s.video_id == video_id) {
                        self.radio_songs.drain(..=at);
                    }
                    self.paused = false;
                    self.start_current(now);
                }
                let _ = reply.send(found);
            }
        }
    }

    fn clear_autoplay(&mut self) {
        self.seed = None;
        self.radio_songs.clear();
        self.recent.clear();
        self.based_on = None;
        self.fetch = None;
        self.failed_during = None;
    }

    /// Starts a fetch when autoplay is on and the seed (the last upcoming
    /// song, else the current one) changed, or a failed fetch can be retried
    /// because another song is now current; clears everything when off or idle.
    fn refresh_autoplay(&mut self) {
        let Some(current_id) = self.queue.current().map(|e| e.id) else {
            self.clear_autoplay();
            return;
        };
        if !self.autoplay_on {
            self.clear_autoplay();
            return;
        }
        let seed = self
            .queue
            .upcoming()
            .back()
            .or(self.queue.current())
            .cloned()
            .expect("current is Some");
        let retry = self.failed_during.is_some_and(|id| id != current_id);
        if self.seed != Some(seed.id) || retry {
            self.seed = Some(seed.id);
            self.failed_during = None;
            self.fetch = Some(RadioFetch::spawn(&self.radio, &self.history, &seed));
        }
    }

    fn on_radio_loaded(&mut self, loaded: RadioLoaded, now: Instant) {
        if self.seed != Some(loaded.seed) {
            return;
        }
        match loaded.result {
            Ok((songs, recent)) => {
                self.radio_songs = songs;
                self.recent = recent;
                self.based_on = Some(loaded.seed_song);
                self.failure_shown = false;
                self.failed_during = None;
            }
            Err(e) => {
                tracing::warn!("autoplay radio failed: {e:#}");
                self.radio_songs.clear();
                self.recent.clear();
                self.based_on = None;
                self.failed_during = self.queue.current().map(|c| c.id);
                if !self.failure_shown {
                    self.failure_shown = true;
                    let _ = self.notices.send(Notice::AutoplayFailed);
                }
            }
        }
        self.update_prefetch(now);
        self.publish(now);
    }

    /// The radio without the seed, anything queued and recent plays.
    fn suggestions(&self) -> Vec<Song> {
        let mut exclude = self.recent.clone();
        exclude.extend(self.based_on.iter().map(|s| s.video_id.clone()));
        exclude.extend(self.queue.current().map(|e| e.song.video_id.clone()));
        exclude.extend(
            self.queue
                .upcoming()
                .iter()
                .map(|e| e.song.video_id.clone()),
        );
        pick_suggestions(&self.radio_songs, &exclude)
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
                let short = self.queue.current().is_some_and(|e| {
                    e.song
                        .duration_secs
                        .is_none_or(|d| u64::from(d) < PLAYED_AFTER.as_secs())
                });
                if !self.played_recorded && short {
                    self.record_play();
                }
                self.advance();
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
            offset: opened.start,
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
        self.start_current_at(Duration::ZERO, now);
    }

    /// Starts `current` `start` into the song (or goes idle).
    fn start_current_at(&mut self, start: Duration, now: Instant) {
        self.generation += 1;
        self.pending = None;
        // Until the reader opens, the position shows where it will start.
        self.clock = Clock {
            offset: start,
            ..Clock::default()
        };
        self.started = false;
        if start.is_zero() {
            self.played_recorded = false;
        }
        match self.queue.current().cloned() {
            None => {
                self.paused = false;
                self.engine_busy = false;
                self.engine_call("stop", |e| e.stop());
            }
            Some(entry) => {
                // A prefetched reader starts at the top of the song.
                let opening = match self.prefetch.take() {
                    Some(p) if p.target == PrefetchFor::Entry(entry.id) && start.is_zero() => {
                        p.opening
                    }
                    other => {
                        self.prefetch = other;
                        Opening::spawn(&self.source, &entry.song.video_id, start)
                    }
                };
                // Don't let the old song keep playing while the new one opens.
                if self.engine_busy && !opening.task.is_finished() {
                    self.engine_busy = false;
                    self.engine_call("stop", |e| e.stop());
                }
                self.pending = Some(Pending {
                    generation: self.generation,
                    start,
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
        self.advance();
        self.start_current(now);
    }

    /// Moves on to the next song. With nothing upcoming and autoplay on,
    /// the first suggestion is queued first, so it plays like any other song
    /// (and uses its prefetched source).
    fn advance(&mut self) {
        if self.autoplay_on
            && self.queue.upcoming().is_empty()
            && let Some(song) = self.suggestions().into_iter().next()
        {
            let video_id = song.video_id.clone();
            self.queue.add_many(vec![song], AddAt::End);
            self.rekey_prefetch(&video_id);
        }
        self.queue.advance();
    }

    /// Points a prefetch made for suggestion `video_id` at the entry it was
    /// just queued as (the last upcoming one).
    fn rekey_prefetch(&mut self, video_id: &str) {
        let Some(id) = self.queue.upcoming().back().map(|e| e.id) else {
            return;
        };
        if let Some(p) = self.prefetch.as_mut()
            && p.target == PrefetchFor::Suggestion(video_id.to_owned())
        {
            p.target = PrefetchFor::Entry(id);
        }
    }

    /// The song expected next: the head of `upcoming`, else (autoplay on)
    /// the first suggestion.
    fn prefetch_target(&self) -> Option<(PrefetchFor, String)> {
        if let Some(head) = self.queue.upcoming().front() {
            return Some((PrefetchFor::Entry(head.id), head.song.video_id.clone()));
        }
        if !self.autoplay_on {
            return None;
        }
        let first = self.suggestions().into_iter().next()?;
        Some((
            PrefetchFor::Suggestion(first.video_id.clone()),
            first.video_id,
        ))
    }

    /// Keeps `prefetch` on the next song, opening it once the current song
    /// has `PREFETCH_LEAD` or less left.
    fn update_prefetch(&mut self, now: Instant) {
        self.prefetch_at = None;
        let Some((target, video_id)) = self.prefetch_target() else {
            self.prefetch = None;
            return;
        };
        if self.prefetch.as_ref().is_some_and(|p| p.target != target) {
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
                let left =
                    Duration::from_secs(secs.into()).saturating_sub(self.clock.song_time(now));
                if left > PREFETCH_LEAD && self.clock.running() {
                    self.prefetch_at = Some(now + (left - PREFETCH_LEAD));
                }
                left <= PREFETCH_LEAD
            }
        };
        if due {
            self.prefetch = Some(Prefetch {
                target,
                opening: Opening::spawn(&self.source, &video_id, Duration::ZERO),
            });
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

    /// Reports the current song on `plays` and won't report it again until
    /// it restarts from zero.
    fn record_play(&mut self) {
        self.played_recorded = true;
        self.played_at = None;
        if let Some(entry) = self.queue.current() {
            let _ = self.plays.send(entry.song.clone());
        }
    }

    /// Records the current song once it has been heard for [`PLAYED_AFTER`]
    /// (its whole length, if shorter), else schedules that moment. Paused
    /// time doesn't count.
    fn update_played(&mut self, now: Instant) {
        self.played_at = None;
        let Some(entry) = self.queue.current() else {
            return;
        };
        if !self.started || self.played_recorded || !self.clock.running() {
            return;
        }
        let need = entry.song.duration_secs.map_or(PLAYED_AFTER, |secs| {
            PLAYED_AFTER.min(Duration::from_secs(secs.into()))
        });
        let played = self.clock.played(now);
        if played >= need {
            self.record_play();
        } else {
            self.played_at = Some(now + (need - played));
        }
    }

    fn publish(&mut self, now: Instant) {
        self.refresh_autoplay();
        // One extra publish when the clock reaches the latency, so clients
        // learn the position has started moving.
        let latency = self.engine.latency();
        let played = self.clock.played(now);
        self.position_at =
            (self.clock.running() && played < latency).then(|| now + (latency - played));
        self.update_played(now);
        self.version += 1;
        self.state.send_replace(PlayerState {
            current: self.queue.current().cloned(),
            upcoming: self.queue.upcoming().iter().cloned().collect(),
            status: self.status(),
            latency: self.engine.latency(),
            version: self.version,
            autoplay: AutoplayView {
                enabled: self.autoplay_on,
                based_on: self.based_on.clone(),
                songs: self.suggestions(),
            },
            clock: self.clock,
        });
    }
}

pub fn spawn_player(
    engine: Box<dyn AudioEngine>,
    events: mpsc::UnboundedReceiver<EngineEvent>,
    source: Arc<dyn PcmSource>,
    autoplay: Autoplay,
) -> PlayerHandle {
    let initial = PlayerState {
        current: None,
        upcoming: Vec::new(),
        status: Status::Stopped,
        latency: engine.latency(),
        version: 0,
        autoplay: AutoplayView {
            enabled: autoplay.enabled,
            ..AutoplayView::default()
        },
        clock: Clock::default(),
    };
    let (state_tx, state_rx) = watch::channel(initial);
    let (notices, _) = broadcast::channel(64);
    let (plays, _) = broadcast::channel(64);
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let actor = Actor {
        queue: Queue::new(),
        engine,
        source,
        state: state_tx,
        notices: notices.clone(),
        plays: plays.clone(),
        generation: 0,
        started: false,
        engine_busy: false,
        paused: false,
        clock: Clock::default(),
        version: 0,
        pending: None,
        prefetch: None,
        prefetch_at: None,
        position_at: None,
        played_recorded: false,
        played_at: None,
        autoplay_on: autoplay.enabled,
        radio: autoplay.radio,
        history: autoplay.history,
        seed: None,
        radio_songs: Vec::new(),
        recent: HashSet::new(),
        based_on: None,
        fetch: None,
        failure_shown: false,
        failed_during: None,
    };
    tokio::spawn(actor.run(cmd_rx, events));
    PlayerHandle {
        commands: cmd_tx,
        state: state_rx,
        notices,
        plays,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::engine::fake::{FakeEngine, FakeSource};
    use crate::radio::fake::FakeRadio;

    struct Rig {
        player: PlayerHandle,
        engine: FakeEngine,
        source: FakeSource,
        events: mpsc::UnboundedSender<EngineEvent>,
        radio: FakeRadio,
        history: History,
    }

    fn rig_built(source: FakeSource, latency: Duration, radio: FakeRadio, enabled: bool) -> Rig {
        let engine = FakeEngine::new(latency);
        let (tx, rx) = mpsc::unbounded_channel();
        let history = History::new(Db::in_memory());
        let player = spawn_player(
            Box::new(engine.clone()),
            rx,
            Arc::new(source.clone()),
            Autoplay {
                radio: Arc::new(radio.clone()),
                history: history.clone(),
                enabled,
            },
        );
        Rig {
            player,
            engine,
            source,
            events: tx,
            radio,
            history,
        }
    }

    fn rig_with(source: FakeSource, latency: Duration) -> Rig {
        rig_built(source, latency, FakeRadio::default(), false)
    }

    fn rig() -> Rig {
        rig_with(FakeSource::default(), Duration::ZERO)
    }

    fn rig_autoplay(radio: FakeRadio) -> Rig {
        rig_built(FakeSource::default(), Duration::ZERO, radio, true)
    }

    fn suggested(r: &Rig) -> Vec<String> {
        view(r)
            .autoplay
            .songs
            .into_iter()
            .map(|s| s.video_id)
            .collect()
    }

    /// Lets the radio fetch (a spawned task plus a blocking history read) finish.
    async fn quiesce() {
        for _ in 0..50 {
            settle().await;
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }

    fn radio_song(id: &str) -> Song {
        song(id, Some(200))
    }

    fn drain_notices(rx: &mut broadcast::Receiver<Notice>) -> usize {
        let mut n = 0;
        while let Ok(notice) = rx.try_recv() {
            if matches!(notice, Notice::AutoplayFailed) {
                n += 1;
            }
        }
        n
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
    async fn seek_reopens_current_at_offset() {
        let r = rig();
        let a = r.player.play_next(song("A", Some(200))).await.unwrap();
        r.player
            .add(vec![song("X", Some(200))], AddAt::End)
            .await
            .unwrap();
        wait_call(&r.engine, "play:A").await;
        tokio::time::sleep(Duration::from_secs(10)).await;
        assert!(r.player.seek(Duration::from_secs(60), None).await.unwrap());
        wait_call_n(&r.engine, "play:A", 2).await;
        assert_eq!(
            r.source.starts().last(),
            Some(&("A".to_owned(), Duration::from_secs(60)))
        );
        let v = view(&r);
        assert_eq!(v.current.unwrap().id, a.id);
        assert_eq!(v.status, Status::Playing);
        assert_eq!(v.position_ms, 60_000);
        assert_eq!(v.upcoming.len(), 1, "the queue is untouched");
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert_eq!(view(&r).position_ms, 65_000);
    }

    #[tokio::test(start_paused = true)]
    async fn seek_while_paused_stays_paused() {
        let r = rig();
        r.player.play_next(song("A", Some(200))).await.unwrap();
        wait_call(&r.engine, "play:A").await;
        r.player.pause().await.unwrap();
        r.player.seek(Duration::from_secs(30), None).await.unwrap();
        wait_call_n(&r.engine, "play:A", 2).await;
        tokio::time::sleep(Duration::from_secs(5)).await;
        let v = view(&r);
        assert_eq!(v.status, Status::Paused);
        assert_eq!(v.position_ms, 30_000);
        assert_eq!(r.engine.calls().last().map(String::as_str), Some("pause"));
    }

    #[tokio::test(start_paused = true)]
    async fn seek_shows_target_while_reopening() {
        let source = FakeSource {
            delay: Duration::from_secs(2),
            ..FakeSource::default()
        };
        let r = rig_with(source, Duration::from_millis(500));
        r.player.play_next(song("A", Some(200))).await.unwrap();
        wait_call(&r.engine, "play:A").await;
        r.player.seek(Duration::from_secs(90), None).await.unwrap();
        let v = view(&r);
        assert_eq!(v.position_ms, 90_000);
        assert!(!v.position_running);
        wait_call_n(&r.engine, "play:A", 2).await;
        assert_eq!(
            view(&r).position_ms,
            90_000,
            "frozen until the latency has passed"
        );
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let v = view(&r);
        assert!(v.position_running);
        assert_eq!(v.position_ms, 91_000);
    }

    #[tokio::test(start_paused = true)]
    async fn seek_past_the_end_is_clamped() {
        let r = rig();
        r.player.play_next(song("A", Some(200))).await.unwrap();
        wait_call(&r.engine, "play:A").await;
        r.player.seek(Duration::from_secs(500), None).await.unwrap();
        wait_call_n(&r.engine, "play:A", 2).await;
        assert_eq!(
            r.source.starts().last(),
            Some(&("A".to_owned(), Duration::from_secs(199)))
        );
    }

    #[tokio::test(start_paused = true)]
    async fn seek_for_a_song_no_longer_current_does_nothing() {
        let r = rig();
        let a = r.player.play_next(song("A", Some(200))).await.unwrap();
        r.player.play_next(song("B", Some(200))).await.unwrap();
        wait_call(&r.engine, "play:A").await;
        end_track(&r).await;
        wait_call(&r.engine, "play:B").await;
        let acted = r
            .player
            .seek(Duration::from_secs(60), Some(a.id))
            .await
            .unwrap();
        assert!(!acted);
        settle().await;
        assert_eq!(
            r.source.starts().last(),
            Some(&("B".to_owned(), Duration::ZERO))
        );
    }

    #[tokio::test(start_paused = true)]
    async fn seek_near_the_end_prefetches_the_next_song() {
        let r = rig();
        r.player.play_next(song("A", Some(100))).await.unwrap();
        r.player
            .add(vec![song("X", Some(100))], AddAt::End)
            .await
            .unwrap();
        wait_call(&r.engine, "play:A").await;
        settle().await;
        assert!(!r.source.opens().contains(&"X".to_owned()));
        r.player.seek(Duration::from_secs(90), None).await.unwrap();
        wait_call_n(&r.engine, "play:A", 2).await;
        settle().await;
        assert!(r.source.opens().contains(&"X".to_owned()));
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
    async fn play_all_replaces_queue_and_drops_old_prefetch() {
        let source = FakeSource {
            delay: Duration::from_secs(2),
            ..FakeSource::default()
        };
        let r = rig_with(source, Duration::ZERO);
        r.player.play_next(song("A", Some(30))).await.unwrap();
        wait_call(&r.engine, "play:A").await;
        r.player.play_next(song("X", Some(30))).await.unwrap();
        r.player.pause().await.unwrap();
        // X's prefetch has started (A has 15 s left) by the time play_all runs.
        tokio::time::sleep(Duration::from_secs(16)).await;
        r.player.resume().await.unwrap();
        tokio::time::sleep(Duration::from_secs(16)).await;
        assert_eq!(r.source.opens(), ["A", "X"]);

        let count = r
            .player
            .play_all(vec![song("B", Some(30)), song("C", Some(30))])
            .await
            .unwrap();
        assert_eq!(count, 2);
        wait_call(&r.engine, "play:B").await;
        let v = view(&r);
        assert_eq!(current_id(&r).as_deref(), Some("B"));
        assert_eq!(v.status, Status::Playing);
        assert_eq!(v.upcoming.len(), 1);
        assert_eq!(v.upcoming[0].song.video_id, "C");
        assert_eq!(r.engine.calls().iter().filter(|c| *c == "stop").count(), 1);
        assert!(!r.engine.calls().contains(&"play:X".to_owned()));
    }

    #[tokio::test(start_paused = true)]
    async fn play_all_unpauses() {
        let r = rig();
        r.player.play_next(song("A", None)).await.unwrap();
        wait_call(&r.engine, "play:A").await;
        r.player.pause().await.unwrap();
        r.player.play_all(vec![song("B", None)]).await.unwrap();
        wait_call(&r.engine, "play:B").await;
        assert_eq!(view(&r).status, Status::Playing);
        let pauses = r.engine.calls().iter().filter(|c| *c == "pause").count();
        assert_eq!(pauses, 1, "only the explicit pause, none for the new song");
    }

    #[tokio::test(start_paused = true)]
    async fn play_all_empty_changes_nothing() {
        let r = rig();
        r.player.play_next(song("A", None)).await.unwrap();
        wait_call(&r.engine, "play:A").await;
        let before = view(&r).version;
        assert_eq!(r.player.play_all(Vec::new()).await.unwrap(), 0);
        settle().await;
        assert_eq!(current_id(&r).as_deref(), Some("A"));
        assert_eq!(view(&r).version, before);
        assert_eq!(r.engine.calls(), ["play:A"]);
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
        } = notices.try_recv().unwrap()
        else {
            panic!("expected SongFailed");
        };
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
    async fn position_runs_only_after_latency_and_republishes() {
        let r = rig_with(FakeSource::default(), Duration::from_millis(500));
        let mut rx = r.player.subscribe();
        r.player.play_next(song("A", Some(200))).await.unwrap();
        wait_call(&r.engine, "play:A").await;
        let v = view(&r);
        assert!(!v.position_running);
        assert_eq!(v.position_ms, 0);
        let before = rx.borrow_and_update().version;
        tokio::time::sleep(Duration::from_millis(600)).await;
        settle().await;
        let state = r.player.state();
        assert!(state.version > before, "a new version is published");
        let v = state.view(Instant::now());
        assert!(v.position_running);
        assert!(v.position_ms >= 100);

        r.player.pause().await.unwrap();
        assert!(!view(&r).position_running);
        r.player.resume().await.unwrap();
        assert!(view(&r).position_running);
    }

    #[tokio::test(start_paused = true)]
    async fn resume_before_latency_republishes_when_running() {
        let r = rig_with(FakeSource::default(), Duration::from_millis(500));
        r.player.play_next(song("A", Some(200))).await.unwrap();
        wait_call(&r.engine, "play:A").await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        r.player.pause().await.unwrap();
        r.player.resume().await.unwrap();
        assert!(!view(&r).position_running);
        let before = r.player.state().version;
        tokio::time::sleep(Duration::from_millis(400)).await;
        settle().await;
        let state = r.player.state();
        assert!(state.version > before);
        assert!(state.view(Instant::now()).position_running);
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

    /// Songs received so far on a `plays()` receiver.
    fn drain(rx: &mut broadcast::Receiver<Song>, got: &mut Vec<String>) {
        while let Ok(song) = rx.try_recv() {
            got.push(song.video_id);
        }
    }

    async fn advance(d: Duration) {
        tokio::time::advance(d).await;
        settle().await;
    }

    #[tokio::test(start_paused = true)]
    async fn counts_a_play_after_30_seconds() {
        let r = rig();
        let mut rx = r.player.plays();
        let mut got = vec![];
        r.player.play_next(song("A", Some(200))).await.unwrap();
        wait_call(&r.engine, "play:A").await;
        advance(Duration::from_secs(29)).await;
        drain(&mut rx, &mut got);
        assert!(got.is_empty());
        advance(Duration::from_secs(2)).await;
        drain(&mut rx, &mut got);
        assert_eq!(got, ["A"]);
        advance(Duration::from_secs(60)).await;
        drain(&mut rx, &mut got);
        assert_eq!(got, ["A"]);
    }

    #[tokio::test(start_paused = true)]
    async fn paused_time_does_not_count() {
        let r = rig();
        let mut rx = r.player.plays();
        let mut got = vec![];
        r.player.play_next(song("A", Some(200))).await.unwrap();
        wait_call(&r.engine, "play:A").await;
        advance(Duration::from_secs(10)).await;
        r.player.pause().await.unwrap();
        advance(Duration::from_secs(3600)).await;
        drain(&mut rx, &mut got);
        assert!(got.is_empty());
        r.player.resume().await.unwrap();
        advance(Duration::from_secs(19)).await;
        drain(&mut rx, &mut got);
        assert!(got.is_empty());
        advance(Duration::from_secs(2)).await;
        drain(&mut rx, &mut got);
        assert_eq!(got, ["A"]);
    }

    #[tokio::test(start_paused = true)]
    async fn skip_before_30_seconds_counts_nothing() {
        let r = rig();
        let mut rx = r.player.plays();
        let mut got = vec![];
        r.player.play_next(song("A", Some(200))).await.unwrap();
        r.player.play_next(song("B", Some(200))).await.unwrap();
        wait_call(&r.engine, "play:A").await;
        advance(Duration::from_secs(10)).await;
        r.player.skip(None).await.unwrap();
        wait_call(&r.engine, "play:B").await;
        drain(&mut rx, &mut got);
        assert!(got.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn short_song_counts_at_its_end() {
        let r = rig();
        let mut rx = r.player.plays();
        let mut got = vec![];
        r.player.play_next(song("A", Some(20))).await.unwrap();
        wait_call(&r.engine, "play:A").await;
        advance(Duration::from_secs(5)).await;
        drain(&mut rx, &mut got);
        assert!(got.is_empty());
        end_track(&r).await;
        drain(&mut rx, &mut got);
        assert_eq!(got, ["A"]);
    }

    #[tokio::test(start_paused = true)]
    async fn unknown_duration_ends_early_counts() {
        let r = rig();
        let mut rx = r.player.plays();
        let mut got = vec![];
        r.player.play_next(song("A", None)).await.unwrap();
        wait_call(&r.engine, "play:A").await;
        advance(Duration::from_secs(5)).await;
        end_track(&r).await;
        drain(&mut rx, &mut got);
        assert_eq!(got, ["A"]);
    }

    #[tokio::test(start_paused = true)]
    async fn previous_restart_counts_again() {
        let r = rig();
        let mut rx = r.player.plays();
        let mut got = vec![];
        r.player.play_next(song("A", Some(200))).await.unwrap();
        wait_call(&r.engine, "play:A").await;
        advance(Duration::from_secs(31)).await;
        drain(&mut rx, &mut got);
        assert_eq!(got, ["A"]);
        r.player.previous(None).await.unwrap();
        wait_call_n(&r.engine, "play:A", 2).await;
        advance(Duration::from_secs(31)).await;
        drain(&mut rx, &mut got);
        assert_eq!(got, ["A", "A"]);
    }

    #[tokio::test(start_paused = true)]
    async fn seek_forward_does_not_count_again() {
        let r = rig();
        let mut rx = r.player.plays();
        let mut got = vec![];
        r.player.play_next(song("A", Some(200))).await.unwrap();
        wait_call(&r.engine, "play:A").await;
        advance(Duration::from_secs(31)).await;
        drain(&mut rx, &mut got);
        assert_eq!(got, ["A"]);
        r.player.seek(Duration::from_secs(100), None).await.unwrap();
        wait_call_n(&r.engine, "play:A", 2).await;
        advance(Duration::from_secs(31)).await;
        drain(&mut rx, &mut got);
        assert_eq!(got, ["A"]);
    }

    #[tokio::test(start_paused = true)]
    async fn seek_to_end_of_long_song_does_not_count() {
        let r = rig();
        let mut rx = r.player.plays();
        let mut got = vec![];
        r.player.play_next(song("A", Some(200))).await.unwrap();
        wait_call(&r.engine, "play:A").await;
        advance(Duration::from_secs(5)).await;
        r.player.seek(Duration::from_secs(199), None).await.unwrap();
        wait_call_n(&r.engine, "play:A", 2).await;
        end_track(&r).await;
        drain(&mut rx, &mut got);
        assert!(got.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn counted_song_that_ends_counts_once() {
        let r = rig();
        let mut rx = r.player.plays();
        let mut got = vec![];
        r.player.play_next(song("A", Some(200))).await.unwrap();
        wait_call(&r.engine, "play:A").await;
        advance(Duration::from_secs(31)).await;
        drain(&mut rx, &mut got);
        assert_eq!(got, ["A"]);
        end_track(&r).await;
        drain(&mut rx, &mut got);
        assert_eq!(got, ["A"]);
    }

    #[tokio::test(start_paused = true)]
    async fn seek_to_zero_counts_again() {
        let r = rig();
        let mut rx = r.player.plays();
        let mut got = vec![];
        r.player.play_next(song("A", Some(200))).await.unwrap();
        wait_call(&r.engine, "play:A").await;
        advance(Duration::from_secs(31)).await;
        drain(&mut rx, &mut got);
        assert_eq!(got, ["A"]);
        r.player.seek(Duration::ZERO, None).await.unwrap();
        wait_call_n(&r.engine, "play:A", 2).await;
        advance(Duration::from_secs(31)).await;
        drain(&mut rx, &mut got);
        assert_eq!(got, ["A", "A"]);
    }

    #[tokio::test(start_paused = true)]
    async fn autoplay_off_never_fetches() {
        let r = rig();
        r.player.play_next(song("A", Some(200))).await.unwrap();
        wait_call(&r.engine, "play:A").await;
        end_track(&r).await;
        quiesce().await;
        assert_eq!(view(&r).status, Status::Stopped);
        assert!(r.radio.calls().is_empty());
        assert_eq!(view(&r).autoplay, AutoplayView::default());
    }

    #[tokio::test(start_paused = true)]
    async fn suggestions_follow_the_last_queued_song() {
        let radio = FakeRadio::default();
        radio.set("A", ["A", "x", "y"].map(radio_song).into());
        radio.set("B", ["B", "z"].map(radio_song).into());
        let r = rig_autoplay(radio);
        r.player.play_next(song("A", Some(200))).await.unwrap();
        quiesce().await;
        assert_eq!(suggested(&r), ["x", "y"]);
        assert_eq!(view(&r).autoplay.based_on.unwrap().video_id, "A");
        r.player
            .add(vec![song("B", Some(200))], AddAt::End)
            .await
            .unwrap();
        quiesce().await;
        assert_eq!(suggested(&r), ["z"]);
        assert_eq!(view(&r).autoplay.based_on.unwrap().video_id, "B");
        assert_eq!(r.radio.calls(), ["A", "B"]);
    }

    #[tokio::test(start_paused = true)]
    async fn suggestions_skip_queue_and_history() {
        let radio = FakeRadio::default();
        radio.set("B", ["B", "A", "h", "k"].map(radio_song).into());
        let r = rig_autoplay(radio);
        r.history.record(&song("h", Some(200))).unwrap();
        r.player.play_next(song("A", Some(200))).await.unwrap();
        r.player
            .add(vec![song("B", Some(200))], AddAt::End)
            .await
            .unwrap();
        quiesce().await;
        assert_eq!(suggested(&r), ["k"]);
    }

    #[tokio::test(start_paused = true)]
    async fn stale_radio_result_is_dropped() {
        let radio = FakeRadio::default();
        radio.set("A", ["A", "old"].map(radio_song).into());
        radio.delay("A", Duration::from_secs(5));
        radio.set("B", ["B", "z"].map(radio_song).into());
        let r = rig_autoplay(radio);
        r.player.play_next(song("A", Some(200))).await.unwrap();
        r.player
            .add(vec![song("B", Some(200))], AddAt::End)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_secs(6)).await;
        quiesce().await;
        assert_eq!(suggested(&r), ["z"]);
        assert_eq!(view(&r).autoplay.based_on.unwrap().video_id, "B");
    }

    #[tokio::test(start_paused = true)]
    async fn failed_radio_notifies_once_until_a_success() {
        let radio = FakeRadio::default();
        radio.fail("A");
        radio.fail("B");
        radio.set("C", ["C", "w"].map(radio_song).into());
        radio.fail("D");
        let r = rig_autoplay(radio);
        let mut notices = r.player.notices();
        r.player.play_next(song("A", Some(200))).await.unwrap();
        quiesce().await;
        r.player
            .add(vec![song("B", Some(200))], AddAt::End)
            .await
            .unwrap();
        quiesce().await;
        assert_eq!(drain_notices(&mut notices), 1);
        assert!(suggested(&r).is_empty());
        r.player
            .add(vec![song("C", Some(200))], AddAt::End)
            .await
            .unwrap();
        quiesce().await;
        assert_eq!(suggested(&r), ["w"]);
        assert_eq!(drain_notices(&mut notices), 0);
        r.player
            .add(vec![song("D", Some(200))], AddAt::End)
            .await
            .unwrap();
        quiesce().await;
        assert_eq!(drain_notices(&mut notices), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn failed_radio_retries_on_the_next_song() {
        let radio = FakeRadio::default();
        radio.set("A", ["A"].map(radio_song).into());
        radio.fail("B");
        let r = rig_autoplay(radio);
        r.player.play_next(song("A", Some(200))).await.unwrap();
        wait_call(&r.engine, "play:A").await;
        r.player
            .add(vec![song("B", Some(200))], AddAt::End)
            .await
            .unwrap();
        quiesce().await;
        assert!(suggested(&r).is_empty());
        r.radio.set("B", ["B", "z"].map(radio_song).into());
        end_track(&r).await;
        quiesce().await;
        assert_eq!(current_id(&r).as_deref(), Some("B"));
        assert_eq!(suggested(&r), ["z"]);
        assert_eq!(r.radio.calls(), ["A", "B", "B"]);
    }

    #[tokio::test(start_paused = true)]
    async fn switch_off_clears_and_on_refetches() {
        let radio = FakeRadio::default();
        radio.set("A", ["A", "x"].map(radio_song).into());
        let r = rig_autoplay(radio);
        r.player.play_next(song("A", Some(200))).await.unwrap();
        quiesce().await;
        assert_eq!(suggested(&r), ["x"]);
        r.player.set_autoplay(false).await.unwrap();
        let v = view(&r).autoplay;
        assert!(!v.enabled);
        assert!(v.songs.is_empty());
        assert!(v.based_on.is_none());
        r.player.set_autoplay(true).await.unwrap();
        quiesce().await;
        assert!(view(&r).autoplay.enabled);
        assert_eq!(suggested(&r), ["x"]);
        assert_eq!(r.radio.calls(), ["A", "A"]);
    }

    #[tokio::test(start_paused = true)]
    async fn idle_player_does_not_fetch() {
        let r = rig_autoplay(FakeRadio::default());
        quiesce().await;
        assert!(r.radio.calls().is_empty());
        let v = view(&r).autoplay;
        assert!(v.enabled);
        assert!(v.songs.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn queue_end_hands_over_to_the_first_suggestion() {
        let radio = FakeRadio::default();
        radio.set("A", ["A", "x", "y"].map(radio_song).into());
        radio.set("x", ["x", "w"].map(radio_song).into());
        let r = rig_autoplay(radio);
        let a = r.player.play_next(song("A", Some(200))).await.unwrap();
        wait_call(&r.engine, "play:A").await;
        quiesce().await;
        end_track(&r).await;
        wait_call(&r.engine, "play:x").await;
        quiesce().await;
        let v = view(&r);
        let cur = v.current.unwrap();
        assert_eq!(cur.song.video_id, "x");
        assert!(cur.id > a.id, "fresh entry id");
        assert!(v.upcoming.is_empty());
        assert_eq!(v.autoplay.based_on.unwrap().video_id, "x");
        assert_eq!(suggested(&r), ["w"]);
    }

    #[tokio::test(start_paused = true)]
    async fn hand_over_has_no_gap() {
        let source = FakeSource {
            delay: Duration::from_secs(2),
            ..FakeSource::default()
        };
        let radio = FakeRadio::default();
        radio.set("A", ["A", "x"].map(radio_song).into());
        let r = rig_built(source, Duration::ZERO, radio, true);
        r.player.play_next(song("A", Some(30))).await.unwrap();
        wait_call(&r.engine, "play:A").await;
        tokio::time::sleep(Duration::from_secs(20)).await;
        assert_eq!(r.source.opens(), ["A", "x"], "x is prefetched at 15 s left");
        end_track(&r).await;
        let took = wait_call(&r.engine, "play:x").await;
        assert!(took <= Duration::from_millis(10), "switch took {took:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn skip_on_the_last_song_plays_the_first_suggestion() {
        let radio = FakeRadio::default();
        radio.set("A", ["A", "x"].map(radio_song).into());
        let r = rig_autoplay(radio);
        r.player.play_next(song("A", Some(200))).await.unwrap();
        wait_call(&r.engine, "play:A").await;
        quiesce().await;
        assert!(r.player.skip(None).await.unwrap());
        wait_call(&r.engine, "play:x").await;
        assert_eq!(current_id(&r).as_deref(), Some("x"));
    }

    #[tokio::test(start_paused = true)]
    async fn playing_a_suggestion_keeps_the_queue() {
        let radio = FakeRadio::default();
        radio.set("B", ["B", "s1", "s2", "s3"].map(radio_song).into());
        let r = rig_autoplay(radio);
        r.player.play_next(song("A", Some(200))).await.unwrap();
        r.player.play_next(song("B", Some(200))).await.unwrap();
        quiesce().await;
        assert_eq!(suggested(&r), ["s1", "s2", "s3"]);
        assert!(r.player.play_suggestion("s2".into()).await.unwrap());
        wait_call(&r.engine, "play:s2").await;
        quiesce().await;
        let v = view(&r);
        assert_eq!(v.current.unwrap().song.video_id, "s2");
        let upcoming: Vec<_> = v
            .upcoming
            .iter()
            .map(|e| e.song.video_id.as_str())
            .collect();
        assert_eq!(upcoming, ["B"]);
        assert_eq!(suggested(&r), ["s3"]);
        assert_eq!(v.autoplay.based_on.unwrap().video_id, "B");
    }

    #[tokio::test(start_paused = true)]
    async fn playing_a_suggestion_with_an_empty_queue_reseeds() {
        let radio = FakeRadio::default();
        radio.set("A", ["A", "x", "y"].map(radio_song).into());
        radio.set("y", ["y", "q"].map(radio_song).into());
        let r = rig_autoplay(radio);
        r.player.play_next(song("A", Some(200))).await.unwrap();
        quiesce().await;
        assert!(r.player.play_suggestion("y".into()).await.unwrap());
        quiesce().await;
        assert_eq!(current_id(&r).as_deref(), Some("y"));
        assert_eq!(view(&r).autoplay.based_on.unwrap().video_id, "y");
        assert_eq!(suggested(&r), ["q"]);
    }

    #[tokio::test(start_paused = true)]
    async fn playing_a_gone_suggestion_does_nothing() {
        let radio = FakeRadio::default();
        radio.set("A", ["A", "x"].map(radio_song).into());
        let r = rig_autoplay(radio);
        r.player.play_next(song("A", Some(200))).await.unwrap();
        r.player.play_next(song("B", Some(200))).await.unwrap();
        quiesce().await;
        let before = view(&r);
        assert!(!r.player.play_suggestion("nope".into()).await.unwrap());
        quiesce().await;
        let after = view(&r);
        assert_eq!(after.current, before.current);
        assert_eq!(after.upcoming, before.upcoming);
    }

    #[tokio::test(start_paused = true)]
    async fn fully_filtered_radio_stops_quietly() {
        let radio = FakeRadio::default();
        radio.set("A", ["A", "x", "y"].map(radio_song).into());
        let r = rig_autoplay(radio);
        r.history.record(&song("x", Some(200))).unwrap();
        r.history.record(&song("y", Some(200))).unwrap();
        let mut notices = r.player.notices();
        r.player.play_next(song("A", Some(200))).await.unwrap();
        wait_call(&r.engine, "play:A").await;
        quiesce().await;
        assert!(suggested(&r).is_empty());
        end_track(&r).await;
        assert_eq!(view(&r).status, Status::Stopped);
        assert!(view(&r).current.is_none());
        assert_eq!(drain_notices(&mut notices), 0);
        assert!(notices.try_recv().is_err());
    }
}
