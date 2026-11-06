//! Decides what state JSON goes out over MQTT, and when. Pure: no I/O.

use std::time::Duration;

use chrono::{DateTime, SecondsFormat, Utc};
use serde::Serialize;
use tokio::time::Instant;

use crate::cover_color::CoverColor;
use crate::player::{StateView, Status};

/// A position further than this from the prediction counts as a seek.
const JUMP_THRESHOLD_S: f64 = 2.0;

/// The state JSON. Every key is always present, `null` when unknown.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct MqttState {
    pub state: &'static str,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub image_url: Option<String>,
    pub color: Option<String>,
    pub duration_s: Option<u32>,
    pub position_s: Option<f64>,
    pub position_at: Option<String>,
}

pub struct Input<'a> {
    pub view: &'a StateView,
    /// Time until the position starts moving (speaker latency window).
    pub moves_in: Option<Duration>,
    pub now: Instant,
    pub wall: DateTime<Utc>,
    /// The tinted colour of the CURRENT song's artwork, see `color_for`.
    pub color: Option<String>,
}

/// The cover colour for the current song only: looked up by its artwork URL,
/// so a colour computed for a song that is no longer current can't attach.
pub fn color_for(
    view: &StateView,
    lookup: impl Fn(&str) -> Option<Option<CoverColor>>,
) -> Option<String> {
    let url = view.current.as_ref()?.song.artwork_url.as_deref()?;
    lookup(url)?.map(|c| c.color)
}

struct Published {
    entry_id: Option<u64>,
    running: bool,
    loading: bool,
    at: Instant,
    state: MqttState,
}

#[derive(Default)]
pub struct Tracker {
    last: Option<Published>,
}

impl Tracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// `Some` is what to publish now.
    pub fn update(&mut self, input: Input<'_>, force: bool) -> Option<MqttState> {
        let candidate = project(&input);
        let entry_id = input.view.current.as_ref().map(|e| e.id);
        let running = input.view.position_running;
        let loading = is_loading(&input);
        let publish = force
            || self.last.as_ref().is_none_or(|last| {
                last.entry_id != entry_id
                    || last.running != running
                    || last.loading != loading
                    || differs_besides_position(&last.state, &candidate)
                    || jumped(last, &candidate, input.now)
            });
        if !publish {
            return None;
        }
        self.last = Some(Published {
            entry_id,
            running,
            loading,
            at: input.now,
            state: candidate.clone(),
        });
        Some(candidate)
    }
}

/// Playing, but the clock is not running and not about to: the source is still opening.
fn is_loading(input: &Input<'_>) -> bool {
    let view = input.view;
    view.current.is_some()
        && view.status == Status::Playing
        && !view.position_running
        && input.moves_in.is_none()
}

fn project(input: &Input<'_>) -> MqttState {
    let view = input.view;
    let song = view
        .current
        .as_ref()
        .filter(|_| view.status != Status::Stopped)
        .map(|e| &e.song);
    let Some(song) = song else {
        return MqttState {
            state: "idle",
            title: None,
            artist: None,
            album: None,
            image_url: None,
            color: None,
            duration_s: None,
            position_s: None,
            position_at: None,
        };
    };
    let at = input.wall
        + chrono::Duration::from_std(input.moves_in.unwrap_or_default()).unwrap_or_default();
    let position_at = (!is_loading(input)).then(|| at.to_rfc3339_opts(SecondsFormat::Millis, true));
    MqttState {
        state: if view.status == Status::Playing {
            "playing"
        } else {
            "paused"
        },
        title: Some(song.title.clone()),
        artist: Some(song.artists.clone()),
        album: song.album.clone(),
        image_url: song.artwork_url.clone(),
        color: input.color.clone(),
        duration_s: song.duration_secs,
        position_s: Some(view.position_ms as f64 / 1000.0),
        position_at,
    }
}

fn differs_besides_position(a: &MqttState, b: &MqttState) -> bool {
    a.state != b.state
        || a.title != b.title
        || a.artist != b.artist
        || a.album != b.album
        || a.image_url != b.image_url
        || a.color != b.color
        || a.duration_s != b.duration_s
}

/// Whether the position is further than the threshold from where the last
/// published state predicts it should be by now.
fn jumped(last: &Published, candidate: &MqttState, now: Instant) -> bool {
    let (Some(was), Some(is)) = (last.state.position_s, candidate.position_s) else {
        return false;
    };
    let elapsed = if last.running && last.state.state == "playing" {
        now.saturating_duration_since(last.at).as_secs_f64()
    } else {
        0.0
    };
    (is - (was + elapsed)).abs() > JUMP_THRESHOLD_S
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::player::{AutoplayView, StateView, Status};
    use crate::queue::QueueEntry;
    use crate::song::Song;
    use chrono::{TimeZone, Utc};
    use std::time::Duration;
    use tokio::time::Instant;

    fn entry(id: u64, title: &str) -> QueueEntry {
        QueueEntry {
            id,
            song: Song {
                video_id: format!("v{id}"),
                title: title.into(),
                artists: "Artist".into(),
                album: Some("Album".into()),
                duration_secs: Some(200),
                artwork_url: Some("https://lh3.googleusercontent.com/x".into()),
            },
        }
    }

    fn view(current: Option<QueueEntry>, status: Status, pos_ms: u64, running: bool) -> StateView {
        StateView {
            current,
            status,
            position_ms: pos_ms,
            position_running: running,
            upcoming: vec![],
            version: 1,
            autoplay: AutoplayView {
                enabled: false,
                based_on: None,
                songs: vec![],
            },
        }
    }

    fn playing(pos_ms: u64) -> StateView {
        view(Some(entry(1, "A")), Status::Playing, pos_ms, true)
    }

    fn wall() -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 6, 12, 0, 0).unwrap()
    }

    fn input(view: &StateView, now: Instant) -> Input<'_> {
        Input {
            view,
            moves_in: None,
            now,
            wall: wall(),
            color: None,
        }
    }

    #[test]
    fn first_update_publishes() {
        let t0 = Instant::now();
        let mut t = Tracker::new();
        assert!(t.update(input(&playing(0), t0), false).is_some());
    }

    #[test]
    fn idle_has_only_nulls() {
        let t0 = Instant::now();
        let v = view(None, Status::Stopped, 0, false);
        let s = Tracker::new().update(input(&v, t0), false).unwrap();
        assert_eq!(
            serde_json::to_string(&s).unwrap(),
            r#"{"state":"idle","title":null,"artist":null,"album":null,"image_url":null,"color":null,"duration_s":null,"position_s":null,"position_at":null}"#
        );
    }

    #[test]
    fn playing_fields_map() {
        let t0 = Instant::now();
        let v = playing(42_500);
        let mut i = input(&v, t0);
        i.color = Some("#123456".into());
        let s = Tracker::new().update(i, false).unwrap();
        assert_eq!(s.state, "playing");
        assert_eq!(s.title.as_deref(), Some("A"));
        assert_eq!(s.artist.as_deref(), Some("Artist"));
        assert_eq!(s.album.as_deref(), Some("Album"));
        assert_eq!(
            s.image_url.as_deref(),
            Some("https://lh3.googleusercontent.com/x")
        );
        assert_eq!(s.color.as_deref(), Some("#123456"));
        assert_eq!(s.duration_s, Some(200));
        assert_eq!(s.position_s, Some(42.5));
        assert_eq!(s.position_at.as_deref(), Some("2026-11-06T12:00:00.000Z"));
    }

    #[test]
    fn ticks_do_not_publish() {
        let t0 = Instant::now();
        let mut t = Tracker::new();
        t.update(input(&playing(10_000), t0), false).unwrap();
        let later = t0 + Duration::from_secs(5);
        assert!(t.update(input(&playing(15_000), later), false).is_none());
    }

    #[test]
    fn autoplay_or_queue_change_does_not_publish() {
        let t0 = Instant::now();
        let mut t = Tracker::new();
        t.update(input(&playing(0), t0), false).unwrap();
        let mut v = playing(0);
        v.upcoming = vec![entry(2, "B")];
        v.version = 9;
        v.autoplay.enabled = true;
        assert!(t.update(input(&v, t0), false).is_none());
    }

    #[test]
    fn pause_publishes() {
        let t0 = Instant::now();
        let mut t = Tracker::new();
        t.update(input(&playing(10_000), t0), false).unwrap();
        let v = view(Some(entry(1, "A")), Status::Paused, 10_000, false);
        let s = t.update(input(&v, t0), false).unwrap();
        assert_eq!(s.state, "paused");
    }

    #[test]
    fn song_change_publishes() {
        let t0 = Instant::now();
        let mut t = Tracker::new();
        t.update(input(&playing(10_000), t0), false).unwrap();
        let v = view(Some(entry(2, "B")), Status::Playing, 0, true);
        assert!(t.update(input(&v, t0), false).is_some());
    }

    #[test]
    fn seek_publishes() {
        let t0 = Instant::now();
        let mut t = Tracker::new();
        t.update(input(&playing(10_000), t0), false).unwrap();
        assert!(t.update(input(&playing(40_000), t0), false).is_some());
    }

    #[test]
    fn small_drift_does_not_publish() {
        let t0 = Instant::now();
        let mut t = Tracker::new();
        t.update(input(&playing(10_000), t0), false).unwrap();
        assert!(t.update(input(&playing(11_500), t0), false).is_none());
    }

    #[test]
    fn restart_publishes() {
        let t0 = Instant::now();
        let mut t = Tracker::new();
        t.update(input(&playing(10_000), t0), false).unwrap();
        let v = view(Some(entry(1, "A")), Status::Playing, 0, false);
        assert!(t.update(input(&v, t0), false).is_some());
    }

    #[test]
    fn latency_window_stamps_future_position_at() {
        let t0 = Instant::now();
        let v = view(Some(entry(1, "A")), Status::Playing, 0, false);
        let mut i = input(&v, t0);
        i.moves_in = Some(Duration::from_millis(750));
        let s = Tracker::new().update(i, false).unwrap();
        assert_eq!(s.position_at.as_deref(), Some("2026-11-06T12:00:00.750Z"));
    }

    #[test]
    fn loading_has_null_position_at() {
        let t0 = Instant::now();
        let v = view(Some(entry(1, "A")), Status::Playing, 0, false);
        let s = Tracker::new().update(input(&v, t0), false).unwrap();
        assert_eq!(s.state, "playing");
        assert_eq!(s.position_s, Some(0.0));
        assert_eq!(s.position_at, None);
    }

    #[test]
    fn loading_to_started_publishes() {
        let t0 = Instant::now();
        let mut t = Tracker::new();
        let v = view(Some(entry(1, "A")), Status::Playing, 0, false);
        assert!(t.update(input(&v, t0), false).is_some());
        assert!(t.update(input(&v, t0), false).is_none());
        let mut i = input(&v, t0);
        i.moves_in = Some(Duration::from_millis(750));
        let s = t.update(i, false).unwrap();
        assert_eq!(s.position_at.as_deref(), Some("2026-11-06T12:00:00.750Z"));
    }

    #[test]
    fn forced_echo_while_loading_has_null_position_at() {
        let t0 = Instant::now();
        let v = view(Some(entry(1, "A")), Status::Playing, 0, false);
        let mut t = Tracker::new();
        t.update(input(&v, t0), false).unwrap();
        let s = t.update(input(&v, t0), true).unwrap();
        assert_eq!(s.position_at, None);
    }

    #[test]
    fn colour_arrival_publishes() {
        let t0 = Instant::now();
        let mut t = Tracker::new();
        t.update(input(&playing(0), t0), false).unwrap();
        let v = playing(0);
        let mut i = input(&v, t0);
        i.color = Some("#102030".into());
        assert!(t.update(i, false).is_some());
    }

    #[test]
    fn late_color_for_old_song_is_ignored() {
        let v = playing(0);
        let old = CoverColor {
            color: "#111111".into(),
            glow: "#222222".into(),
        };
        let only_old =
            |url: &str| (url == "https://lh3.googleusercontent.com/old").then(|| Some(old.clone()));
        assert_eq!(color_for(&v, only_old), None);
        let current = |_: &str| {
            Some(Some(CoverColor {
                color: "#333333".into(),
                glow: "#444444".into(),
            }))
        };
        assert_eq!(color_for(&v, current).as_deref(), Some("#333333"));
        assert_eq!(color_for(&v, |_| Some(None)), None);
        assert_eq!(color_for(&v, |_| None), None);
    }

    #[test]
    fn force_publishes_even_if_unchanged() {
        let t0 = Instant::now();
        let mut t = Tracker::new();
        t.update(input(&playing(0), t0), false).unwrap();
        assert!(t.update(input(&playing(0), t0), true).is_some());
    }
}
