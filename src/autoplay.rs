//! The autoplay switch and the filter that turns a radio into suggestions.

use std::collections::HashSet;

use crate::db::Db;
use crate::song::Song;

/// Settings key holding "on" or "off".
pub const AUTOPLAY_SETTING: &str = "autoplay";
/// Settings key holding the crossfade length in whole seconds.
pub const CROSSFADE_SETTING: &str = "crossfade";
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

/// The radio in its order, without excluded songs or repeats, cut to
/// `SHOWN_SUGGESTIONS`.
pub fn pick_suggestions(radio: &[Song], exclude: &HashSet<String>) -> Vec<Song> {
    let mut seen = HashSet::new();
    radio
        .iter()
        .filter(|s| !exclude.contains(&s.video_id) && seen.insert(s.video_id.clone()))
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
    fn pick_drops_excluded_and_repeats() {
        let radio: Vec<Song> = ["S", "a", "b", "a", "q", "c"].map(song).into();
        let exclude: HashSet<String> = ["S", "q"].map(String::from).into();
        assert_eq!(ids(&pick_suggestions(&radio, &exclude)), ["a", "b", "c"]);
    }

    #[test]
    fn pick_stops_at_ten() {
        let radio: Vec<Song> = (0..25).map(|i| song(&format!("s{i}"))).collect();
        let picked = pick_suggestions(&radio, &HashSet::new());
        assert_eq!(picked, radio[..10]);
    }

    #[test]
    fn pick_of_a_fully_excluded_radio_is_empty() {
        let radio: Vec<Song> = ["a", "b"].map(song).into();
        let exclude: HashSet<String> = ["a", "b"].map(String::from).into();
        assert!(pick_suggestions(&radio, &exclude).is_empty());
    }
}
