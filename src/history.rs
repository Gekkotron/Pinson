//! Play history: every song that was actually played (server-only).

use serde::Serialize;
use tokio::sync::broadcast::{self, error::RecvError};
use tokio::task::JoinHandle;

use crate::db::{CATALOG_SONG_COLUMNS, Db, read_song, register_song};
use crate::fuzzy::Matcher;
use crate::song::Song;

/// How many of the most recent plays the Home mixes look at.
pub const MIX_SEED_WINDOW: usize = 100;

/// One recorded play: its row id (the paging cursor), when it started (UTC RFC 3339) and the song.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Play {
    pub id: i64,
    pub played_at: String,
    pub song: Song,
}

#[derive(Clone)]
pub struct History {
    db: Db,
}

impl History {
    pub fn new(db: Db) -> History {
        History { db }
    }

    /// Adds a play at the front. Every play is kept.
    pub fn record(&self, song: &Song) -> anyhow::Result<()> {
        self.record_at(song, &chrono::Utc::now().to_rfc3339())
    }

    pub(crate) fn record_at(&self, song: &Song, played_at: &str) -> anyhow::Result<()> {
        self.db.with(|c| {
            let tx = c.transaction()?;
            let song_id = register_song(&tx, song)?;
            tx.execute(
                "INSERT INTO history (played_at, song_id) VALUES (?1, ?2)",
                (played_at, song_id),
            )?;
            tx.commit()
        })
    }

    /// The last `limit` plays, newest first; repeats are kept.
    pub fn recent(&self, limit: usize) -> anyhow::Result<Vec<Song>> {
        self.db.with(|c| {
            let mut stmt = c.prepare(&format!(
                "SELECT {CATALOG_SONG_COLUMNS} FROM history JOIN songs ON songs.id = song_id
                 ORDER BY history.id DESC LIMIT ?1"
            ))?;
            stmt.query_map([limit as i64], |r| read_song(r, 0))?
                .collect()
        })
    }

    /// Plays that started at or after `cutoff` (all plays when `None`), newest first, as
    /// `(played_at, song)`. Every row is stored as UTC RFC 3339 (`+00:00`), so comparing the
    /// text against `cutoff.to_rfc3339()` orders the same as comparing the instants.
    pub fn since(
        &self,
        cutoff: Option<chrono::DateTime<chrono::Utc>>,
    ) -> anyhow::Result<Vec<(String, Song)>> {
        let cutoff = cutoff.map(|c| c.to_rfc3339());
        self.db.with(|c| {
            let mut stmt = c.prepare(&format!(
                "SELECT played_at, {CATALOG_SONG_COLUMNS} FROM history JOIN songs ON songs.id = song_id
                 WHERE (?1 IS NULL OR played_at >= ?1)
                 ORDER BY history.id DESC"
            ))?;
            stmt.query_map(rusqlite::params![cutoff], |r| {
                Ok((r.get(0)?, read_song(r, 1)?))
            })?
            .collect()
        })
    }

    /// Up to `limit` plays older than the play `before` (all plays when `None`), newest first,
    /// repeats kept. A non-blank `query` keeps the plays whose title and artists match it,
    /// typos allowed (see [`Matcher`]).
    pub fn page(
        &self,
        query: Option<&str>,
        before: Option<i64>,
        limit: usize,
    ) -> anyhow::Result<Vec<Play>> {
        let matcher = query.and_then(Matcher::new);
        if limit == 0 {
            return Ok(Vec::new());
        }
        self.db.with(|c| {
            // Matching runs in Rust, so walk the history newest first until the page is full.
            let mut stmt = c.prepare(&format!(
                "SELECT history.id, played_at, {CATALOG_SONG_COLUMNS} FROM history JOIN songs ON songs.id = song_id
                 WHERE (?1 IS NULL OR history.id < ?1)
                 ORDER BY history.id DESC"
            ))?;
            let rows = stmt.query_map(rusqlite::params![before], |r| {
                Ok(Play {
                    id: r.get(0)?,
                    played_at: r.get(1)?,
                    song: read_song(r, 2)?,
                })
            })?;
            let mut page = Vec::new();
            for play in rows {
                let play = play?;
                let keep = matcher
                    .as_ref()
                    .is_none_or(|m| m.matches(&[&play.song.title, &play.song.artists]));
                if keep {
                    page.push(play);
                    if page.len() == limit {
                        break;
                    }
                }
            }
            Ok(page)
        })
    }
}

/// Records every song that arrives on `plays`, until the channel closes.
pub fn spawn_recorder(history: History, mut plays: broadcast::Receiver<Song>) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match plays.recv().await {
                Ok(song) => {
                    let history = history.clone();
                    match tokio::task::spawn_blocking(move || history.record(&song)).await {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) => tracing::warn!("recording play history: {e:#}"),
                        Err(e) => tracing::warn!("recording play history: {e}"),
                    }
                }
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => break,
            }
        }
    })
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

    fn ids(songs: &[Song]) -> Vec<&str> {
        songs.iter().map(|s| s.video_id.as_str()).collect()
    }

    #[test]
    fn recent_is_newest_first_with_repeats() {
        let h = History::new(Db::in_memory());
        for id in ["A", "B", "A"] {
            h.record(&song(id)).unwrap();
        }
        assert_eq!(ids(&h.recent(10).unwrap()), ["A", "B", "A"]);
        assert_eq!(h.recent(2).unwrap().len(), 2);
    }

    fn song_named(id: &str, title: &str, artists: &str) -> Song {
        Song {
            title: title.into(),
            artists: artists.into(),
            ..song(id)
        }
    }

    fn play_ids(plays: &[Play]) -> Vec<&str> {
        plays.iter().map(|p| p.song.video_id.as_str()).collect()
    }

    fn titles(plays: &[Play]) -> Vec<&str> {
        plays.iter().map(|p| p.song.title.as_str()).collect()
    }

    #[test]
    fn keeps_every_play() {
        let h = History::new(Db::in_memory());
        for i in 0..105 {
            h.record(&song(&format!("s{i}"))).unwrap();
        }
        let all = h.recent(200).unwrap();
        assert_eq!(all.len(), 105);
        assert_eq!(all[0].video_id, "s104");
        assert_eq!(all[104].video_id, "s0");
    }

    #[test]
    fn page_is_newest_first_and_cursor_paged() {
        let h = History::new(Db::in_memory());
        for id in ["A", "B", "C", "D"] {
            h.record(&song(id)).unwrap();
        }
        let first = h.page(None, None, 2).unwrap();
        assert_eq!(play_ids(&first), ["D", "C"]);
        assert!(first[0].id > first[1].id);
        for p in &first {
            assert!(chrono::DateTime::parse_from_rfc3339(&p.played_at).is_ok());
        }
        let second = h.page(None, Some(first[1].id), 2).unwrap();
        assert_eq!(play_ids(&second), ["B", "A"]);
        assert!(second[0].id > second[1].id);
        assert!(h.page(None, Some(second[1].id), 2).unwrap().is_empty());
    }

    #[test]
    fn page_search_matches_title_or_artists_case_insensitive() {
        let h = History::new(Db::in_memory());
        h.record(&song_named("1", "Get Lucky", "Daft Punk"))
            .unwrap();
        h.record(&song_named("2", "Nightcall", "Kavinsky")).unwrap();
        h.record(&song_named("3", "Midnight City", "M83")).unwrap();
        assert_eq!(
            titles(&h.page(Some("daft"), None, 10).unwrap()),
            ["Get Lucky"]
        );
        assert_eq!(
            titles(&h.page(Some("NIGHT"), None, 10).unwrap()),
            ["Midnight City", "Nightcall"]
        );
        assert_eq!(h.page(Some("  "), None, 10).unwrap().len(), 3);
    }

    #[test]
    fn page_search_treats_wildcards_literally() {
        let h = History::new(Db::in_memory());
        for (i, t) in ["100% Pure", "100 Pure", "a_b", "axb", r"x\y", "xy"]
            .iter()
            .enumerate()
        {
            h.record(&song_named(&i.to_string(), t, "Artist")).unwrap();
        }
        assert_eq!(
            titles(&h.page(Some("100%"), None, 10).unwrap()),
            ["100% Pure"]
        );
        assert_eq!(titles(&h.page(Some("a_b"), None, 10).unwrap()), ["a_b"]);
        assert_eq!(titles(&h.page(Some(r"x\y"), None, 10).unwrap()), [r"x\y"]);
    }

    #[test]
    fn page_search_tolerates_typos_and_accents() {
        let h = History::new(Db::in_memory());
        h.record(&song_named("1", "Get Lucky", "Daft Punk"))
            .unwrap();
        h.record(&song_named("2", "Halo", "Beyoncé")).unwrap();
        h.record(&song_named("3", "Nightcall", "Kavinsky")).unwrap();
        assert_eq!(
            titles(&h.page(Some("dfat pnuk"), None, 10).unwrap()),
            ["Get Lucky"]
        );
        assert_eq!(
            titles(&h.page(Some("beyonce"), None, 10).unwrap()),
            ["Halo"]
        );
        assert_eq!(
            titles(&h.page(Some("kavinksy"), None, 10).unwrap()),
            ["Nightcall"]
        );
        assert!(h.page(Some("metallica"), None, 10).unwrap().is_empty());
    }

    #[test]
    fn page_search_pages_through_the_matches() {
        let h = History::new(Db::in_memory());
        for (i, t) in ["Daft 1", "Other", "Daft 2", "Other", "Daft 3"]
            .iter()
            .enumerate()
        {
            h.record(&song_named(&i.to_string(), t, "Artist")).unwrap();
        }
        let first = h.page(Some("daft"), None, 2).unwrap();
        assert_eq!(titles(&first), ["Daft 3", "Daft 2"]);
        let second = h.page(Some("daft"), Some(first[1].id), 2).unwrap();
        assert_eq!(titles(&second), ["Daft 1"]);
    }

    fn record_at(h: &History, played_at: &str, id: &str) {
        h.record_at(&song(id), played_at).unwrap();
    }

    #[test]
    fn since_filters_by_instant_newest_first() {
        use chrono::{Duration, TimeZone, Utc};
        let h = History::new(Db::in_memory());
        let cutoff = Utc.with_ymd_and_hms(2026, 11, 10, 12, 0, 0).unwrap();
        // Same format `record` writes: UTC, with or without a fractional second.
        let before = (cutoff - Duration::milliseconds(1)).to_rfc3339();
        record_at(&h, &before, "old");
        record_at(&h, &cutoff.to_rfc3339(), "edge");
        record_at(
            &h,
            &(cutoff + Duration::nanoseconds(1)).to_rfc3339(),
            "after",
        );
        record_at(&h, &(cutoff + Duration::days(1)).to_rfc3339(), "late");
        assert!(before.ends_with("+00:00"));
        let got = h.since(Some(cutoff)).unwrap();
        let got_ids: Vec<&str> = got.iter().map(|(_, s)| s.video_id.as_str()).collect();
        assert_eq!(got_ids, ["late", "after", "edge"]);
        assert_eq!(got[2].0, cutoff.to_rfc3339());
        assert_eq!(h.since(None).unwrap().len(), 4);
    }

    #[test]
    fn since_includes_plays_recorded_now() {
        let h = History::new(Db::in_memory());
        h.record(&song("A")).unwrap();
        let cutoff = chrono::Utc::now() - chrono::Duration::days(7);
        assert_eq!(h.since(Some(cutoff)).unwrap().len(), 1);
        assert!(
            h.since(Some(chrono::Utc::now() + chrono::Duration::seconds(5)))
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn recorder_stores_songs_until_the_channel_closes() {
        let h = History::new(Db::in_memory());
        let (tx, rx) = broadcast::channel(8);
        let task = spawn_recorder(h.clone(), rx);
        tx.send(song("A")).unwrap();
        tx.send(song("B")).unwrap();
        drop(tx);
        task.await.unwrap();
        assert_eq!(ids(&h.recent(10).unwrap()), ["B", "A"]);
    }
}
