//! Play history: the last songs that were actually played (server-only).

use tokio::sync::broadcast::{self, error::RecvError};
use tokio::task::JoinHandle;

use crate::db::{Db, SONG_COLUMNS, read_song, song_params};
use crate::song::Song;

/// How many plays are kept; older ones are deleted.
pub const HISTORY_LIMIT: usize = 100;

#[derive(Clone)]
pub struct History {
    db: Db,
}

impl History {
    pub fn new(db: Db) -> History {
        History { db }
    }

    /// Adds a play at the front and trims the table to [`HISTORY_LIMIT`].
    pub fn record(&self, song: &Song) -> anyhow::Result<()> {
        self.db.with(|c| {
            let tx = c.transaction()?;
            let mut params: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(7);
            let now = chrono::Utc::now().to_rfc3339();
            params.push(&now);
            params.extend(song_params(song));
            tx.execute(
                &format!(
                    "INSERT INTO history (played_at, {SONG_COLUMNS})
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)"
                ),
                params.as_slice(),
            )?;
            tx.execute(
                "DELETE FROM history WHERE id NOT IN
                 (SELECT id FROM history ORDER BY id DESC LIMIT ?1)",
                [HISTORY_LIMIT as i64],
            )?;
            tx.commit()
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

    #[test]
    fn keeps_only_the_last_100() {
        let h = History::new(Db::in_memory());
        for i in 0..105 {
            h.record(&song(&format!("s{i}"))).unwrap();
        }
        let all = h.recent(200).unwrap();
        assert_eq!(all.len(), HISTORY_LIMIT);
        assert_eq!(all[0].video_id, "s104");
        assert_eq!(all[99].video_id, "s5");
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
