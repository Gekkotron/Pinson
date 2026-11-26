//! Smart playlists: lists that build themselves from your play history and favorites.
//!
//! Nothing is stored. Each list is computed from `history` and `favorites` on request.

use std::collections::HashSet;

use chrono::{DateTime, Duration, Utc};

use crate::catalog::{Collection, CollectionKind, CollectionSummary};
use crate::db::{CATALOG_SONG_COLUMNS, Db, read_song};
use crate::song::Song;

/// A list with fewer songs than this is left out.
pub const MIN_SONGS: usize = 5;

/// Days looked back by "On repeat".
const REPEAT_DAYS: i64 = 30;
/// Plays inside that window that put a song on repeat.
const REPEAT_PLAYS: i64 = 2;
/// Days without a play after which a favorite counts as forgotten.
const FORGOTTEN_DAYS: i64 = 90;
/// Days after which a single play counts as "heard once".
const ONCE_DAYS: i64 = 14;

/// One of the built-in smart lists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SmartList {
    OnRepeat,
    ForgottenFavorites,
    HeardOnce,
}

impl SmartList {
    /// Every list, in display order.
    pub const ALL: [SmartList; 3] = [
        SmartList::OnRepeat,
        SmartList::ForgottenFavorites,
        SmartList::HeardOnce,
    ];

    /// The id used in the API.
    pub fn id(self) -> &'static str {
        match self {
            SmartList::OnRepeat => "on-repeat",
            SmartList::ForgottenFavorites => "forgotten-favorites",
            SmartList::HeardOnce => "heard-once",
        }
    }

    /// The list with API id `id`.
    pub fn from_id(id: &str) -> Option<SmartList> {
        SmartList::ALL.into_iter().find(|l| l.id() == id)
    }

    fn title(self) -> &'static str {
        match self {
            SmartList::OnRepeat => "On repeat",
            SmartList::ForgottenFavorites => "Forgotten favorites",
            SmartList::HeardOnce => "Heard once",
        }
    }

    fn subtitle(self) -> &'static str {
        match self {
            SmartList::OnRepeat => "Your most played this month",
            SmartList::ForgottenFavorites => "Favorites you haven't played in 3 months",
            SmartList::HeardOnce => "Songs you played once and never again",
        }
    }

    /// Most songs in the list.
    fn max_songs(self) -> usize {
        match self {
            SmartList::OnRepeat => 50,
            SmartList::ForgottenFavorites | SmartList::HeardOnce => 100,
        }
    }
}

/// The text form `history.played_at` is stored in, for comparing as text.
fn cutoff_text(now: DateTime<Utc>, days: i64) -> String {
    (now - Duration::days(days)).to_rfc3339()
}

/// Songs played at least twice in the last 30 days: most plays first, then most recent.
fn on_repeat(db: &Db, now: DateTime<Utc>, max: usize) -> anyhow::Result<Vec<Song>> {
    let cutoff = cutoff_text(now, REPEAT_DAYS);
    db.with(|c| {
        let mut stmt = c.prepare(&format!(
            "SELECT {CATALOG_SONG_COLUMNS} FROM songs
             JOIN (SELECT song_id, MAX(id) AS last_id, COUNT(*) AS plays, MAX(played_at) AS last_at
                   FROM history WHERE played_at >= ?1
                   GROUP BY song_id HAVING COUNT(*) >= ?2) g ON songs.id = g.song_id
             ORDER BY g.plays DESC, g.last_at DESC, g.last_id DESC
             LIMIT ?3"
        ))?;
        stmt.query_map(rusqlite::params![cutoff, REPEAT_PLAYS, max as i64], |r| {
            read_song(r, 0)
        })?
        .collect()
    })
}

/// Favorites, in favorites order, with no play in the last 90 days.
fn forgotten_favorites(
    db: &Db,
    favorites: &[Song],
    now: DateTime<Utc>,
    max: usize,
) -> anyhow::Result<Vec<Song>> {
    let cutoff = cutoff_text(now, FORGOTTEN_DAYS);
    let recent: HashSet<String> = db.with(|c| {
        let mut stmt = c.prepare(
            "SELECT DISTINCT video_id FROM history JOIN songs ON songs.id = song_id
             WHERE played_at >= ?1 AND video_id IS NOT NULL",
        )?;
        stmt.query_map([cutoff], |r| r.get(0))?.collect()
    })?;
    Ok(favorites
        .iter()
        .filter(|s| !recent.contains(&s.video_id))
        .take(max)
        .cloned()
        .collect())
}

/// Songs played exactly once ever, that play older than 14 days: newest play first.
fn heard_once(db: &Db, now: DateTime<Utc>, max: usize) -> anyhow::Result<Vec<Song>> {
    let cutoff = cutoff_text(now, ONCE_DAYS);
    db.with(|c| {
        let mut stmt = c.prepare(&format!(
            "SELECT {CATALOG_SONG_COLUMNS} FROM songs
             JOIN (SELECT song_id, MAX(id) AS last_id, MAX(played_at) AS last_at
                   FROM history GROUP BY song_id
                   HAVING COUNT(*) = 1 AND MAX(played_at) < ?1) g ON songs.id = g.song_id
             ORDER BY g.last_at DESC, g.last_id DESC
             LIMIT ?2"
        ))?;
        stmt.query_map(rusqlite::params![cutoff, max as i64], |r| read_song(r, 0))?
            .collect()
    })
}

/// The songs of `list` as of `now`, whatever their number.
fn songs(
    list: SmartList,
    db: &Db,
    favorites: &[Song],
    now: DateTime<Utc>,
) -> anyhow::Result<Vec<Song>> {
    let max = list.max_songs();
    match list {
        SmartList::OnRepeat => on_repeat(db, now, max),
        SmartList::ForgottenFavorites => forgotten_favorites(db, favorites, now, max),
        SmartList::HeardOnce => heard_once(db, now, max),
    }
}

/// `list` as a collection; `None` when it has fewer than [`MIN_SONGS`] songs.
fn collection(
    list: SmartList,
    db: &Db,
    favorites: &[Song],
    now: DateTime<Utc>,
) -> anyhow::Result<Option<Collection>> {
    let songs = songs(list, db, favorites, now)?;
    if songs.len() < MIN_SONGS {
        return Ok(None);
    }
    // The list page adds its own song count and length after the subtitle.
    Ok(Some(Collection {
        summary: CollectionSummary {
            kind: CollectionKind::Smart,
            id: list.id().to_owned(),
            title: list.title().to_owned(),
            subtitle: Some(list.subtitle().to_owned()),
            artwork_url: songs.iter().find_map(|s| s.artwork_url.clone()),
        },
        songs,
    }))
}

/// The lists that have enough songs, in display order. Their subtitle starts
/// with the song count ("12 songs · …"), for rows that show no count of their own.
pub fn summaries(
    db: &Db,
    favorites: &[Song],
    now: DateTime<Utc>,
) -> anyhow::Result<Vec<CollectionSummary>> {
    let mut out = Vec::new();
    for list in SmartList::ALL {
        if let Some(c) = collection(list, db, favorites, now)? {
            let count = c.songs.len();
            let mut summary = c.summary;
            summary.subtitle = Some(format!(
                "{count} {} · {}",
                if count == 1 { "song" } else { "songs" },
                list.subtitle()
            ));
            out.push(summary);
        }
    }
    Ok(out)
}

/// The list with API id `id`; `None` for an unknown id or a list under [`MIN_SONGS`] songs.
pub fn get(
    id: &str,
    db: &Db,
    favorites: &[Song],
    now: DateTime<Utc>,
) -> anyhow::Result<Option<Collection>> {
    match SmartList::from_id(id) {
        Some(list) => collection(list, db, favorites, now),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn song(id: &str) -> Song {
        Song {
            video_id: id.into(),
            title: format!("Title {id}"),
            artists: "Artist".into(),
            album: None,
            duration_secs: Some(180),
            artwork_url: None,
        }
    }

    fn now() -> DateTime<Utc> {
        "2026-06-30T12:00:00Z".parse().unwrap()
    }

    /// Inserts a play of `id` that started `days` days before `now()`.
    fn play(db: &Db, id: &str, days: i64) {
        let at = (now() - Duration::days(days)).to_rfc3339();
        crate::history::History::new(db.clone())
            .record_at(&song(id), &at)
            .unwrap();
    }

    fn ids(songs: &[Song]) -> Vec<&str> {
        songs.iter().map(|s| s.video_id.as_str()).collect()
    }

    #[test]
    fn on_repeat_needs_two_plays_inside_30_days() {
        let db = Db::in_memory();
        play(&db, "in", 1);
        play(&db, "in", 29);
        play(&db, "out", 1);
        play(&db, "out", 31);
        play(&db, "single", 2);
        assert_eq!(ids(&on_repeat(&db, now(), 50).unwrap()), ["in"]);
    }

    #[test]
    fn on_repeat_orders_by_plays_then_most_recent() {
        let db = Db::in_memory();
        for (id, days) in [("a", 5), ("a", 6), ("b", 1), ("b", 2), ("c", 3), ("c", 4)] {
            play(&db, id, days);
        }
        play(&db, "c", 7);
        // c has 3 plays; b and a have 2 and b played most recently.
        assert_eq!(ids(&on_repeat(&db, now(), 50).unwrap()), ["c", "b", "a"]);
    }

    #[test]
    fn on_repeat_is_cut_to_max() {
        let db = Db::in_memory();
        for i in 0..8 {
            play(&db, &format!("s{i}"), 1);
            play(&db, &format!("s{i}"), 2);
        }
        assert_eq!(on_repeat(&db, now(), 5).unwrap().len(), 5);
    }

    #[test]
    fn forgotten_favorites_skip_recent_plays_and_count_never_played() {
        let db = Db::in_memory();
        play(&db, "recent", 89);
        play(&db, "old", 91);
        let favs = [song("recent"), song("old"), song("never"), song("old2")];
        let got = forgotten_favorites(&db, &favs, now(), 100).unwrap();
        // Favorites order is kept.
        assert_eq!(ids(&got), ["old", "never", "old2"]);
        assert_eq!(forgotten_favorites(&db, &favs, now(), 2).unwrap().len(), 2);
    }

    #[test]
    fn a_recent_play_beats_an_old_one_for_forgotten_favorites() {
        let db = Db::in_memory();
        play(&db, "x", 200);
        play(&db, "x", 3);
        assert!(
            forgotten_favorites(&db, &[song("x")], now(), 100)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn heard_once_needs_one_play_older_than_14_days() {
        let db = Db::in_memory();
        play(&db, "old", 15);
        play(&db, "fresh", 13);
        play(&db, "twice", 20);
        play(&db, "twice", 40);
        play(&db, "older", 60);
        // Newest play first.
        assert_eq!(ids(&heard_once(&db, now(), 100).unwrap()), ["old", "older"]);
        assert_eq!(heard_once(&db, now(), 1).unwrap().len(), 1);
    }

    #[test]
    fn lists_under_five_songs_are_left_out() {
        let db = Db::in_memory();
        for i in 0..4 {
            play(&db, &format!("s{i}"), 20);
        }
        assert!(summaries(&db, &[], now()).unwrap().is_empty());
        assert!(get("heard-once", &db, &[], now()).unwrap().is_none());
        play(&db, "s4", 20);
        let sums = summaries(&db, &[], now()).unwrap();
        assert_eq!(sums.len(), 1);
        assert_eq!(sums[0].id, "heard-once");
        assert_eq!(sums[0].kind, CollectionKind::Smart);
        assert_eq!(
            sums[0].subtitle.as_deref(),
            Some("5 songs · Songs you played once and never again")
        );
        let c = get("heard-once", &db, &[], now()).unwrap().unwrap();
        assert_eq!(c.songs.len(), 5);
        assert!(get("nope", &db, &[], now()).unwrap().is_none());
    }

    #[test]
    fn artwork_is_the_first_song_with_one() {
        let db = Db::in_memory();
        for i in 0..5 {
            play(&db, &format!("s{i}"), 20);
        }
        db.with(|c| {
            c.execute(
                "UPDATE songs SET artwork_url = 'https://x/cover.jpg' WHERE video_id = 's2'",
                [],
            )
        })
        .unwrap();
        let c = get("heard-once", &db, &[], now()).unwrap().unwrap();
        assert_eq!(
            c.summary.artwork_url.as_deref(),
            Some("https://x/cover.jpg")
        );
    }

    #[test]
    fn ids_round_trip() {
        for list in SmartList::ALL {
            assert_eq!(SmartList::from_id(list.id()), Some(list));
        }
        assert_eq!(SmartList::from_id("x"), None);
    }
}
