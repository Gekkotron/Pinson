//! Play history: every song that was actually played (server-only).

use serde::Serialize;
use tokio::sync::broadcast::{self, error::RecvError};
use tokio::task::JoinHandle;

use crate::db::{Db, SONG_COLUMNS, read_song, song_params};
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
        self.db.with(|c| {
            let mut params: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(7);
            let now = chrono::Utc::now().to_rfc3339();
            params.push(&now);
            params.extend(song_params(song));
            c.execute(
                &format!(
                    "INSERT INTO history (played_at, {SONG_COLUMNS})
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)"
                ),
                params.as_slice(),
            )
            .map(|_| ())
        })
    }

    /// The last `limit` plays, newest first; repeats are kept.
    pub fn recent(&self, limit: usize) -> anyhow::Result<Vec<Song>> {
        self.db.with(|c| {
            let mut stmt = c.prepare(&format!(
                "SELECT {SONG_COLUMNS} FROM history ORDER BY id DESC LIMIT ?1"
            ))?;
            stmt.query_map([limit as i64], |r| read_song(r, 0))?
                .collect()
        })
    }

    /// Up to `limit` plays older than the play `before` (all plays when `None`), newest first,
    /// repeats kept. A non-blank `query` keeps the plays whose title or artists contain it,
    /// ignoring ASCII case; `%`, `_` and `\` in it match literally.
    pub fn page(
        &self,
        query: Option<&str>,
        before: Option<i64>,
        limit: usize,
    ) -> anyhow::Result<Vec<Play>> {
        let pattern = query
            .map(str::trim)
            .filter(|q| !q.is_empty())
            .map(|q| format!("%{}%", like_escape(q)));
        self.db.with(|c| {
            let mut stmt = c.prepare(&format!(
                "SELECT id, played_at, {SONG_COLUMNS} FROM history
                 WHERE (?1 IS NULL OR id < ?1)
                   AND (?2 IS NULL OR title LIKE ?2 ESCAPE '\\' OR artists LIKE ?2 ESCAPE '\\')
                 ORDER BY id DESC LIMIT ?3"
            ))?;
            stmt.query_map(rusqlite::params![before, pattern, limit as i64], |r| {
                Ok(Play {
                    id: r.get(0)?,
                    played_at: r.get(1)?,
                    song: read_song(r, 2)?,
                })
            })?
            .collect()
        })
    }
}

/// Escapes `\`, `%` and `_` for a `LIKE … ESCAPE '\'` pattern.
fn like_escape(q: &str) -> String {
    let mut out = String::with_capacity(q.len());
    for ch in q.chars() {
        if matches!(ch, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
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
