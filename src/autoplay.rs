//! The autoplay switch and the filter that turns a radio into suggestions.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::db::Db;
use crate::hidden::HiddenSet;
use crate::song::Song;

/// Settings key holding "on" or "off".
pub const AUTOPLAY_SETTING: &str = "autoplay";
/// Settings key holding the crossfade length in whole seconds.
pub const CROSSFADE_SETTING: &str = "crossfade";
/// Settings key holding the repeat mode: "off", "all" or "one".
pub const REPEAT_SETTING: &str = "repeat";
/// Crossfade length when nothing valid is stored.
const DEFAULT_CROSSFADE_SECS: u32 = 6;
/// Longest crossfade.
const MAX_CROSSFADE_SECS: u32 = 12;
/// Most suggestions shown at once.
pub const SHOWN_SUGGESTIONS: usize = 10;
/// How many of the latest plays are kept out of suggestions.
pub const RECENT_FILTER: usize = 50;

/// On unless the stored value is "off".
pub fn autoplay_enabled(db: &Db) -> anyhow::Result<bool> {
    Ok(db.setting(AUTOPLAY_SETTING)?.as_deref() != Some("off"))
}

pub fn set_autoplay_enabled(db: &Db, on: bool) -> anyhow::Result<()> {
    db.set_setting(AUTOPLAY_SETTING, if on { "on" } else { "off" })
}

/// Crossfade length in seconds: 6 when missing or invalid, else clamped to 0..=12.
pub fn crossfade_secs(db: &Db) -> anyhow::Result<u32> {
    Ok(db
        .setting(CROSSFADE_SETTING)?
        .and_then(|v| v.trim().parse::<u32>().ok())
        .map_or(DEFAULT_CROSSFADE_SECS, |s| s.min(MAX_CROSSFADE_SECS)))
}

pub fn set_crossfade_secs(db: &Db, secs: u32) -> anyhow::Result<()> {
    db.set_setting(CROSSFADE_SETTING, &secs.to_string())
}

/// What happens when a song, or the queue, ends.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Repeat {
    /// Play on to the next song.
    #[default]
    Off,
    /// Loop the songs that were queued.
    All,
    /// Play the current song again.
    One,
}

impl Repeat {
    fn as_str(self) -> &'static str {
        match self {
            Repeat::Off => "off",
            Repeat::All => "all",
            Repeat::One => "one",
        }
    }
}

/// The stored repeat mode: off when missing or invalid.
pub fn repeat_mode(db: &Db) -> anyhow::Result<Repeat> {
    Ok(match db.setting(REPEAT_SETTING)?.as_deref() {
        Some("all") => Repeat::All,
        Some("one") => Repeat::One,
        _ => Repeat::Off,
    })
}

pub fn set_repeat_mode(db: &Db, mode: Repeat) -> anyhow::Result<()> {
    db.set_setting(REPEAT_SETTING, mode.as_str())
}

/// The radio in its order, without excluded or hidden songs or repeats, cut to
/// `SHOWN_SUGGESTIONS`.
pub fn pick_suggestions(
    radio: &[Song],
    exclude: &HashSet<String>,
    hidden: &HiddenSet,
) -> Vec<Song> {
    let mut seen = HashSet::new();
    radio
        .iter()
        .filter(|s| {
            !exclude.contains(&s.video_id) && !hidden.hides(s) && seen.insert(s.video_id.clone())
        })
        .take(SHOWN_SUGGESTIONS)
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn song(id: &str) -> Song {
        Song {
            video_id: id.into(),
            title: id.into(),
            artists: "a".into(),
            album: None,
            duration_secs: None,
            artwork_url: None,
        }
    }

    fn ids(songs: &[Song]) -> Vec<&str> {
        songs.iter().map(|s| s.video_id.as_str()).collect()
    }

    #[test]
    fn autoplay_is_on_by_default() {
        assert!(autoplay_enabled(&Db::in_memory()).unwrap());
    }

    #[test]
    fn autoplay_setting_round_trips() {
        let db = Db::in_memory();
        set_autoplay_enabled(&db, false).unwrap();
        assert!(!autoplay_enabled(&db).unwrap());
        assert_eq!(
            db.setting(AUTOPLAY_SETTING).unwrap().as_deref(),
            Some("off")
        );
        set_autoplay_enabled(&db, true).unwrap();
        assert!(autoplay_enabled(&db).unwrap());
    }

    #[test]
    fn crossfade_defaults_to_six_and_round_trips() {
        let db = Db::in_memory();
        assert_eq!(crossfade_secs(&db).unwrap(), 6);
        set_crossfade_secs(&db, 0).unwrap();
        assert_eq!(crossfade_secs(&db).unwrap(), 0);
        set_crossfade_secs(&db, 12).unwrap();
        assert_eq!(crossfade_secs(&db).unwrap(), 12);
    }

    #[test]
    fn crossfade_invalid_or_large_values_are_fixed() {
        let db = Db::in_memory();
        db.set_setting(CROSSFADE_SETTING, "abc").unwrap();
        assert_eq!(crossfade_secs(&db).unwrap(), 6);
        db.set_setting(CROSSFADE_SETTING, "99").unwrap();
        assert_eq!(crossfade_secs(&db).unwrap(), 12);
    }

    #[test]
    fn repeat_defaults_to_off_and_round_trips() {
        let db = Db::in_memory();
        assert_eq!(repeat_mode(&db).unwrap(), Repeat::Off);
        for mode in [Repeat::All, Repeat::One, Repeat::Off] {
            set_repeat_mode(&db, mode).unwrap();
            assert_eq!(repeat_mode(&db).unwrap(), mode);
        }
        set_repeat_mode(&db, Repeat::One).unwrap();
        assert_eq!(db.setting(REPEAT_SETTING).unwrap().as_deref(), Some("one"));
    }

    #[test]
    fn repeat_invalid_value_is_off() {
        let db = Db::in_memory();
        db.set_setting(REPEAT_SETTING, "loop").unwrap();
        assert_eq!(repeat_mode(&db).unwrap(), Repeat::Off);
    }

    #[test]
    fn pick_drops_excluded_and_repeats() {
        let radio: Vec<Song> = ["S", "a", "b", "a", "q", "c"].map(song).into();
        let exclude: HashSet<String> = ["S", "q"].map(String::from).into();
        assert_eq!(
            ids(&pick_suggestions(&radio, &exclude, &HiddenSet::default())),
            ["a", "b", "c"]
        );
    }

    #[test]
    fn pick_stops_at_ten() {
        let radio: Vec<Song> = (0..25).map(|i| song(&format!("s{i}"))).collect();
        let picked = pick_suggestions(&radio, &HashSet::new(), &HiddenSet::default());
        assert_eq!(picked, radio[..10]);
    }

    #[test]
    fn pick_of_a_fully_excluded_radio_is_empty() {
        let radio: Vec<Song> = ["a", "b"].map(song).into();
        let exclude: HashSet<String> = ["a", "b"].map(String::from).into();
        assert!(pick_suggestions(&radio, &exclude, &HiddenSet::default()).is_empty());
    }

    #[test]
    fn pick_drops_hidden_songs_and_artists_and_still_gives_ten() {
        let mut radio: Vec<Song> = (0..30).map(|i| song(&format!("s{i}"))).collect();
        radio[2].artists = "Bad, Good".into();
        radio[5].artists = "bad".into();
        let hidden = HiddenSet {
            songs: ["s0", "s1"].map(String::from).into(),
            artists: ["bad"].map(String::from).into(),
        };
        let picked = pick_suggestions(&radio, &HashSet::new(), &hidden);
        assert_eq!(picked.len(), SHOWN_SUGGESTIONS);
        assert_eq!(
            ids(&picked),
            [
                "s3", "s4", "s6", "s7", "s8", "s9", "s10", "s11", "s12", "s13"
            ]
        );
    }

    #[test]
    fn pick_of_a_fully_hidden_radio_is_empty() {
        let radio: Vec<Song> = ["a", "b"].map(song).into();
        let hidden = HiddenSet {
            songs: HashSet::new(),
            artists: ["a"].map(String::from).into(),
        };
        assert!(pick_suggestions(&radio, &HashSet::new(), &hidden).is_empty());
    }
}
