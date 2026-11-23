//! The player actor: owns the `Queue` and the `AudioEngine`.
//!
//! The HTTP layer talks to it through a `PlayerHandle` (commands in, state
//! out on a `watch` channel, notices on a `broadcast` channel).

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use anyhow::anyhow;
use serde::Serialize;
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::autoplay::{RECENT_FILTER, Repeat, pick_suggestions};
use crate::crossfade::{self, Chain, ChainEvent, ChainHandle, Withdrawn};
use crate::engine::null::NullEngine;
use crate::engine::{AudioEngine, EngineEvent};
use crate::fade::{Fade, FadeControl};
use crate::history::History;
use crate::queue::{AddAt, AddOutcome, EntryId, PlayNextOutcome, Queue, QueueEntry};
use crate::radio::{RADIO_TIMEOUT, Radio};
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
    /// Whether the clock tracks what the listener hears. False while a
    /// reporting engine has yet to send its first `Playing`.
    anchored: bool,
    /// Listening time banked before the last re-anchor, so reports don't
    /// reset what counts towards `PLAYED_AFTER`.
    counted_before: Duration,
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

/// What a client asks the sleep timer to do.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SleepRequest {
    /// Fade out and pause after this many minutes.
    Minutes(u32),
    /// Pause when the current song ends.
    EndOfSong,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Sleep {
    At(Instant),
    EndOfSong,
}

/// The sleep timer as clients see it.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(untagged, rename_all_fields = "camelCase")]
pub enum SleepView {
    EndOfSong { end_of_song: bool },
    Remaining { remaining_ms: u64 },
}

#[derive(Clone, Debug)]
pub struct PlayerState {
    pub current: Option<QueueEntry>,
    pub upcoming: Vec<QueueEntry>,
    pub status: Status,
    pub latency: Duration,
    pub version: u64,
    pub autoplay: AutoplayView,
    sleep: Option<Sleep>,
    /// How long songs blend into each other; zero is off.
    pub crossfade: Duration,
    pub repeat: Repeat,
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
    /// The sleep timer, `None` when off.
    pub sleep: Option<SleepView>,
    pub crossfade_secs: u32,
    pub repeat: Repeat,
}

impl Clock {
    /// How much the engine has played since its reader started.
    fn played(&self, now: Instant) -> Duration {
        self.played_before + self.resumed_at.map_or(Duration::ZERO, |t| now - t)
    }

    /// How long the song has been heard, across re-anchors.
    fn listened(&self, now: Instant) -> Duration {
        self.counted_before + self.played(now)
    }

    /// How far into the song the engine has written.
    fn song_time(&self, now: Instant) -> Duration {
        self.offset + self.played(now)
    }

    /// The position the listener hears, `latency` behind the engine.
    fn heard(&self, now: Instant, latency: Duration) -> Duration {
        if !self.anchored {
            return self.offset;
        }
        self.offset + self.played(now).saturating_sub(latency)
    }

    /// Whether `heard` is advancing.
    fn moving(&self, now: Instant, latency: Duration) -> bool {
        self.anchored && self.running() && self.played(now) >= latency
    }

    /// How long until `heard` starts advancing, while the clock runs.
    fn moves_in(&self, now: Instant, latency: Duration) -> Option<Duration> {
        let played = self.played(now);
        (self.anchored && self.running() && played < latency).then(|| latency - played)
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
    /// How long until the position starts moving: `Some` while the clock
    /// runs but has not yet passed the speaker latency.
    pub fn moves_in(&self, now: Instant) -> Option<Duration> {
        self.clock.moves_in(now, self.latency)
    }

    pub fn view(&self, now: Instant) -> StateView {
        let position = self.clock.heard(now, self.latency);
        StateView {
            current: self.current.clone(),
            status: self.status,
            position_ms: position.as_millis() as u64,
            position_running: self.clock.moving(now, self.latency),
            upcoming: self.upcoming.clone(),
            version: self.version,
            autoplay: self.autoplay.clone(),
            sleep: self.sleep.map(|s| match s {
                Sleep::EndOfSong => SleepView::EndOfSong { end_of_song: true },
                Sleep::At(at) => SleepView::Remaining {
                    remaining_ms: at.saturating_duration_since(now).as_millis() as u64,
                },
            }),
            crossfade_secs: self.crossfade.as_secs() as u32,
            repeat: self.repeat,
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
    /// Something asked to play while no output is chosen.
    NoOutput,
}

enum Command {
    PlayNext(Song, oneshot::Sender<PlayNextOutcome>),
    PlayAll(Vec<Song>, Duration, oneshot::Sender<usize>),
    Add(Vec<Song>, AddAt, oneshot::Sender<AddOutcome>),
    Remove(EntryId, oneshot::Sender<bool>),
    Move(EntryId, Option<EntryId>, oneshot::Sender<bool>),
    Pause(oneshot::Sender<()>),
    Resume(oneshot::Sender<()>),
    Skip(Option<EntryId>, oneshot::Sender<bool>),
    Previous(Option<EntryId>, oneshot::Sender<bool>),
    Seek(Duration, Option<EntryId>, oneshot::Sender<bool>),
    SetAutoplay(bool, oneshot::Sender<()>),
    SetSleep(Option<SleepRequest>, oneshot::Sender<()>),
    SetCrossfade(Duration, oneshot::Sender<()>),
    SetRepeat(Repeat, oneshot::Sender<()>),
    PlaySuggestion(String, oneshot::Sender<bool>),
    SetOutput(Box<dyn AudioEngine>, oneshot::Sender<()>),
    ReleaseOutput(oneshot::Sender<()>),
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
        self.ask(|tx| Command::PlayAll(songs, Duration::ZERO, tx))
            .await
    }
    /// Like [`play_all`](Self::play_all), and the sound rises from silence
    /// over `fade_in` of audio (zero: none). Skip, previous, seek, pause, a
    /// new play, or an output switch end the fade-in at once; a song ending
    /// by itself does not.
    pub async fn play_all_with_fade_in(
        &self,
        songs: Vec<Song>,
        fade_in: Duration,
    ) -> anyhow::Result<usize> {
        self.ask(|tx| Command::PlayAll(songs, fade_in, tx)).await
    }
    /// Adds songs after the current one (`Next`) or at the end; starts the
    /// first one at once when nothing was playing.
    pub async fn add(&self, songs: Vec<Song>, at: AddAt) -> anyhow::Result<AddOutcome> {
        self.ask(|tx| Command::Add(songs, at, tx)).await
    }
    pub async fn remove(&self, id: EntryId) -> anyhow::Result<bool> {
        self.ask(|tx| Command::Remove(id, tx)).await
    }
    /// Moves upcoming entry `id` to just after upcoming entry `after`, or to
    /// the top when `None`. False when either isn't upcoming.
    pub async fn move_entry(&self, id: EntryId, after: Option<EntryId>) -> anyhow::Result<bool> {
        self.ask(|tx| Command::Move(id, after, tx)).await
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
    /// Restarts the current song, or when it has played under
    /// [`RESTART_THRESHOLD`] goes back to the previously played one (if
    /// any); `expected` as for [`skip`](Self::skip).
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
    /// Sets the sleep timer, replacing any current one; `None` cancels it.
    pub async fn set_sleep(&self, request: Option<SleepRequest>) -> anyhow::Result<()> {
        self.ask(|tx| Command::SetSleep(request, tx)).await
    }
    /// Sets how long songs blend into each other; zero turns it off.
    pub async fn set_crossfade(&self, d: Duration) -> anyhow::Result<()> {
        self.ask(|tx| Command::SetCrossfade(d, tx)).await
    }

    /// Sets what happens when a song or the queue ends.
    pub async fn set_repeat(&self, mode: Repeat) -> anyhow::Result<()> {
        self.ask(|tx| Command::SetRepeat(mode, tx)).await
    }
    /// Plays a suggested song now, keeping the queue. `false` (nothing
    /// changes) unless it is among the current suggestions.
    pub async fn play_suggestion(&self, video_id: String) -> anyhow::Result<bool> {
        self.ask(|tx| Command::PlaySuggestion(video_id, tx)).await
    }
    /// Switches to a new output. The current song moves to it at the
    /// position heard so far, keeping play/pause. Events from the old
    /// output become stale.
    pub async fn set_output(&self, engine: Box<dyn AudioEngine>) -> anyhow::Result<()> {
        self.ask(|tx| Command::SetOutput(engine, tx)).await
    }
    /// Drops the current output (for shutdown): the player moves onto no output, as
    /// before one was chosen, and the current song stays, paused.
    pub async fn release_output(&self) -> anyhow::Result<()> {
        self.ask(Command::ReleaseOutput).await
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

/// How often to check whether a running mix is over, to offer the next song.
const MIX_POLL: Duration = Duration::from_millis(250);

/// Previous restarts a song played longer than this, else steps back.
pub const RESTART_THRESHOLD: Duration = Duration::from_secs(3);

/// After this many songs in a row fail to open, autoplay stops handing over.
pub const MAX_AUTOPLAY_FAILURES: u32 = 3;

/// A source being opened in the background. Dropping it cancels the open
/// (or drops the reader, killing its processes).
struct Opening {
    task: JoinHandle<anyhow::Result<Box<dyn PcmReader>>>,
    /// Built from a result we already have (see `ready`).
    done: bool,
}

impl Opening {
    fn spawn(source: &Arc<dyn PcmSource>, video_id: &str, start: Duration) -> Self {
        let source = source.clone();
        let video_id = video_id.to_owned();
        let task = tokio::spawn(async move { source.open(&video_id, start).await });
        Self { task, done: false }
    }

    /// An opening that is already finished with `result` (a reader taken back).
    fn ready(result: anyhow::Result<Box<dyn PcmReader>>) -> Self {
        Self {
            task: tokio::spawn(async move { result }),
            done: true,
        }
    }

    fn is_ready(&self) -> bool {
        self.done || self.task.is_finished()
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

/// A prefetch that finished while the chain could take it.
struct PrefetchDone {
    target: PrefetchFor,
    result: anyhow::Result<Box<dyn PcmReader>>,
}

/// Waits for the prefetch to finish, but only while `wanted`. Takes the
/// prefetch out when it finishes, so it is never polled twice.
async fn wait_prefetch(prefetch: &mut Option<Prefetch>, wanted: bool) -> PrefetchDone {
    let Some(p) = prefetch.as_mut().filter(|_| wanted) else {
        return std::future::pending().await;
    };
    let joined = (&mut p.opening.task).await;
    let p = prefetch.take().expect("prefetch was Some");
    PrefetchDone {
        target: p.target,
        result: joined.unwrap_or_else(|e| Err(anyhow!("open task failed: {e}"))),
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
            let songs = tokio::time::timeout(RADIO_TIMEOUT, radio.radio(&video_id))
                .await
                .map_err(|_| anyhow!("radio request timed out"))??;
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
    /// `false` while `engine` is a `NullEngine` placeholder: songs stay
    /// paused and nothing is opened.
    has_output: bool,
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
    /// How long songs blend into each other; zero is off.
    crossfade: Duration,
    repeat: Repeat,
    /// Entries Repeat all appended to `upcoming` to loop the cycle.
    looped: HashSet<EntryId>,
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
    /// Video ids of the songs that most recently became current, oldest
    /// first, so suggestions never bring one straight back.
    played_recently: VecDeque<String>,
    /// Songs in a row that failed to open; reset by a successful open.
    failures: u32,
    sleep: Option<Sleep>,
    /// When a timed sleep pauses the player.
    sleep_at: Option<Instant>,
    /// Shared with the `Fade` wrapped around every reader the engine gets.
    fade: FadeControl,
    /// Chain events (from the chain the engine reads) arrive here.
    chain_tx: mpsc::UnboundedSender<ChainEvent>,
    /// The chain handed to the engine for the current generation.
    chain: Option<ChainHandle>,
    /// The prefetched reader the chain holds, and the `seq` it was offered with.
    offer: Option<(PrefetchFor, u64)>,
    offer_seq: u64,
    /// The prefetch for this target could not be offered (or failed to open):
    /// leave it for the normal transition instead of trying again.
    offer_blocked: Option<PrefetchFor>,
}

enum Wake {
    Command(Option<Command>),
    Engine(EngineEvent),
    Opened(Opened),
    Prefetched(PrefetchDone),
    Chain(ChainEvent),
    PrefetchDue,
    PositionDue,
    PlayedDue,
    SleepDue,
    RadioLoaded(RadioLoaded),
}

impl Actor {
    async fn run(
        mut self,
        mut commands: mpsc::UnboundedReceiver<Command>,
        mut events: mpsc::UnboundedReceiver<EngineEvent>,
        mut chain_events: mpsc::UnboundedReceiver<ChainEvent>,
    ) {
        loop {
            let offer_wanted = self.offer_wanted();
            let wake = tokio::select! {
                cmd = commands.recv() => Wake::Command(cmd),
                Some(ev) = events.recv() => Wake::Engine(ev),
                opened = wait_pending(&mut self.pending) => Wake::Opened(opened),
                Some(ev) = chain_events.recv() => Wake::Chain(ev),
                done = wait_prefetch(&mut self.prefetch, offer_wanted) => Wake::Prefetched(done),
                () = sleep_until_opt(self.prefetch_at) => Wake::PrefetchDue,
                () = sleep_until_opt(self.position_at) => Wake::PositionDue,
                () = sleep_until_opt(self.played_at) => Wake::PlayedDue,
                () = sleep_until_opt(self.sleep_at) => Wake::SleepDue,
                loaded = wait_radio(&mut self.fetch) => Wake::RadioLoaded(loaded),
            };
            let now = Instant::now();
            match wake {
                Wake::Command(None) => break,
                Wake::Command(Some(cmd)) => self.on_command(cmd, now),
                Wake::Engine(ev) => self.on_engine_event(ev, now),
                Wake::Opened(opened) => self.on_opened(opened, now),
                Wake::Prefetched(done) => self.on_prefetched(done, now),
                Wake::Chain(ev) => self.on_chain_event(ev, now),
                Wake::PrefetchDue => self.update_prefetch(now),
                Wake::PositionDue => self.publish(now),
                Wake::PlayedDue => self.update_played(now),
                Wake::SleepDue => self.on_sleep_due(now),
                Wake::RadioLoaded(loaded) => self.on_radio_loaded(loaded, now),
            }
        }
        let _ = self.engine.stop();
    }

    fn on_command(&mut self, cmd: Command, now: Instant) {
        match cmd {
            Command::PlayNext(song, reply) => {
                let outcome = self.queue.play_next(song);
                self.fill_loop();
                if outcome.started {
                    self.fade.end_fade_in();
                    self.paused = false;
                    self.start_current(now);
                } else {
                    self.update_prefetch(now);
                    self.publish(now);
                }
                let _ = reply.send(outcome);
            }
            Command::PlayAll(songs, fade_in, reply) => {
                let count = songs.len();
                if self.queue.play_all(songs).is_some() {
                    self.looped.clear();
                    self.fill_loop();
                    self.fade.end_fade_in();
                    self.fade.arm_fade_in(fade_in);
                    self.paused = false;
                    self.start_current(now);
                }
                let _ = reply.send(count);
            }
            Command::Add(songs, at, reply) => {
                let outcome = self.queue.add_many(songs, at);
                self.fill_loop();
                if outcome.started {
                    self.fade.end_fade_in();
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
                    self.fill_loop();
                    self.update_prefetch(now);
                    self.publish(now);
                }
                let _ = reply.send(removed);
            }
            Command::Move(id, after, reply) => {
                let moved = self.queue.move_entry(id, after);
                if moved {
                    self.update_prefetch(now);
                    self.publish(now);
                }
                let _ = reply.send(moved);
            }
            Command::Pause(reply) => {
                self.fade.end_fade_in();
                self.pause(now);
                let _ = reply.send(());
            }
            Command::Resume(reply) => {
                if !self.has_output && self.queue.current().is_some() {
                    let _ = self.notices.send(Notice::NoOutput);
                } else if self.queue.current().is_some() && self.paused {
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
                    self.fade.end_fade_in();
                    self.advance(false);
                    self.paused = false;
                    self.start_current(now);
                }
                let _ = reply.send(act);
            }
            Command::Previous(expected, reply) => {
                let act = self.is_current(expected);
                if act {
                    self.fade.end_fade_in();
                    let position = self.clock.heard(now, self.engine.latency());
                    if position < RESTART_THRESHOLD {
                        self.queue.back();
                    }
                    self.paused = false;
                    self.start_current(now);
                }
                let _ = reply.send(act);
            }
            Command::Seek(position, expected, reply) => {
                let act = self.is_current(expected);
                if act {
                    self.fade.end_fade_in();
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
            Command::SetSleep(request, reply) => {
                self.set_sleep(request, now);
                let _ = reply.send(());
            }
            Command::SetCrossfade(d, reply) => {
                self.crossfade = d;
                if d.is_zero() && self.take_back_offer() {
                    // The running chain plays through; the next song comes the normal way.
                    self.end_song(now);
                    self.publish(now);
                    let _ = reply.send(());
                    return;
                }
                self.update_prefetch(now);
                self.publish(now);
                let _ = reply.send(());
            }
            Command::SetRepeat(mode, reply) => {
                self.set_repeat(mode, now);
                let _ = reply.send(());
            }
            Command::PlaySuggestion(video_id, reply) => {
                let song = self
                    .suggestions()
                    .into_iter()
                    .find(|s| s.video_id == video_id);
                let found = song.is_some();
                if let Some(song) = song {
                    self.fade.end_fade_in();
                    self.queue.replace_current(song);
                    self.looped.clear();
                    self.fill_loop();
                    if let Some(at) = self.radio_songs.iter().position(|s| s.video_id == video_id) {
                        self.radio_songs.drain(..=at);
                    }
                    self.paused = false;
                    self.start_current(now);
                }
                let _ = reply.send(found);
            }
            Command::SetOutput(engine, reply) => {
                self.fade.end_fade_in();
                self.set_output(engine, now);
                let _ = reply.send(());
            }
            Command::ReleaseOutput(reply) => {
                self.release_output(now);
                let _ = reply.send(());
            }
        }
    }

    /// Switches the repeat mode. Any offer is taken back first, since the
    /// next song it stands for may change.
    fn set_repeat(&mut self, mode: Repeat, now: Instant) {
        if mode == self.repeat {
            return;
        }
        if self.take_back_offer() {
            // The chain already joined the next song: that song is current now.
            self.end_song(now);
        }
        let was = self.repeat;
        self.repeat = mode;
        if was == Repeat::All {
            self.queue.drop_upcoming(&self.looped);
            self.looped.clear();
        }
        self.fill_loop();
        self.update_prefetch(now);
        self.publish(now);
    }

    /// With Repeat all and nothing upcoming, appends the cycle so the loop
    /// shows in Up next and the wrap is prefetched like any other song.
    /// Not after a run of failed songs, so unplayable songs can't spin.
    fn fill_loop(&mut self) {
        if self.repeat != Repeat::All
            || self.queue.current().is_none()
            || !self.queue.upcoming().is_empty()
            || self.failures >= MAX_AUTOPLAY_FAILURES
        {
            return;
        }
        // Upcoming is empty, so every earlier loop copy has been played.
        self.looped.clear();
        self.looped.extend(self.queue.append_cycle());
    }

    fn pause(&mut self, now: Instant) {
        if self.queue.current().is_some() && !self.paused {
            self.paused = true;
            if self.started {
                self.engine_call("pause", |e| e.pause());
                self.clock.pause(now);
            }
            self.update_prefetch(now);
            self.publish(now);
        }
    }

    fn set_sleep(&mut self, request: Option<SleepRequest>, now: Instant) {
        self.fade.clear();
        self.sleep_at = None;
        self.sleep = match request {
            None => None,
            Some(SleepRequest::EndOfSong) => Some(Sleep::EndOfSong),
            Some(SleepRequest::Minutes(minutes)) => {
                let deadline = now + Duration::from_secs(u64::from(minutes) * 60);
                self.fade.start(deadline, now);
                self.sleep_at = Some(deadline);
                Some(Sleep::At(deadline))
            }
        };
        if self.sleep == Some(Sleep::EndOfSong) && self.take_back_offer() {
            // The chain already joined the next song: that song is the end.
            self.end_song(now);
        }
        self.publish(now);
    }

    /// A timed sleep reached its deadline: pause (if playing), then the
    /// timer is spent and the volume is back for the next Play.
    fn on_sleep_due(&mut self, now: Instant) {
        self.sleep = None;
        self.sleep_at = None;
        self.fade.clear();
        self.pause(now);
        self.publish(now);
    }

    fn set_output(&mut self, engine: Box<dyn AudioEngine>, now: Instant) {
        let heard = self.clock.heard(now, self.engine.latency());
        self.reclaim_offer();
        if self.engine_busy {
            self.engine_busy = false;
            self.engine_call("stop", |e| e.stop());
        }
        self.engine = engine;
        self.has_output = true;
        self.started = false;
        if self.queue.current().is_some() {
            // The prefetch stays: readers don't depend on the engine.
            self.open_current(heard, false, now);
        } else {
            self.publish(now);
        }
    }

    fn release_output(&mut self, now: Instant) {
        self.reclaim_offer();
        if self.engine_busy {
            self.engine_busy = false;
            self.engine_call("stop", |e| e.stop());
        }
        if self.started && !self.paused {
            self.clock.pause(now);
        }
        // Dropping the old engine closes it; its late events are stale.
        self.engine = Box::new(NullEngine);
        self.has_output = false;
        self.generation += 1;
        self.pending = None;
        self.started = false;
        if self.queue.current().is_some() {
            self.paused = true;
        }
        self.update_prefetch(now);
        self.publish(now);
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
        exclude.extend(self.played_recently.iter().cloned());
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
                self.on_track_ended(now);
            }
            EngineEvent::TrackFailed { generation, reason } if generation == self.generation => {
                self.engine_busy = false;
                self.fail_current(reason, now);
            }
            EngineEvent::Playing {
                generation,
                position,
                at,
            } if generation == self.generation && self.started => {
                let counted_before = if self.clock.anchored {
                    self.clock.listened(now)
                } else {
                    self.clock.counted_before
                };
                self.clock = Clock {
                    offset: position,
                    played_before: Duration::ZERO,
                    resumed_at: (!self.paused).then_some(at),
                    anchored: true,
                    counted_before,
                };
                self.update_prefetch(now);
                self.publish(now);
            }
            _ => {} // stale: from a song we already moved away from
        }
    }

    /// A song shorter than `PLAYED_AFTER` (or of unknown length) counts as
    /// played when it ends, if it was not recorded yet.
    fn record_short_play(&mut self) {
        let short = self.queue.current().is_some_and(|e| {
            e.song
                .duration_secs
                .is_none_or(|d| u64::from(d) < PLAYED_AFTER.as_secs())
        });
        if !self.played_recorded && short {
            self.record_play();
        }
    }

    /// Whether the player hands the engine a crossfade chain.
    fn chaining(&self) -> bool {
        !self.crossfade.is_zero() && self.engine.crossfades() && !self.engine.reports_position()
    }

    /// Whether the next song may be offered to the chain. Not while the sleep
    /// timer waits for the end of the song, so the song really ends, and not
    /// while the song repeats.
    fn offer_allowed(&self) -> bool {
        self.sleep != Some(Sleep::EndOfSong) && self.repeat != Repeat::One
    }

    fn offer_wanted(&self) -> bool {
        self.chain.is_some()
            && self.started
            && self.chaining()
            && self.offer.is_none()
            && self.offer_allowed()
            && self.near_end(Instant::now())
            && self.chain.as_ref().is_some_and(|c| !c.mixing())
            && self
                .prefetch
                .as_ref()
                .is_some_and(|p| self.offer_blocked.as_ref() != Some(&p.target))
    }

    /// The current song has `prefetch_lead` or less left (or its length is
    /// unknown). An offer made earlier would make the chain hold back its
    /// first bytes, waiting to see the end.
    fn near_end(&self, now: Instant) -> bool {
        let Some(secs) = self.queue.current().and_then(|e| e.song.duration_secs) else {
            return true;
        };
        let left = Duration::from_secs(secs.into()).saturating_sub(self.clock.song_time(now));
        left <= self.prefetch_lead()
    }

    /// How long before the end the next song opens.
    fn prefetch_lead(&self) -> Duration {
        if self.chaining() {
            PREFETCH_LEAD + self.crossfade + crossfade::MAX_TRAILING_SILENCE
        } else {
            PREFETCH_LEAD
        }
    }

    /// The song a prefetch target stands for.
    fn target_song(&self, target: &PrefetchFor) -> Option<Song> {
        match target {
            PrefetchFor::Entry(id) => self
                .queue
                .upcoming()
                .iter()
                .find(|e| e.id == *id)
                .map(|e| e.song.clone()),
            PrefetchFor::Suggestion(video_id) => self
                .suggestions()
                .into_iter()
                .find(|s| s.video_id == *video_id),
        }
    }

    /// No fade between songs of the same album and artists (they play gapless).
    fn fade_for_next(&self, target: &PrefetchFor) -> Duration {
        let (Some(cur), Some(next)) = (
            self.queue.current().map(|e| &e.song),
            self.target_song(target),
        ) else {
            return self.crossfade;
        };
        let album = |s: &Song| s.album.as_deref().unwrap_or("").trim().to_lowercase();
        let same = !album(cur).is_empty()
            && album(cur) == album(&next)
            && cur.artists.trim().to_lowercase() == next.artists.trim().to_lowercase();
        if same { Duration::ZERO } else { self.crossfade }
    }

    /// Takes a held offer back from the chain; a reader that had not started
    /// mixing becomes the prefetch again.
    ///
    /// Returns true when the chain had already taken the offer itself (a
    /// gapless join, or a zero-length mix): the next song is playing, and the
    /// caller must move the player onto it (see `end_song_early`).
    #[must_use]
    fn take_back_offer(&mut self) -> bool {
        let Some((target, _)) = self.offer.take() else {
            return false;
        };
        match self.chain.as_ref().map(|c| c.withdraw()) {
            Some(Withdrawn::Reader(reader)) => {
                if self.prefetch.is_none() {
                    self.prefetch = Some(Prefetch {
                        target,
                        opening: Opening::ready(Ok(reader)),
                    });
                }
                false
            }
            Some(Withdrawn::Consumed) => true,
            Some(Withdrawn::Nothing) | None => false,
        }
    }

    /// The engine finished the current song by itself. With Repeat one it
    /// starts over (paused if the sleep timer waits for the end of the
    /// song); otherwise the queue moves on.
    fn on_track_ended(&mut self, now: Instant) {
        if self.repeat != Repeat::One || self.queue.current().is_none() {
            self.end_song(now);
            return;
        }
        self.record_short_play();
        if self.sleep == Some(Sleep::EndOfSong) {
            self.sleep = None;
            self.paused = true;
        }
        self.start_current(now);
    }

    /// The current song ended (or the chain already moved on to the next one
    /// before the player could take the offer back): record it, move on, and
    /// honour the sleep timer's "End of song".
    fn end_song(&mut self, now: Instant) {
        self.record_short_play();
        self.advance(false);
        if self.sleep == Some(Sleep::EndOfSong) {
            self.sleep = None;
            // `start_current` opens the song paused (or stops, when
            // nothing is left).
            self.paused = self.queue.current().is_some();
        }
        self.start_current(now);
    }

    /// Before a new generation (or no engine): take the offer back and let go
    /// of the chain.
    fn reclaim_offer(&mut self) {
        let _ = self.take_back_offer();
        self.chain = None;
    }

    /// The prefetch finished while the chain could take it: offer it.
    fn on_prefetched(&mut self, done: PrefetchDone, now: Instant) {
        let PrefetchDone { target, result } = done;
        let reader = match result {
            Ok(reader) => reader,
            Err(e) => {
                // Kept for the normal transition, which fails the song as today.
                self.offer_blocked = Some(target.clone());
                self.prefetch = Some(Prefetch {
                    target,
                    opening: Opening::ready(Err(e)),
                });
                return;
            }
        };
        let fade = self.fade_for_next(&target);
        self.offer_seq += 1;
        let seq = self.offer_seq;
        let refused = match self.chain.as_ref() {
            Some(chain) => chain.offer(reader, seq, fade).err(),
            None => Some(reader),
        };
        match refused {
            None => self.offer = Some((target, seq)),
            Some(reader) => {
                self.offer_blocked = Some(target.clone());
                self.prefetch = Some(Prefetch {
                    target,
                    opening: Opening::ready(Ok(reader)),
                });
            }
        }
        self.publish(now);
    }

    fn on_chain_event(&mut self, ev: ChainEvent, now: Instant) {
        let ChainEvent::Started {
            generation,
            seq,
            stream_at,
            lead,
        } = ev;
        if generation != self.generation {
            return;
        }
        let Some((target, offered_seq)) = self.offer.clone() else {
            return;
        };
        if offered_seq != seq {
            return;
        }
        self.record_short_play();
        self.prefetch = None;
        self.offer = None;
        self.advance(false);
        let matches = self.queue.current().is_some_and(|e| match &target {
            PrefetchFor::Entry(id) => e.id == *id,
            PrefetchFor::Suggestion(video_id) => e.song.video_id == *video_id,
        });
        if !matches {
            tracing::warn!("crossfade started a song that is not the current one");
            self.start_current(now);
            return;
        }
        self.played_recorded = false;
        self.remember_current();
        if let Some(entry) = self.queue.current() {
            self.engine.song(&entry.song);
            tracing::info!(
                title = %entry.song.title,
                ?stream_at,
                ?lead,
                "crossfade: started"
            );
        }
        self.failures = 0;
        self.clock = Clock {
            offset: lead,
            played_before: Duration::ZERO,
            resumed_at: (!self.paused).then_some(now),
            anchored: true,
            counted_before: Duration::ZERO,
        };
        self.update_prefetch(now);
        self.publish(now);
    }

    fn on_opened(&mut self, opened: Opened, now: Instant) {
        if opened.generation != self.generation {
            return;
        }
        let reader = match opened.result {
            Ok(reader) => reader,
            Err(e) => return self.fail_current(format!("{e:#}"), now),
        };
        if let Some(entry) = self.queue.current() {
            self.engine.song(&entry.song);
        }
        let reader: Box<dyn PcmReader> = if self.chaining() {
            let (chain, handle) = Chain::new(reader, self.generation, self.chain_tx.clone());
            self.chain = Some(handle);
            Box::new(chain)
        } else {
            self.chain = None;
            reader
        };
        // The sleep fade wraps the chain, so it fades every song the chain plays.
        let reader: Box<dyn PcmReader> = Box::new(Fade::new(reader, self.fade.clone()));
        self.offer_blocked = None;
        if let Err(e) = self.engine.play_from(reader, self.generation, opened.start) {
            self.chain = None;
            return self.fail_current(format!("{e:#}"), now);
        }
        self.failures = 0;
        self.started = true;
        self.engine_busy = true;
        self.clock = Clock {
            offset: opened.start,
            played_before: Duration::ZERO,
            resumed_at: Some(now),
            anchored: !self.engine.reports_position(),
            counted_before: Duration::ZERO,
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
        self.open_current(start, start.is_zero(), now);
    }

    /// Opens `current` at `start`. `fresh` marks a song starting over (not a
    /// seek or an output switch): it clears `played_recorded` and notes the
    /// song as recently played.
    fn open_current(&mut self, start: Duration, fresh: bool, now: Instant) {
        self.reclaim_offer();
        self.generation += 1;
        self.pending = None;
        // Until the reader opens, the position shows where it will start.
        self.clock = Clock {
            offset: start,
            ..Clock::default()
        };
        self.started = false;
        if fresh {
            self.played_recorded = false;
            self.remember_current();
        }
        match self.queue.current().cloned() {
            None => {
                self.paused = false;
                self.engine_busy = false;
                self.engine_call("stop", |e| e.stop());
            }
            Some(_) if !self.has_output => {
                self.paused = true;
                let _ = self.notices.send(Notice::NoOutput);
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
                if self.engine_busy && !opening.is_ready() {
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

    /// Adds the current song to `played_recently` (most recent last).
    fn remember_current(&mut self) {
        let Some(entry) = self.queue.current() else {
            return;
        };
        let id = entry.song.video_id.clone();
        self.played_recently.retain(|v| *v != id);
        self.played_recently.push_back(id);
        while self.played_recently.len() > RECENT_FILTER {
            self.played_recently.pop_front();
        }
    }

    fn fail_current(&mut self, reason: String, now: Instant) {
        self.failures += 1;
        if let Some(entry) = self.queue.current() {
            tracing::warn!(title = %entry.song.title, %reason, "song failed");
            let _ = self.notices.send(Notice::SongFailed {
                entry_id: entry.id,
                title: entry.song.title.clone(),
                reason,
            });
        }
        self.advance(true);
        self.start_current(now);
    }

    /// Moves on to the next song; a `failed` one isn't kept for Previous.
    /// With nothing upcoming and autoplay on,
    /// the first suggestion is queued first, so it plays like any other song
    /// (and uses its prefetched source).
    fn advance(&mut self, failed: bool) {
        // Keep the chain: the handover advances while the chain goes on.
        let _ = self.take_back_offer();
        if self.autoplay_on
            && self.failures < MAX_AUTOPLAY_FAILURES
            && self.queue.upcoming().is_empty()
            && let Some(song) = self.suggestions().into_iter().next()
        {
            let video_id = song.video_id.clone();
            self.queue.add_suggestion(song);
            self.rekey_prefetch(&video_id);
        }
        if failed {
            self.queue.advance_discarding();
        } else {
            self.queue.advance();
        }
        self.fill_loop();
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
        if !self.has_output || self.repeat == Repeat::One {
            return None;
        }
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
        let wanted = self.prefetch_target();
        if let Some((held, _)) = &self.offer
            && wanted.as_ref().map(|(t, _)| t) != Some(held)
        {
            // The queue changed under the offer: take it back and drop it.
            if self.take_back_offer() {
                self.end_song(now);
                return;
            }
            self.prefetch = None;
        }
        let Some((target, video_id)) = wanted else {
            self.prefetch = None;
            self.offer_blocked = None;
            return;
        };
        self.source.prepare(&video_id);
        if self.prefetch.as_ref().is_some_and(|p| p.target != target) {
            self.prefetch = None;
        }
        if self.prefetch.is_none() && self.offer_blocked.as_ref() != Some(&target) {
            self.offer_blocked = None;
        }
        if self.offer.is_some() {
            return;
        }
        if self.chain.as_ref().is_some_and(|c| c.mixing()) {
            // The chain refuses offers while it mixes; look again soon.
            self.prefetch_at = Some(now + MIX_POLL);
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
                let lead = self.prefetch_lead();
                if left > lead && self.clock.running() {
                    self.prefetch_at = Some(now + (left - lead));
                }
                left <= lead
            }
        };
        if due && self.prefetch.is_none() {
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
        let played = self.clock.listened(now);
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
        self.position_at = self.clock.moves_in(now, latency).map(|left| now + left);
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
            sleep: self.sleep,
            crossfade: self.crossfade,
            repeat: self.repeat,
            clock: self.clock,
        });
    }
}

/// `engine` is `None` until an output is chosen (see `PlayerHandle::set_output`).
pub fn spawn_player(
    engine: Option<Box<dyn AudioEngine>>,
    events: mpsc::UnboundedReceiver<EngineEvent>,
    source: Arc<dyn PcmSource>,
    autoplay: Autoplay,
    crossfade: Duration,
    repeat: Repeat,
) -> PlayerHandle {
    spawn_player_inner(engine, events, source, autoplay, crossfade, repeat).0
}

/// Like `spawn_player`, also returning the sender chain events come in on.
fn spawn_player_inner(
    engine: Option<Box<dyn AudioEngine>>,
    events: mpsc::UnboundedReceiver<EngineEvent>,
    source: Arc<dyn PcmSource>,
    autoplay: Autoplay,
    crossfade: Duration,
    repeat: Repeat,
) -> (PlayerHandle, mpsc::UnboundedSender<ChainEvent>) {
    let has_output = engine.is_some();
    let engine = engine.unwrap_or_else(|| Box::new(NullEngine));
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
        sleep: None,
        crossfade,
        repeat,
        clock: Clock::default(),
    };
    let (state_tx, state_rx) = watch::channel(initial);
    let (notices, _) = broadcast::channel(64);
    let (plays, _) = broadcast::channel(64);
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (chain_tx, chain_rx) = mpsc::unbounded_channel();
    let actor = Actor {
        queue: Queue::new(),
        engine,
        has_output,
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
        crossfade,
        repeat,
        looped: HashSet::new(),
        radio: autoplay.radio,
        history: autoplay.history,
        seed: None,
        radio_songs: Vec::new(),
        recent: HashSet::new(),
        based_on: None,
        fetch: None,
        failure_shown: false,
        failed_during: None,
        played_recently: VecDeque::new(),
        failures: 0,
        sleep: None,
        sleep_at: None,
        fade: FadeControl::default(),
        chain_tx,
        chain: None,
        offer: None,
        offer_seq: 0,
        offer_blocked: None,
    };
    let chain_events = actor.chain_tx.clone();
    tokio::spawn(actor.run(cmd_rx, events, chain_rx));
    let handle = PlayerHandle {
        commands: cmd_tx,
        state: state_rx,
        notices,
        plays,
    };
    (handle, chain_events)
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
        rig_engine(FakeEngine::new(latency), source, radio, enabled)
    }

    fn rig_engine(engine: FakeEngine, source: FakeSource, radio: FakeRadio, enabled: bool) -> Rig {
        let (tx, rx) = mpsc::unbounded_channel();
        let history = History::new(Db::in_memory());
        let player = spawn_player(
            Some(Box::new(engine.clone())),
            rx,
            Arc::new(source.clone()),
            Autoplay {
                radio: Arc::new(radio.clone()),
                history: history.clone(),
                enabled,
            },
            std::time::Duration::ZERO,
            crate::autoplay::Repeat::Off,
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
            // A seek records "play:A@<ms>"; it still counts as a play of A.
            let at = format!("{call}@");
            let seen = |c: &&String| *c == call || c.starts_with(&at);
            if engine.calls().iter().filter(seen).count() >= n {
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

    fn rig_no_output() -> Rig {
        let engine = FakeEngine::new(Duration::ZERO);
        let (tx, rx) = mpsc::unbounded_channel();
        let source = FakeSource::default();
        let radio = FakeRadio::default();
        let history = History::new(Db::in_memory());
        let player = spawn_player(
            None,
            rx,
            Arc::new(source.clone()),
            Autoplay {
                radio: Arc::new(radio.clone()),
                history: history.clone(),
                enabled: false,
            },
            std::time::Duration::ZERO,
            crate::autoplay::Repeat::Off,
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

    fn count_no_output(rx: &mut broadcast::Receiver<Notice>) -> usize {
        let mut n = 0;
        while let Ok(notice) = rx.try_recv() {
            if matches!(notice, Notice::NoOutput) {
                n += 1;
            }
        }
        n
    }

    #[test]
    fn no_output_notice_serializes() {
        assert_eq!(
            serde_json::to_string(&Notice::NoOutput).unwrap(),
            r#"{"type":"noOutput"}"#
        );
    }

    #[tokio::test(start_paused = true)]
    async fn no_output_keeps_song_paused_and_opens_nothing() {
        let r = rig_no_output();
        let mut rx = r.player.notices();
        r.player.play_next(song("A", Some(200))).await.unwrap();
        settle().await;
        let v = view(&r);
        assert_eq!(v.current.unwrap().song.video_id, "A");
        assert_eq!(v.status, Status::Paused);
        assert!(r.source.opens().is_empty());
        assert_eq!(count_no_output(&mut rx), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn resume_without_output_sends_no_output() {
        let r = rig_no_output();
        let mut rx = r.player.notices();
        r.player.play_next(song("A", Some(200))).await.unwrap();
        settle().await;
        assert_eq!(count_no_output(&mut rx), 1);
        r.player.resume().await.unwrap();
        settle().await;
        assert_eq!(view(&r).status, Status::Paused);
        assert_eq!(count_no_output(&mut rx), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn set_output_opens_current_paused() {
        let r = rig_no_output();
        r.player.play_next(song("A", Some(200))).await.unwrap();
        let e2 = FakeEngine::new(Duration::ZERO);
        r.player.set_output(Box::new(e2.clone())).await.unwrap();
        wait_call(&e2, "pause").await;
        assert_eq!(e2.calls(), ["play:A", "pause"]);
        assert_eq!(view(&r).status, Status::Paused);
    }

    #[tokio::test(start_paused = true)]
    async fn release_output_drops_the_engine_and_pauses() {
        let r = rig_with(FakeSource::default(), Duration::ZERO);
        r.player.play_next(song("A", Some(200))).await.unwrap();
        wait_call(&r.engine, "play:A").await;
        r.player.release_output().await.unwrap();
        assert_eq!(r.engine.calls().last().map(String::as_str), Some("stop"));
        assert_eq!(view(&r).status, Status::Paused);
        r.player.resume().await.unwrap();
        assert_eq!(r.engine.calls().last().map(String::as_str), Some("stop"));
        assert_eq!(view(&r).status, Status::Paused);
    }

    #[tokio::test(start_paused = true)]
    async fn switch_moves_playing_song_at_heard_position() {
        let r = rig_with(FakeSource::default(), Duration::from_millis(750));
        r.player.play_next(song("A", Some(200))).await.unwrap();
        wait_call(&r.engine, "play:A").await;
        tokio::time::sleep(Duration::from_secs(10)).await;
        let e2 = FakeEngine::new(Duration::ZERO);
        r.player.set_output(Box::new(e2.clone())).await.unwrap();
        wait_call(&e2, "play:A").await;
        assert_eq!(r.engine.calls().last().map(String::as_str), Some("stop"));
        assert_eq!(
            r.source.starts().last().cloned(),
            Some(("A".to_owned(), Duration::from_millis(9250)))
        );
        assert_eq!(view(&r).status, Status::Playing);
    }

    #[tokio::test(start_paused = true)]
    async fn switch_while_paused_stays_paused() {
        let r = rig();
        r.player.play_next(song("A", Some(200))).await.unwrap();
        wait_call(&r.engine, "play:A").await;
        tokio::time::sleep(Duration::from_secs(5)).await;
        r.player.pause().await.unwrap();
        let e2 = FakeEngine::new(Duration::ZERO);
        r.player.set_output(Box::new(e2.clone())).await.unwrap();
        wait_call(&e2, "pause").await;
        assert_eq!(e2.calls(), ["play:A@5000", "pause"]);
        assert_eq!(
            r.source.starts().last().cloned(),
            Some(("A".to_owned(), Duration::from_secs(5)))
        );
        assert_eq!(view(&r).status, Status::Paused);
    }

    #[tokio::test(start_paused = true)]
    async fn switch_during_open_discards_old_open() {
        let source = FakeSource {
            delay: Duration::from_secs(2),
            ..FakeSource::default()
        };
        let r = rig_with(source, Duration::ZERO);
        r.player.play_next(song("A", Some(200))).await.unwrap();
        tokio::time::sleep(Duration::from_secs(1)).await;
        let e2 = FakeEngine::new(Duration::ZERO);
        r.player.set_output(Box::new(e2.clone())).await.unwrap();
        wait_call(&e2, "play:A").await;
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert!(!r.engine.calls().contains(&"play:A".to_owned()));
        assert_eq!(e2.calls().iter().filter(|c| *c == "play:A").count(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn stale_event_from_old_engine_is_ignored() {
        let r = rig();
        r.player.play_next(song("A", Some(200))).await.unwrap();
        r.player.play_next(song("B", Some(200))).await.unwrap();
        wait_call(&r.engine, "play:A").await;
        let old = r.engine.generation();
        let e2 = FakeEngine::new(Duration::ZERO);
        r.player.set_output(Box::new(e2.clone())).await.unwrap();
        wait_call(&e2, "play:A").await;
        r.events
            .send(EngineEvent::TrackEnded { generation: old })
            .unwrap();
        settle().await;
        assert_eq!(current_id(&r).as_deref(), Some("A"));
    }

    #[tokio::test(start_paused = true)]
    async fn switch_does_not_record_play_twice() {
        let r = rig();
        let mut plays = r.player.plays();
        r.player.play_next(song("A", Some(200))).await.unwrap();
        wait_call(&r.engine, "play:A").await;
        tokio::time::sleep(Duration::from_secs(40)).await;
        let e2 = FakeEngine::new(Duration::ZERO);
        r.player.set_output(Box::new(e2.clone())).await.unwrap();
        wait_call(&e2, "play:A").await;
        tokio::time::sleep(Duration::from_secs(40)).await;
        settle().await;
        let mut ids = Vec::new();
        while let Ok(s) = plays.try_recv() {
            ids.push(s.video_id);
        }
        assert_eq!(ids, ["A"]);
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
    async fn player_passes_song_before_play() {
        let r = rig();
        r.player.play_next(song("a", Some(200))).await.unwrap();
        wait_call(&r.engine, "play:a").await;
        let calls = r.engine.all_calls();
        let at = calls.iter().position(|c| c == "play:a").unwrap();
        assert_eq!(calls[at - 1], "song:a", "{calls:?}");
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
    async fn previous_near_the_start_steps_back() {
        let r = rig();
        let a = r.player.play_next(song("A", Some(200))).await.unwrap();
        let x = r.player.play_next(song("X", Some(200))).await.unwrap();
        wait_call(&r.engine, "play:A").await;
        r.player.skip(None).await.unwrap();
        wait_call(&r.engine, "play:X").await;
        tokio::time::sleep(Duration::from_secs(1)).await;
        r.player.previous(None).await.unwrap();
        wait_call_n(&r.engine, "play:A", 2).await;
        let v = view(&r);
        assert_eq!(v.current.unwrap().id, a.id);
        assert_eq!(v.position_ms, 0);
        assert_eq!(v.status, Status::Playing);
        assert_eq!(v.upcoming.first().map(|e| e.id), Some(x.id));
    }

    #[tokio::test(start_paused = true)]
    async fn previous_twice_restarts_then_steps_back() {
        let r = rig();
        let a = r.player.play_next(song("A", Some(200))).await.unwrap();
        let x = r.player.play_next(song("X", Some(200))).await.unwrap();
        wait_call(&r.engine, "play:A").await;
        r.player.skip(None).await.unwrap();
        wait_call(&r.engine, "play:X").await;
        tokio::time::sleep(Duration::from_secs(30)).await;
        r.player.previous(None).await.unwrap();
        wait_call_n(&r.engine, "play:X", 2).await;
        assert_eq!(view(&r).current.unwrap().id, x.id);
        r.player.previous(None).await.unwrap();
        wait_call_n(&r.engine, "play:A", 2).await;
        assert_eq!(view(&r).current.unwrap().id, a.id);
    }

    #[tokio::test(start_paused = true)]
    async fn previous_without_history_restarts() {
        let r = rig();
        let a = r.player.play_next(song("A", Some(200))).await.unwrap();
        r.player.play_next(song("X", Some(200))).await.unwrap();
        wait_call(&r.engine, "play:A").await;
        r.player.previous(None).await.unwrap();
        wait_call_n(&r.engine, "play:A", 2).await;
        let v = view(&r);
        assert_eq!(v.current.unwrap().id, a.id);
        assert_eq!(v.upcoming.len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn previous_skips_a_song_that_failed_to_open() {
        let source = FakeSource {
            failing: ["X".to_owned()].into(),
            ..FakeSource::default()
        };
        let r = rig_with(source, Duration::ZERO);
        let a = r.player.play_next(song("A", Some(200))).await.unwrap();
        r.player.play_next(song("X", Some(200))).await.unwrap();
        r.player.play_next(song("Y", Some(200))).await.unwrap();
        wait_call(&r.engine, "play:A").await;
        end_track(&r).await;
        wait_call(&r.engine, "play:Y").await;
        r.player.previous(None).await.unwrap();
        wait_call_n(&r.engine, "play:A", 2).await;
        let v = view(&r);
        assert_eq!(v.current.unwrap().id, a.id);
        assert_eq!(v.upcoming.len(), 1);
        assert_eq!(v.upcoming[0].song.video_id, "Y");
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
    async fn seek_passes_start_to_engine() {
        let r = rig();
        r.player.play_next(song("A", Some(200))).await.unwrap();
        wait_call(&r.engine, "play:A").await;
        assert!(r.player.seek(Duration::from_secs(42), None).await.unwrap());
        wait_call(&r.engine, "play:A@42000").await;
        assert!(r.engine.calls().contains(&"play:A@42000".to_string()));
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
    async fn prefetch_target_is_prepared_when_a_song_starts() {
        let r = rig();
        r.player
            .add(vec![song("A", Some(100)), song("X", Some(100))], AddAt::End)
            .await
            .unwrap();
        wait_call(&r.engine, "play:A").await;
        settle().await;
        assert!(r.source.prepares().contains(&"X".to_owned()));
        assert!(
            !r.source.opens().contains(&"X".to_owned()),
            "prepared, not opened yet"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn prepare_follows_the_next_queued_entry() {
        let r = rig();
        r.player.play_next(song("A", Some(100))).await.unwrap();
        wait_call(&r.engine, "play:A").await;
        r.player
            .add(vec![song("X", Some(100))], AddAt::End)
            .await
            .unwrap();
        settle().await;
        assert_eq!(r.source.prepares().last().map(String::as_str), Some("X"));
        r.player
            .add(vec![song("Y", Some(100))], AddAt::Next)
            .await
            .unwrap();
        settle().await;
        assert_eq!(r.source.prepares().last().map(String::as_str), Some("Y"));
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
    async fn moves_in_counts_down_the_latency_window() {
        let r = rig_with(FakeSource::default(), Duration::from_millis(750));
        r.player.play_next(song("A", Some(200))).await.unwrap();
        wait_call(&r.engine, "play:A").await;
        let left = r.player.state().moves_in(Instant::now()).unwrap();
        assert!(left <= Duration::from_millis(750) && left >= Duration::from_millis(700));
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(r.player.state().moves_in(Instant::now()), None);

        r.player.pause().await.unwrap();
        assert_eq!(r.player.state().moves_in(Instant::now()), None);
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

    fn rig_reporting() -> Rig {
        rig_engine(
            FakeEngine::reporting(),
            FakeSource::default(),
            FakeRadio::default(),
            false,
        )
    }

    fn report(r: &Rig, generation: u64, position: Duration) {
        r.events
            .send(EngineEvent::Playing {
                generation,
                position,
                at: Instant::now(),
            })
            .unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn reporting_engine_holds_position_until_report() {
        let r = rig_reporting();
        r.player.play_next(song("A", Some(200))).await.unwrap();
        wait_call(&r.engine, "play:A").await;
        tokio::time::sleep(Duration::from_secs(3)).await;
        let v = view(&r);
        assert_eq!(v.position_ms, 0);
        assert!(!v.position_running);
        assert_eq!(r.player.state().moves_in(Instant::now()), None);
    }

    #[tokio::test(start_paused = true)]
    async fn report_reanchors_position() {
        let r = rig_reporting();
        r.player.play_next(song("A", Some(200))).await.unwrap();
        wait_call(&r.engine, "play:A").await;
        tokio::time::sleep(Duration::from_secs(3)).await;
        let generation = r.engine.generation();
        report(&r, generation, Duration::ZERO);
        settle().await;
        tokio::time::sleep(Duration::from_secs(2)).await;
        let v = view(&r);
        assert_eq!(v.position_ms, 2000);
        assert!(v.position_running);
        report(&r, generation, Duration::from_secs(10));
        settle().await;
        assert_eq!(view(&r).position_ms, 10_000);
    }

    #[tokio::test(start_paused = true)]
    async fn report_for_stale_generation_is_ignored() {
        let r = rig_reporting();
        r.player.play_next(song("A", Some(200))).await.unwrap();
        wait_call(&r.engine, "play:A").await;
        let generation = r.engine.generation();
        report(&r, generation - 1, Duration::from_secs(9));
        settle().await;
        let v = view(&r);
        assert_eq!(v.position_ms, 0);
        assert!(!v.position_running);
    }

    #[tokio::test(start_paused = true)]
    async fn report_while_paused_keeps_position_still() {
        let r = rig_reporting();
        r.player.play_next(song("A", Some(200))).await.unwrap();
        wait_call(&r.engine, "play:A").await;
        let generation = r.engine.generation();
        r.player.pause().await.unwrap();
        report(&r, generation, Duration::from_secs(4));
        settle().await;
        tokio::time::sleep(Duration::from_secs(2)).await;
        let v = view(&r);
        assert_eq!(v.position_ms, 4000);
        assert!(!v.position_running);
        r.player.resume().await.unwrap();
        tokio::time::sleep(Duration::from_secs(1)).await;
        let v = view(&r);
        assert_eq!(v.position_ms, 5000);
        assert!(v.position_running);
    }

    #[tokio::test(start_paused = true)]
    async fn periodic_reports_still_record_play() {
        let r = rig_reporting();
        let mut rx = r.player.plays();
        r.player.play_next(song("A", Some(200))).await.unwrap();
        wait_call(&r.engine, "play:A").await;
        let generation = r.engine.generation();
        for i in 0..=4u64 {
            report(&r, generation, Duration::from_secs(i * 10));
            settle().await;
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
        settle().await;
        let mut got = vec![];
        drain(&mut rx, &mut got);
        assert_eq!(got, ["A"]);
    }

    #[tokio::test(start_paused = true)]
    async fn switch_from_reporting_engine_before_report_uses_offset() {
        let r = rig_reporting();
        r.player.play_next(song("A", Some(200))).await.unwrap();
        wait_call(&r.engine, "play:A").await;
        tokio::time::sleep(Duration::from_secs(3)).await;
        r.player
            .set_output(Box::new(FakeEngine::new(Duration::ZERO)))
            .await
            .unwrap();
        settle().await;
        assert_eq!(
            r.source.starts().last().cloned(),
            Some(("A".to_string(), Duration::ZERO))
        );
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

    #[tokio::test(start_paused = true)]
    async fn just_played_song_does_not_come_back() {
        let radio = FakeRadio::default();
        radio.set("A", ["A", "p"].map(radio_song).into());
        radio.set("B", ["B", "A", "k"].map(radio_song).into());
        let r = rig_autoplay(radio);
        let _rec = crate::history::spawn_recorder(r.history.clone(), r.player.plays());
        r.player.play_next(song("A", Some(200))).await.unwrap();
        wait_call(&r.engine, "play:A").await;
        r.player
            .add(vec![song("B", Some(200))], AddAt::End)
            .await
            .unwrap();
        quiesce().await;
        tokio::time::sleep(Duration::from_secs(31)).await;
        quiesce().await;
        end_track(&r).await;
        wait_call(&r.engine, "play:B").await;
        quiesce().await;
        assert_eq!(suggested(&r), ["k"]);
        end_track(&r).await;
        wait_call(&r.engine, "play:k").await;
        assert_eq!(current_id(&r).as_deref(), Some("k"));
    }

    #[tokio::test(start_paused = true)]
    async fn album_just_played_does_not_come_back() {
        let radio = FakeRadio::default();
        radio.set("C", ["C", "A", "B", "k"].map(radio_song).into());
        let r = rig_autoplay(radio);
        r.player
            .play_all(["A", "B", "C"].map(|id| song(id, Some(200))).into())
            .await
            .unwrap();
        wait_call(&r.engine, "play:A").await;
        end_track(&r).await;
        wait_call(&r.engine, "play:B").await;
        end_track(&r).await;
        wait_call(&r.engine, "play:C").await;
        quiesce().await;
        assert_eq!(suggested(&r), ["k"]);
    }

    #[tokio::test(start_paused = true)]
    async fn skipped_song_does_not_come_back() {
        let radio = FakeRadio::default();
        radio.set("A", ["A", "x"].map(radio_song).into());
        radio.set("x", ["x", "A", "w"].map(radio_song).into());
        let r = rig_autoplay(radio);
        r.player.play_next(song("A", Some(200))).await.unwrap();
        wait_call(&r.engine, "play:A").await;
        quiesce().await;
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert!(r.player.skip(None).await.unwrap());
        wait_call(&r.engine, "play:x").await;
        quiesce().await;
        assert_eq!(suggested(&r), ["w"]);
    }

    #[tokio::test(start_paused = true)]
    async fn autoplay_stops_after_repeated_failures() {
        let mut source = FakeSource {
            delay: Duration::from_secs(2),
            ..FakeSource::default()
        };
        for id in ["X", "Y", "Z", "W"] {
            source.failing.insert(id.into());
        }
        let radio = FakeRadio::default();
        radio.set("A", ["A", "X"].map(radio_song).into());
        radio.set("X", ["X", "Y"].map(radio_song).into());
        radio.set("Y", ["Y", "Z"].map(radio_song).into());
        radio.set("Z", ["Z", "W"].map(radio_song).into());
        radio.set("W", ["W", "V"].map(radio_song).into());
        let r = rig_built(source, Duration::ZERO, radio, true);
        let mut notices = r.player.notices();
        r.player.play_next(song("A", Some(200))).await.unwrap();
        wait_call(&r.engine, "play:A").await;
        quiesce().await;
        end_track(&r).await;
        for _ in 0..600 {
            settle().await;
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let mut failed = 0;
        while let Ok(n) = notices.try_recv() {
            if matches!(n, Notice::SongFailed { .. }) {
                failed += 1;
            }
        }
        assert_eq!(failed, MAX_AUTOPLAY_FAILURES);
        assert_eq!(view(&r).status, Status::Stopped);
        assert_eq!(r.source.opens(), ["A", "X", "Y", "Z"]);
    }

    #[tokio::test(start_paused = true)]
    async fn stalled_radio_fails_once_after_the_timeout() {
        let radio = FakeRadio::default();
        radio.set("A", ["A", "x"].map(radio_song).into());
        radio.delay("A", RADIO_TIMEOUT + Duration::from_secs(5));
        let r = rig_autoplay(radio);
        let mut notices = r.player.notices();
        r.player.play_next(song("A", Some(200))).await.unwrap();
        wait_call(&r.engine, "play:A").await;
        tokio::time::sleep(RADIO_TIMEOUT + Duration::from_secs(10)).await;
        quiesce().await;
        assert_eq!(drain_notices(&mut notices), 1);
        assert!(suggested(&r).is_empty());
    }

    fn tone_rig(value: i16) -> Rig {
        let source = FakeSource {
            tone: Some(value),
            ..FakeSource::default()
        };
        rig_with(source, Duration::ZERO)
    }

    /// The first sample the engine's reader gives right now.
    async fn sample_now(r: &Rig) -> i16 {
        let mut reader = r.engine.last_reader.lock().unwrap().take().unwrap();
        let mut buf = [0u8; 4];
        let n = reader.read(&mut buf).await.unwrap();
        assert_eq!(n, 4);
        *r.engine.last_reader.lock().unwrap() = Some(reader);
        i16::from_le_bytes([buf[0], buf[1]])
    }

    fn sleep_json(r: &Rig) -> serde_json::Value {
        serde_json::to_value(view(r)).unwrap()["sleep"].clone()
    }

    #[tokio::test(start_paused = true)]
    async fn sleep_is_null_by_default() {
        let r = rig();
        assert_eq!(sleep_json(&r), serde_json::Value::Null);
    }

    #[tokio::test(start_paused = true)]
    async fn timed_sleep_shows_remaining_time() {
        let r = tone_rig(10000);
        r.player.play_next(song("A", Some(600))).await.unwrap();
        wait_call(&r.engine, "play:A").await;
        r.player
            .set_sleep(Some(SleepRequest::Minutes(1)))
            .await
            .unwrap();
        assert_eq!(sleep_json(&r), serde_json::json!({"remainingMs": 60000}));
        tokio::time::advance(Duration::from_secs(10)).await;
        assert_eq!(sleep_json(&r), serde_json::json!({"remainingMs": 50000}));
    }

    #[tokio::test(start_paused = true)]
    async fn end_of_song_sleep_shows_in_state() {
        let r = rig();
        r.player
            .set_sleep(Some(SleepRequest::EndOfSong))
            .await
            .unwrap();
        assert_eq!(sleep_json(&r), serde_json::json!({"endOfSong": true}));
    }

    #[tokio::test(start_paused = true)]
    async fn timed_sleep_pauses_at_the_deadline() {
        let r = tone_rig(10000);
        r.player.play_next(song("A", Some(600))).await.unwrap();
        wait_call(&r.engine, "play:A").await;
        r.player
            .set_sleep(Some(SleepRequest::Minutes(1)))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_secs(59)).await;
        settle().await;
        assert_eq!(view(&r).status, Status::Playing);
        tokio::time::sleep(Duration::from_secs(2)).await;
        settle().await;
        let v = view(&r);
        assert_eq!(v.status, Status::Paused);
        assert_eq!(v.current.unwrap().song.video_id, "A");
        assert_eq!(sleep_json(&r), serde_json::Value::Null);
        assert_eq!(r.engine.calls().last().map(String::as_str), Some("pause"));
        // Play afterwards is at full volume.
        r.player.resume().await.unwrap();
        assert_eq!(sample_now(&r).await, 10000);
    }

    #[tokio::test(start_paused = true)]
    async fn fade_runs_over_the_last_30_seconds() {
        let r = tone_rig(10000);
        r.player.play_next(song("A", Some(600))).await.unwrap();
        wait_call(&r.engine, "play:A").await;
        r.player
            .set_sleep(Some(SleepRequest::Minutes(1)))
            .await
            .unwrap();
        assert_eq!(sample_now(&r).await, 10000);
        tokio::time::advance(Duration::from_secs(30)).await;
        assert_eq!(sample_now(&r).await, 10000);
        tokio::time::advance(Duration::from_secs(15)).await;
        assert_eq!(sample_now(&r).await, 5000);
        tokio::time::advance(Duration::from_secs(7)).await;
        let late = sample_now(&r).await;
        assert!((2660..=2670).contains(&late), "{late}");
    }

    /// Plays A with a 60 s fade-in armed (the tone rig starts silent).
    async fn fading_in(r: &Rig) {
        r.player
            .play_all_with_fade_in(
                vec![song("A", Some(600)), song("B", Some(600))],
                Duration::from_secs(60),
            )
            .await
            .unwrap();
        wait_call(&r.engine, "play:A").await;
        assert_eq!(sample_now(r).await, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn play_all_with_fade_in_arms_fade() {
        let r = tone_rig(10000);
        fading_in(&r).await;
    }

    #[tokio::test(start_paused = true)]
    async fn plain_play_all_clears_armed_fade_in() {
        let r = tone_rig(10000);
        r.player
            .play_all_with_fade_in(vec![song("A", Some(600))], Duration::from_secs(60))
            .await
            .unwrap();
        // The song never opened; the owner presses play later.
        r.player.play_all(vec![song("B", Some(600))]).await.unwrap();
        wait_call(&r.engine, "play:B").await;
        assert_eq!(sample_now(&r).await, 10000);
    }

    #[tokio::test(start_paused = true)]
    async fn skip_ends_fade_in() {
        let r = tone_rig(10000);
        fading_in(&r).await;
        r.player.skip(None).await.unwrap();
        wait_call(&r.engine, "play:B").await;
        assert_eq!(sample_now(&r).await, 10000);
    }

    #[tokio::test(start_paused = true)]
    async fn pause_ends_fade_in() {
        let r = tone_rig(10000);
        fading_in(&r).await;
        r.player.pause().await.unwrap();
        assert_eq!(sample_now(&r).await, 10000);
    }

    #[tokio::test(start_paused = true)]
    async fn seek_ends_fade_in() {
        let r = tone_rig(10000);
        fading_in(&r).await;
        r.player.seek(Duration::from_secs(10), None).await.unwrap();
        settle().await;
        assert_eq!(sample_now(&r).await, 10000);
    }

    #[tokio::test(start_paused = true)]
    async fn natural_track_end_keeps_fade_in() {
        let r = tone_rig(10000);
        fading_in(&r).await;
        end_track(&r).await;
        wait_call(&r.engine, "play:B").await;
        assert_eq!(sample_now(&r).await, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn set_sleep_keeps_fade_in() {
        let r = tone_rig(10000);
        fading_in(&r).await;
        r.player
            .set_sleep(Some(SleepRequest::Minutes(10)))
            .await
            .unwrap();
        r.player.set_sleep(None).await.unwrap();
        assert_eq!(sample_now(&r).await, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn fade_covers_the_next_song_too() {
        let r = tone_rig(10000);
        r.player.play_next(song("A", Some(600))).await.unwrap();
        r.player
            .add(vec![song("B", Some(600))], AddAt::End)
            .await
            .unwrap();
        wait_call(&r.engine, "play:A").await;
        r.player
            .set_sleep(Some(SleepRequest::Minutes(1)))
            .await
            .unwrap();
        tokio::time::advance(Duration::from_secs(45)).await;
        r.player.skip(None).await.unwrap();
        wait_call(&r.engine, "play:B").await;
        assert_eq!(sample_now(&r).await, 5000);
    }

    #[tokio::test(start_paused = true)]
    async fn cancelling_clears_the_timer_and_the_fade() {
        let r = tone_rig(10000);
        r.player.play_next(song("A", Some(600))).await.unwrap();
        wait_call(&r.engine, "play:A").await;
        r.player
            .set_sleep(Some(SleepRequest::Minutes(1)))
            .await
            .unwrap();
        tokio::time::advance(Duration::from_secs(45)).await;
        r.player.set_sleep(None).await.unwrap();
        assert_eq!(sleep_json(&r), serde_json::Value::Null);
        assert_eq!(sample_now(&r).await, 10000);
        tokio::time::sleep(Duration::from_secs(120)).await;
        settle().await;
        assert_eq!(view(&r).status, Status::Playing);
    }

    #[tokio::test(start_paused = true)]
    async fn deadline_while_paused_just_clears_the_timer() {
        let r = tone_rig(10000);
        r.player.play_next(song("A", Some(600))).await.unwrap();
        wait_call(&r.engine, "play:A").await;
        r.player
            .set_sleep(Some(SleepRequest::Minutes(1)))
            .await
            .unwrap();
        r.player.pause().await.unwrap();
        tokio::time::sleep(Duration::from_secs(70)).await;
        settle().await;
        assert_eq!(sleep_json(&r), serde_json::Value::Null);
        assert_eq!(view(&r).status, Status::Paused);
        let pauses = r.engine.calls().iter().filter(|c| *c == "pause").count();
        assert_eq!(pauses, 1);
        assert_eq!(sample_now(&r).await, 10000);
    }

    #[tokio::test(start_paused = true)]
    async fn end_of_song_leaves_the_next_entry_paused() {
        let r = rig();
        r.player.play_next(song("A", Some(200))).await.unwrap();
        r.player
            .add(vec![song("B", Some(200))], AddAt::End)
            .await
            .unwrap();
        wait_call(&r.engine, "play:A").await;
        r.player
            .set_sleep(Some(SleepRequest::EndOfSong))
            .await
            .unwrap();
        end_track(&r).await;
        wait_call(&r.engine, "play:B").await;
        settle().await;
        let v = view(&r);
        assert_eq!(v.current.unwrap().song.video_id, "B");
        assert_eq!(v.status, Status::Paused);
        assert_eq!(v.position_ms, 0);
        assert_eq!(sleep_json(&r), serde_json::Value::Null);
        assert_eq!(r.engine.calls().last().map(String::as_str), Some("pause"));
        // Play starts it.
        r.player.resume().await.unwrap();
        assert_eq!(view(&r).status, Status::Playing);
    }

    #[tokio::test(start_paused = true)]
    async fn skip_does_not_trigger_end_of_song() {
        let r = rig();
        r.player.play_next(song("A", Some(200))).await.unwrap();
        r.player
            .add(vec![song("B", Some(200)), song("C", Some(200))], AddAt::End)
            .await
            .unwrap();
        wait_call(&r.engine, "play:A").await;
        r.player
            .set_sleep(Some(SleepRequest::EndOfSong))
            .await
            .unwrap();
        r.player.skip(None).await.unwrap();
        wait_call(&r.engine, "play:B").await;
        settle().await;
        assert_eq!(view(&r).status, Status::Playing);
        assert_eq!(sleep_json(&r), serde_json::json!({"endOfSong": true}));
        r.player.previous(None).await.unwrap();
        settle().await;
        assert_eq!(view(&r).status, Status::Playing);
        assert_eq!(sleep_json(&r), serde_json::json!({"endOfSong": true}));
        // The natural end of the current song then triggers it.
        end_track(&r).await;
        settle().await;
        assert_eq!(view(&r).status, Status::Paused);
        assert_eq!(sleep_json(&r), serde_json::Value::Null);
    }

    #[tokio::test(start_paused = true)]
    async fn end_of_song_with_nothing_next_just_stops() {
        let r = rig();
        r.player.play_next(song("A", Some(200))).await.unwrap();
        wait_call(&r.engine, "play:A").await;
        r.player
            .set_sleep(Some(SleepRequest::EndOfSong))
            .await
            .unwrap();
        end_track(&r).await;
        let v = view(&r);
        assert_eq!(v.status, Status::Stopped);
        assert_eq!(sleep_json(&r), serde_json::Value::Null);
    }

    #[tokio::test(start_paused = true)]
    async fn end_of_song_covers_the_autoplay_handover() {
        let radio = FakeRadio::default();
        radio.set("A", ["A", "S1", "S2"].map(radio_song).into());
        let r = rig_autoplay(radio);
        r.player.play_next(song("A", Some(200))).await.unwrap();
        wait_call(&r.engine, "play:A").await;
        quiesce().await;
        r.player
            .set_sleep(Some(SleepRequest::EndOfSong))
            .await
            .unwrap();
        end_track(&r).await;
        wait_call(&r.engine, "play:S1").await;
        settle().await;
        let v = view(&r);
        assert_eq!(v.current.unwrap().song.video_id, "S1");
        assert_eq!(v.status, Status::Paused);
        assert_eq!(sleep_json(&r), serde_json::Value::Null);
    }

    // ---- crossfade chain ----

    struct XfRig {
        player: PlayerHandle,
        engine: FakeEngine,
        source: FakeSource,
        chain_tx: mpsc::UnboundedSender<ChainEvent>,
        _events: mpsc::UnboundedSender<EngineEvent>,
    }

    fn xf_rig_with(engine: FakeEngine, pcm_secs: u32, crossfade_secs: u64) -> XfRig {
        let (tx, rx) = mpsc::unbounded_channel();
        let source = FakeSource {
            pcm_secs,
            ..FakeSource::default()
        };
        let (player, chain_tx) = spawn_player_inner(
            Some(Box::new(engine.clone())),
            rx,
            Arc::new(source.clone()),
            Autoplay {
                radio: Arc::new(FakeRadio::default()),
                history: History::new(Db::in_memory()),
                enabled: false,
            },
            Duration::from_secs(crossfade_secs),
            Repeat::Off,
        );
        XfRig {
            player,
            engine,
            source,
            chain_tx,
            _events: tx,
        }
    }

    fn xf_rig(pcm_secs: u32) -> XfRig {
        xf_rig_with(FakeEngine::crossfading(), pcm_secs, 6)
    }

    fn xf_current(r: &XfRig) -> Option<String> {
        r.player
            .state()
            .view(Instant::now())
            .current
            .map(|e| e.song.video_id)
    }

    /// Reads the chain like an engine until it ends; returns the bytes read.
    async fn drain_chain(r: &XfRig) -> usize {
        let mut chain = r.engine.take_chain().expect("a chain was handed over");
        let mut buf = vec![0u8; 64 * 1024];
        let mut total = 0;
        loop {
            let n = chain.read(&mut buf).await.unwrap();
            if n == 0 {
                return total;
            }
            total += n;
        }
    }

    const SEC_BYTES: usize = 48_000 * 4;

    #[tokio::test(start_paused = true)]
    async fn chain_is_handed_to_a_crossfading_engine() {
        let r = xf_rig(20);
        r.player.play_next(song("A", Some(20))).await.unwrap();
        wait_call(&r.engine, "play:chain").await;
        assert_eq!(r.engine.calls(), ["play:chain"]);
    }

    #[tokio::test(start_paused = true)]
    async fn no_chain_without_crossfade_or_for_other_engines() {
        for (engine, secs) in [
            (FakeEngine::crossfading(), 0),
            (FakeEngine::default(), 6),
            (
                FakeEngine {
                    reports: true,
                    ..FakeEngine::crossfading()
                },
                6,
            ),
        ] {
            let r = xf_rig_with(engine, 0, secs);
            r.player.play_next(song("A", Some(20))).await.unwrap();
            wait_call(&r.engine, "play:A").await;
            assert_eq!(r.engine.calls(), ["play:A"]);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn prefetch_opens_earlier_with_crossfade() {
        let r = xf_rig(40);
        r.player.play_next(song("A", Some(40))).await.unwrap();
        r.player.play_next(song("B", Some(40))).await.unwrap();
        wait_call(&r.engine, "play:chain").await;
        tokio::time::sleep(Duration::from_secs(8)).await;
        settle().await;
        assert!(r.source.opens().iter().all(|v| v == "A"));
        tokio::time::sleep(Duration::from_secs(5)).await;
        settle().await;
        assert_eq!(r.source.opens(), ["A", "B"]);
        // Without crossfade the same song opens only 15 s before the end.
        let r = xf_rig_with(FakeEngine::crossfading(), 40, 0);
        r.player.play_next(song("A", Some(40))).await.unwrap();
        r.player.play_next(song("B", Some(40))).await.unwrap();
        wait_call(&r.engine, "play:A").await;
        tokio::time::sleep(Duration::from_secs(15)).await;
        settle().await;
        assert_eq!(r.source.opens(), ["A"]);
    }

    #[tokio::test(start_paused = true)]
    async fn handover_advances_without_a_new_play() {
        let r = xf_rig(20);
        let mut plays = r.player.plays();
        r.player.play_next(song("A", Some(20))).await.unwrap();
        r.player.play_next(song("B", Some(40))).await.unwrap();
        wait_call(&r.engine, "play:chain").await;
        settle().await;
        let generation = r.engine.generation();
        let total = drain_chain(&r).await;
        settle().await;
        assert_eq!(xf_current(&r).as_deref(), Some("B"));
        assert_eq!(r.engine.calls(), ["play:chain"]);
        assert_eq!(r.engine.generation(), generation);
        assert_eq!(r.engine.all_calls().last().unwrap(), "song:B");
        // 20 s + 20 s of B, overlapped by the fade.
        assert!(total < 40 * SEC_BYTES, "{total}");
        let view = r.player.state().view(Instant::now());
        assert!(view.position_ms < 2000, "{}", view.position_ms);
        assert!(view.position_running);
        // A was short: it counted when B took over.
        assert_eq!(plays.try_recv().unwrap().video_id, "A");
        // B counts after being heard for 30 s, like any song.
        tokio::time::sleep(Duration::from_secs(31)).await;
        settle().await;
        assert_eq!(plays.try_recv().unwrap().video_id, "B");
        assert!(plays.try_recv().is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn same_album_is_offered_gapless() {
        let r = xf_rig(20);
        let on_album = |id: &str, album: &str| Song {
            album: Some(album.into()),
            ..song(id, Some(20))
        };
        r.player.play_next(on_album("A", "Live")).await.unwrap();
        r.player.play_next(on_album("B", " live ")).await.unwrap();
        wait_call(&r.engine, "play:chain").await;
        settle().await;
        // No overlap: the whole of both songs comes out.
        assert_eq!(drain_chain(&r).await, 40 * SEC_BYTES);
        settle().await;
        assert_eq!(xf_current(&r).as_deref(), Some("B"));
    }

    #[tokio::test(start_paused = true)]
    async fn stale_chain_events_are_ignored() {
        let r = xf_rig(20);
        r.player.play_next(song("A", Some(20))).await.unwrap();
        r.player.play_next(song("B", Some(20))).await.unwrap();
        wait_call(&r.engine, "play:chain").await;
        settle().await;
        let generation = r.engine.generation();
        for (generation, seq) in [(generation + 1, 1), (generation, 99)] {
            r.chain_tx
                .send(ChainEvent::Started {
                    generation,
                    seq,
                    stream_at: Duration::ZERO,
                    lead: Duration::ZERO,
                })
                .unwrap();
        }
        settle().await;
        assert_eq!(xf_current(&r).as_deref(), Some("A"));
        assert_eq!(r.player.state().upcoming.len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn skip_while_offered_reuses_the_reader() {
        let r = xf_rig(20);
        for id in ["A", "B", "C"] {
            r.player.play_next(song(id, Some(20))).await.unwrap();
        }
        // play_next puts each one right after the current: A, C, B, ...
        wait_call(&r.engine, "play:chain").await;
        settle().await;
        let next = r.player.state().upcoming[0].song.video_id.clone();
        assert_eq!(r.source.opens(), ["A", next.as_str()]);
        assert!(r.player.skip(None).await.unwrap());
        wait_call_n(&r.engine, "play:chain", 2).await;
        assert_eq!(xf_current(&r).as_deref(), Some(next.as_str()));
        assert_eq!(r.source.opens().iter().filter(|v| **v == next).count(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn removing_the_offered_song_withdraws_it() {
        let r = xf_rig(20);
        r.player
            .add(
                vec![
                    song("A", Some(20)),
                    song("B", Some(20)),
                    song("C", Some(20)),
                ],
                AddAt::End,
            )
            .await
            .unwrap();
        wait_call(&r.engine, "play:chain").await;
        settle().await;
        let b = r.player.state().upcoming[0].id;
        assert!(r.player.remove(b).await.unwrap());
        settle().await;
        let total = drain_chain(&r).await;
        settle().await;
        assert_eq!(xf_current(&r).as_deref(), Some("C"));
        assert!(total < 40 * SEC_BYTES);
        assert_eq!(r.source.opens(), ["A", "B", "C"]);
    }

    #[tokio::test(start_paused = true)]
    async fn turning_crossfade_off_takes_the_offer_back() {
        let r = xf_rig(20);
        r.player.play_next(song("A", Some(20))).await.unwrap();
        r.player.play_next(song("B", Some(20))).await.unwrap();
        wait_call(&r.engine, "play:chain").await;
        settle().await;
        r.player.set_crossfade(Duration::ZERO).await.unwrap();
        settle().await;
        // The chain plays A alone; the player goes on the normal way.
        assert_eq!(drain_chain(&r).await, 20 * SEC_BYTES);
        assert_eq!(xf_current(&r).as_deref(), Some("A"));
        r._events
            .send(EngineEvent::TrackEnded {
                generation: r.engine.generation(),
            })
            .unwrap();
        // B's reader came back wrapped by the chain, so the fake engine can't name it.
        wait_call(&r.engine, "play:?").await;
        assert_eq!(xf_current(&r).as_deref(), Some("B"));
        assert_eq!(r.source.opens(), ["A", "B"]);
    }

    /// Ends the chain's song the way the engine would, after the chain ran out.
    fn xf_track_ended(r: &XfRig) {
        r._events
            .send(EngineEvent::TrackEnded {
                generation: r.engine.generation(),
            })
            .unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn end_of_song_sleep_takes_the_offer_back() {
        let r = xf_rig(20);
        r.player.play_next(song("A", Some(20))).await.unwrap();
        r.player.play_next(song("B", Some(20))).await.unwrap();
        wait_call(&r.engine, "play:chain").await;
        settle().await;
        r.player
            .set_sleep(Some(SleepRequest::EndOfSong))
            .await
            .unwrap();
        settle().await;
        // A really ends: no mix, and B waits paused.
        assert_eq!(drain_chain(&r).await, 20 * SEC_BYTES);
        xf_track_ended(&r);
        // B gets a chain of its own, opened paused.
        wait_call_n(&r.engine, "play:chain", 2).await;
        settle().await;
        assert_eq!(xf_current(&r).as_deref(), Some("B"));
        assert_eq!(r.player.state().status, Status::Paused);
        assert_eq!(r.source.opens(), ["A", "B"]);
    }

    #[tokio::test(start_paused = true)]
    async fn no_offer_while_end_of_song_is_set() {
        let r = xf_rig(20);
        r.player
            .set_sleep(Some(SleepRequest::EndOfSong))
            .await
            .unwrap();
        r.player.play_next(song("A", Some(20))).await.unwrap();
        r.player.play_next(song("B", Some(20))).await.unwrap();
        wait_call(&r.engine, "play:chain").await;
        settle().await;
        assert_eq!(drain_chain(&r).await, 20 * SEC_BYTES);
        xf_track_ended(&r);
        settle().await;
        assert_eq!(xf_current(&r).as_deref(), Some("B"));
        assert_eq!(r.player.state().status, Status::Paused);
    }

    #[tokio::test(start_paused = true)]
    async fn sleep_fade_wraps_the_chain() {
        let r = xf_rig(600);
        r.player.play_next(song("A", Some(600))).await.unwrap();
        wait_call(&r.engine, "play:chain").await;
        r.player
            .set_sleep(Some(SleepRequest::Minutes(1)))
            .await
            .unwrap();
        tokio::time::advance(Duration::from_secs(45)).await;
        let mut chain = r.engine.take_chain().expect("a chain was handed over");
        let mut buf = [0u8; 4];
        let mut n = 0;
        while n < 4 {
            n += chain.read(&mut buf[n..]).await.unwrap();
        }
        // Halfway through the 30 s fade: half of the fake song's 1000.
        assert_eq!(i16::from_le_bytes([buf[0], buf[1]]), 500);
    }

    #[tokio::test(start_paused = true)]
    async fn three_songs_crossfade_twice() {
        let r = xf_rig(20);
        for id in ["A", "B", "C"] {
            r.player
                .add(vec![song(id, Some(20))], AddAt::End)
                .await
                .unwrap();
        }
        wait_call(&r.engine, "play:chain").await;
        settle().await;
        let mut chain = r.engine.take_chain().unwrap();
        let mut buf = vec![0u8; 64 * 1024];
        let mut total = 0;
        let mut joins = 0;
        let mut since_join = 0;
        loop {
            let n = chain.read(&mut buf).await.unwrap();
            if n == 0 {
                break;
            }
            total += n;
            settle().await;
            let now = xf_current(&r).unwrap();
            if joins == 0 && now == "B" {
                joins = 1;
            } else if joins == 1 {
                since_join += 1;
                if since_join == 30 {
                    // The mix (6 s of audio) is over; real time has passed too.
                    tokio::time::sleep(Duration::from_secs(8)).await;
                    settle().await;
                }
                if now == "C" {
                    joins = 2;
                }
            }
        }
        settle().await;
        assert_eq!(joins, 2);
        assert_eq!(xf_current(&r).as_deref(), Some("C"));
        assert_eq!(r.engine.calls(), ["play:chain"]);
        assert!(total < 60 * SEC_BYTES);
        assert_eq!(r.source.opens(), ["A", "B", "C"]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_join_before_the_player_hears_of_it_still_moves_on() {
        let r = xf_rig(10);
        let on_album = |id: &str| Song {
            album: Some("Live".into()),
            ..song(id, Some(10))
        };
        r.player
            .add(
                vec![on_album("A"), on_album("B"), on_album("C")],
                AddAt::End,
            )
            .await
            .unwrap();
        wait_call(&r.engine, "play:chain").await;
        settle().await;
        // The chain joins B (gapless) without yielding, so the actor hasn't seen
        // the event when the next command arrives.
        let mut chain = r.engine.take_chain().unwrap();
        let mut buf = vec![0u8; 64 * 1024];
        let mut total = 0;
        while total < 11 * SEC_BYTES {
            total += chain.read(&mut buf).await.unwrap();
        }
        r.player.set_crossfade(Duration::ZERO).await.unwrap();
        settle().await;
        // Either order of the two messages ends on B, with C next.
        assert_eq!(xf_current(&r).as_deref(), Some("B"));
        assert_eq!(r.player.state().upcoming.len(), 1);
    }

    fn upcoming_ids(r: &Rig) -> Vec<String> {
        view(r)
            .upcoming
            .into_iter()
            .map(|e| e.song.video_id)
            .collect()
    }

    async fn queue_ab(r: &Rig) {
        r.player.play_next(song("A", Some(200))).await.unwrap();
        r.player
            .add(vec![song("B", Some(200))], AddAt::End)
            .await
            .unwrap();
        wait_call(&r.engine, "play:A").await;
    }

    #[tokio::test(start_paused = true)]
    async fn repeat_one_replays_the_same_entry() {
        let r = rig();
        let a = r.player.play_next(song("A", Some(200))).await.unwrap();
        r.player
            .add(vec![song("B", Some(200))], AddAt::End)
            .await
            .unwrap();
        wait_call(&r.engine, "play:A").await;
        r.player.set_repeat(Repeat::One).await.unwrap();
        assert_eq!(view(&r).repeat, Repeat::One);
        end_track(&r).await;
        wait_call_n(&r.engine, "play:A", 2).await;
        let v = view(&r);
        assert_eq!(v.current.unwrap().id, a.id);
        assert_eq!(v.upcoming.len(), 1);
        assert_eq!(v.status, Status::Playing);
        assert_eq!(v.position_ms, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn skip_with_repeat_one_moves_on_and_keeps_repeat() {
        let r = rig();
        queue_ab(&r).await;
        r.player.set_repeat(Repeat::One).await.unwrap();
        r.player.skip(None).await.unwrap();
        wait_call(&r.engine, "play:B").await;
        assert_eq!(current_id(&r).as_deref(), Some("B"));
        assert_eq!(view(&r).repeat, Repeat::One);
    }

    #[tokio::test(start_paused = true)]
    async fn end_of_song_sleep_beats_repeat_one() {
        let r = rig();
        queue_ab(&r).await;
        r.player.set_repeat(Repeat::One).await.unwrap();
        r.player
            .set_sleep(Some(SleepRequest::EndOfSong))
            .await
            .unwrap();
        end_track(&r).await;
        wait_call_n(&r.engine, "play:A", 2).await;
        settle().await;
        let v = view(&r);
        assert_eq!(v.current.unwrap().song.video_id, "A");
        assert_eq!(v.status, Status::Paused);
        assert!(v.sleep.is_none());
        // The next end repeats again.
        r.player.resume().await.unwrap();
        end_track(&r).await;
        wait_call_n(&r.engine, "play:A", 3).await;
        assert_eq!(current_id(&r).as_deref(), Some("A"));
    }

    #[tokio::test(start_paused = true)]
    async fn repeat_all_wraps_to_the_first_song() {
        let r = rig();
        queue_ab(&r).await;
        r.player.set_repeat(Repeat::All).await.unwrap();
        end_track(&r).await;
        wait_call(&r.engine, "play:B").await;
        // B is the last song: the cycle shows after it.
        assert_eq!(upcoming_ids(&r), ["A", "B"]);
        end_track(&r).await;
        wait_call_n(&r.engine, "play:A", 2).await;
        assert_eq!(current_id(&r).as_deref(), Some("A"));
        assert_eq!(upcoming_ids(&r), ["B"]);
    }

    #[tokio::test(start_paused = true)]
    async fn repeat_all_with_a_single_song_loops_it() {
        let r = rig();
        r.player.play_next(song("A", Some(200))).await.unwrap();
        wait_call(&r.engine, "play:A").await;
        assert!(upcoming_ids(&r).is_empty());
        r.player.set_repeat(Repeat::All).await.unwrap();
        assert_eq!(upcoming_ids(&r), ["A"]);
        end_track(&r).await;
        wait_call_n(&r.engine, "play:A", 2).await;
        assert_eq!(upcoming_ids(&r), ["A"]);
    }

    #[tokio::test(start_paused = true)]
    async fn repeat_all_never_adds_suggestions_and_keeps_them_out_of_the_cycle() {
        let radio = FakeRadio::default();
        radio.set("A", ["A", "x"].map(radio_song).into());
        radio.set("B", ["B", "x"].map(radio_song).into());
        let r = rig_autoplay(radio);
        queue_ab(&r).await;
        r.player.set_repeat(Repeat::All).await.unwrap();
        quiesce().await;
        end_track(&r).await;
        wait_call(&r.engine, "play:B").await;
        quiesce().await;
        end_track(&r).await;
        wait_call_n(&r.engine, "play:A", 2).await;
        assert!(!r.engine.calls().iter().any(|c| c == "play:x"));
        // Back to Off: the loop copies go, and at the end autoplay takes over.
        r.player.set_repeat(Repeat::Off).await.unwrap();
        assert!(upcoming_ids(&r).is_empty());
        quiesce().await;
        end_track(&r).await;
        wait_call(&r.engine, "play:x").await;
        // The suggestion is not in the cycle: the loop is just A, B.
        r.player.set_repeat(Repeat::All).await.unwrap();
        assert_eq!(current_id(&r).as_deref(), Some("x"));
        assert_eq!(upcoming_ids(&r), ["A", "B"]);
    }

    #[tokio::test(start_paused = true)]
    async fn switching_repeat_off_removes_the_loop_copies() {
        let r = rig();
        queue_ab(&r).await;
        r.player.set_repeat(Repeat::All).await.unwrap();
        end_track(&r).await;
        wait_call(&r.engine, "play:B").await;
        assert_eq!(upcoming_ids(&r), ["A", "B"]);
        r.player
            .add(vec![song("C", Some(200))], AddAt::End)
            .await
            .unwrap();
        r.player.set_repeat(Repeat::Off).await.unwrap();
        assert_eq!(upcoming_ids(&r), ["C"]);
        assert_eq!(view(&r).repeat, Repeat::Off);
    }

    #[tokio::test(start_paused = true)]
    async fn repeat_off_ends_the_queue_as_before() {
        let r = rig();
        queue_ab(&r).await;
        end_track(&r).await;
        wait_call(&r.engine, "play:B").await;
        end_track(&r).await;
        settle().await;
        assert_eq!(view(&r).status, Status::Stopped);
        assert!(view(&r).current.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn repeat_all_after_an_autoplay_song_loops_the_queued_songs() {
        let radio = FakeRadio::default();
        radio.set("A", ["A", "x"].map(radio_song).into());
        let r = rig_autoplay(radio);
        r.player.play_next(song("A", Some(200))).await.unwrap();
        wait_call(&r.engine, "play:A").await;
        quiesce().await;
        assert!(r.player.skip(None).await.unwrap());
        wait_call(&r.engine, "play:x").await;
        // The suggestion is current and the cycle is still [A].
        r.player.set_repeat(Repeat::All).await.unwrap();
        assert_eq!(upcoming_ids(&r), ["A"]);
    }

    #[tokio::test(start_paused = true)]
    async fn set_repeat_one_takes_a_made_offer_back() {
        let r = xf_rig(20);
        r.player.play_next(song("A", Some(20))).await.unwrap();
        r.player.play_next(song("B", Some(20))).await.unwrap();
        wait_call(&r.engine, "play:chain").await;
        settle().await;
        r.player.set_repeat(Repeat::One).await.unwrap();
        settle().await;
        // A really ends: no mix into B, and A starts over on a new chain.
        assert_eq!(drain_chain(&r).await, 20 * SEC_BYTES);
        xf_track_ended(&r);
        wait_call_n(&r.engine, "play:chain", 2).await;
        settle().await;
        assert_eq!(xf_current(&r).as_deref(), Some("A"));
        assert_eq!(r.source.opens().iter().filter(|o| *o == "A").count(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn no_offer_while_repeat_is_one() {
        let r = xf_rig(20);
        r.player.set_repeat(Repeat::One).await.unwrap();
        r.player.play_next(song("A", Some(20))).await.unwrap();
        r.player.play_next(song("B", Some(20))).await.unwrap();
        wait_call(&r.engine, "play:chain").await;
        settle().await;
        assert_eq!(drain_chain(&r).await, 20 * SEC_BYTES);
        xf_track_ended(&r);
        wait_call_n(&r.engine, "play:chain", 2).await;
        settle().await;
        assert_eq!(xf_current(&r).as_deref(), Some("A"));
        assert!(!r.source.opens().iter().any(|o| o == "B"));
    }
}
