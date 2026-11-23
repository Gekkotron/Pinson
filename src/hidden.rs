//! Songs and artists the house never wants picked automatically.
//!
//! Like [`Favorites`](crate::favorites::Favorites), the store is cheap to
//! clone, every change is saved to SQLite before it becomes visible, and the
//! current [`HiddenSet`] is published through a `watch` channel.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use anyhow::Context;
use serde::Serialize;
use tokio::sync::watch;

use crate::db::{Db, SONG_COLUMNS, read_song, song_params};
use crate::song::Song;
use crate::stats::split_artists;

/// What is hidden, shaped for fast lookups.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HiddenSet {
    /// Video ids of hidden songs.
    pub songs: HashSet<String>,
    /// Hidden artists, trimmed and lowercase.
    pub artists: HashSet<String>,
}

impl HiddenSet {
    /// Whether `song` is hidden itself or has a hidden artist among its credits.
    pub fn hides(&self, song: &Song) -> bool {
        self.songs.contains(&song.video_id)
            || (!self.artists.is_empty()
                && split_artists(&song.artists)
                    .into_iter()
                    .any(|a| self.artists.contains(&artist_key(a))))
    }
}

/// How an artist is matched: trimmed and lowercase.
fn artist_key(name: &str) -> String {
    name.trim().to_lowercase()
}

/// A hidden song as the API lists it: the song's fields plus when it was hidden.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HiddenSong {
    #[serde(flatten)]
    pub song: Song,
    #[serde(serialize_with = "rfc3339")]
    pub hidden_at: i64,
}

/// A hidden artist as the API lists it.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HiddenArtist {
    pub name: String,
    #[serde(serialize_with = "rfc3339")]
    pub hidden_at: i64,
}

/// Everything hidden, newest first.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct HiddenList {
    pub songs: Vec<HiddenSong>,
    pub artists: Vec<HiddenArtist>,
}

fn rfc3339<S: serde::Serializer>(secs: &i64, s: S) -> Result<S::Ok, S::Error> {
    let at = chrono::DateTime::from_timestamp(*secs, 0).unwrap_or_default();
    s.serialize_str(&at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
}

struct Inner {
    db: Db,
    list: Mutex<HiddenList>,
    changes: watch::Sender<HiddenSet>,
}

#[derive(Clone)]
pub struct Hidden {
    inner: Arc<Inner>,
}

impl Hidden {
    /// Loads what is hidden in `db`.
    pub fn open(db: Db) -> anyhow::Result<Self> {
        let list = db.with(|c| {
            let mut stmt = c.prepare(&format!(
                "SELECT {SONG_COLUMNS}, hidden_at FROM hidden_songs
                 ORDER BY hidden_at DESC, rowid DESC"
            ))?;
            let songs = stmt
                .query_map([], |r| {
                    Ok(HiddenSong {
                        song: read_song(r, 0)?,
                        hidden_at: r.get(6)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let mut stmt = c.prepare(
                "SELECT name, hidden_at FROM hidden_artists
                 ORDER BY hidden_at DESC, rowid DESC",
            )?;
            let artists = stmt
                .query_map([], |r| {
                    Ok(HiddenArtist {
                        name: r.get(0)?,
                        hidden_at: r.get(1)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(HiddenList { songs, artists })
        })?;
        let (changes, _) = watch::channel(set_of(&list));
        Ok(Self {
            inner: Arc::new(Inner {
                db,
                list: Mutex::new(list),
                changes,
            }),
        })
    }

    /// A store backed by a private in-memory database.
    pub fn in_memory() -> Self {
        Self::open(Db::in_memory()).expect("open in-memory hidden")
    }

    /// Everything hidden, newest first.
    pub fn list(&self) -> HiddenList {
        self.lock().clone()
    }

    /// The current lookup set.
    pub fn snapshot(&self) -> HiddenSet {
        self.inner.changes.borrow().clone()
    }

    /// The lookup set, updated after every change.
    pub fn subscribe(&self) -> watch::Receiver<HiddenSet> {
        self.inner.changes.subscribe()
    }

    /// Hides a song. One that is already hidden keeps its place and gets the
    /// new metadata.
    pub fn hide_song(&self, song: Song) -> anyhow::Result<()> {
        let mut guard = self.lock();
        let at = chrono::Utc::now().timestamp();
        self.inner
            .db
            .with(|c| {
                let p = song_params(&song);
                c.execute(
                    &format!(
                        "INSERT INTO hidden_songs ({SONG_COLUMNS}, hidden_at)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                         ON CONFLICT(video_id) DO UPDATE SET
                             title = excluded.title, artists = excluded.artists,
                             album = excluded.album,
                             duration_secs = excluded.duration_secs,
                             artwork_url = excluded.artwork_url"
                    ),
                    rusqlite::params![p[0], p[1], p[2], p[3], p[4], p[5], at],
                )
                .map(|_| ())
            })
            .context("saving hidden song")?;
        match guard
            .songs
            .iter_mut()
            .find(|h| h.song.video_id == song.video_id)
        {
            Some(existing) => existing.song = song,
            None => guard.songs.insert(
                0,
                HiddenSong {
                    song,
                    hidden_at: at,
                },
            ),
        }
        self.publish(&guard);
        Ok(())
    }

    /// Shows a song again; returns whether it was hidden.
    pub fn unhide_song(&self, video_id: &str) -> anyhow::Result<bool> {
        let mut guard = self.lock();
        let Some(index) = guard.songs.iter().position(|h| h.song.video_id == video_id) else {
            return Ok(false);
        };
        self.inner
            .db
            .with(|c| {
                c.execute("DELETE FROM hidden_songs WHERE video_id = ?1", [video_id])
                    .map(|_| ())
            })
            .context("removing hidden song")?;
        guard.songs.remove(index);
        self.publish(&guard);
        Ok(true)
    }

    /// Hides an artist, matched case-insensitively. A blank name is an error.
    /// An artist that is already hidden is left as it is.
    pub fn hide_artist(&self, name: &str) -> anyhow::Result<()> {
        let name = name.trim();
        anyhow::ensure!(!name.is_empty(), "artist name must not be blank");
        let key = artist_key(name);
        let mut guard = self.lock();
        if guard.artists.iter().any(|a| artist_key(&a.name) == key) {
            return Ok(());
        }
        let at = chrono::Utc::now().timestamp();
        self.inner
            .db
            .with(|c| {
                c.execute(
                    "INSERT INTO hidden_artists (key, name, hidden_at) VALUES (?1, ?2, ?3)",
                    rusqlite::params![key, name, at],
                )
                .map(|_| ())
            })
            .context("saving hidden artist")?;
        guard.artists.insert(
            0,
            HiddenArtist {
                name: name.to_owned(),
                hidden_at: at,
            },
        );
        self.publish(&guard);
        Ok(())
    }

    /// Shows an artist again; returns whether it was hidden.
    pub fn unhide_artist(&self, name: &str) -> anyhow::Result<bool> {
        let key = artist_key(name);
        let mut guard = self.lock();
        let Some(index) = guard
            .artists
            .iter()
            .position(|a| artist_key(&a.name) == key)
        else {
            return Ok(false);
        };
        self.inner
            .db
            .with(|c| {
                c.execute("DELETE FROM hidden_artists WHERE key = ?1", [&key])
                    .map(|_| ())
            })
            .context("removing hidden artist")?;
        guard.artists.remove(index);
        self.publish(&guard);
        Ok(true)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HiddenList> {
        // The list is only changed after the database write succeeded, so a
        // poisoned lock still guards a consistent list.
        self.inner.list.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn publish(&self, list: &HiddenList) {
        self.inner.changes.send_replace(set_of(list));
    }
}

fn set_of(list: &HiddenList) -> HiddenSet {
    HiddenSet {
        songs: list.songs.iter().map(|h| h.song.video_id.clone()).collect(),
        artists: list.artists.iter().map(|a| artist_key(&a.name)).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn song(id: &str, artists: &str) -> Song {
        Song {
            video_id: id.into(),
            title: format!("Title {id}"),
            artists: artists.into(),
            album: Some("Album".into()),
            duration_secs: Some(200),
            artwork_url: Some(format!("https://art/{id}")),
        }
    }

    fn ids(h: &Hidden) -> Vec<String> {
        h.list()
            .songs
            .into_iter()
            .map(|s| s.song.video_id)
            .collect()
    }

    /// A fresh directory under the system temp dir, removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "pinson-hidden-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn hides_a_song_by_id() {
        let h = Hidden::in_memory();
        h.hide_song(song("a", "X")).unwrap();
        let set = h.snapshot();
        assert!(set.hides(&song("a", "Someone else")));
        assert!(!set.hides(&song("b", "X")));
    }

    #[test]
    fn hides_an_artist_anywhere_in_the_credit_ignoring_case() {
        let h = Hidden::in_memory();
        h.hide_artist("  daft PUNK ").unwrap();
        let set = h.snapshot();
        for credit in [
            "Daft Punk",
            "A, Daft Punk & C feat. D",
            "A, B & C feat. DAFT punk",
            "A ft. Daft Punk",
        ] {
            assert!(set.hides(&song("s", credit)), "{credit}");
        }
        assert!(!set.hides(&song("s", "Daft Punks")));
        assert!(!set.hides(&song("s", "Punk")));
    }

    #[test]
    fn middle_and_last_credits_match() {
        let h = Hidden::in_memory();
        h.hide_artist("C").unwrap();
        assert!(h.snapshot().hides(&song("s", "A, B & C feat. D")));
        h.hide_artist("d").unwrap();
        h.unhide_artist("C").unwrap();
        assert!(h.snapshot().hides(&song("s", "A, B & C feat. D")));
        h.unhide_artist("D").unwrap();
        assert!(!h.snapshot().hides(&song("s", "A, B & C feat. D")));
    }

    #[test]
    fn empty_set_hides_nothing() {
        assert!(!HiddenSet::default().hides(&song("a", "X")));
    }

    #[test]
    fn list_is_newest_first_and_hiding_twice_keeps_one_row() {
        let h = Hidden::in_memory();
        h.hide_song(song("a", "X")).unwrap();
        h.hide_song(song("b", "X")).unwrap();
        h.hide_song(song("a", "X")).unwrap();
        assert_eq!(ids(&h), ["b", "a"]);
        h.hide_artist("One").unwrap();
        h.hide_artist("Two").unwrap();
        h.hide_artist("one").unwrap();
        let names: Vec<_> = h.list().artists.into_iter().map(|a| a.name).collect();
        assert_eq!(names, ["Two", "One"]);
    }

    #[test]
    fn unhide_reports_whether_it_was_hidden() {
        let h = Hidden::in_memory();
        h.hide_song(song("a", "X")).unwrap();
        h.hide_artist("Y").unwrap();
        assert!(h.unhide_song("a").unwrap());
        assert!(!h.unhide_song("a").unwrap());
        assert!(h.unhide_artist("y").unwrap());
        assert!(!h.unhide_artist("Y").unwrap());
        assert_eq!(h.snapshot(), HiddenSet::default());
    }

    #[test]
    fn blank_artist_is_refused() {
        assert!(Hidden::in_memory().hide_artist("   ").is_err());
    }

    #[test]
    fn subscribers_see_changes() {
        let h = Hidden::in_memory();
        let mut rx = h.subscribe();
        rx.borrow_and_update();
        h.hide_song(song("a", "X")).unwrap();
        assert!(rx.has_changed().unwrap());
        assert!(rx.borrow_and_update().songs.contains("a"));
        h.hide_artist("X").unwrap();
        assert!(rx.has_changed().unwrap());
        assert!(rx.borrow_and_update().artists.contains("x"));
    }

    #[test]
    fn survives_reopen_with_song_fields() {
        let dir = TempDir::new("reopen");
        let path = dir.0.join("pinson.db");
        let h = Hidden::open(Db::open(&path).unwrap()).unwrap();
        h.hide_song(song("a", "X")).unwrap();
        h.hide_song(song("b", "Y")).unwrap();
        h.hide_artist("Zed").unwrap();
        h.unhide_song("a").unwrap();
        drop(h);
        let h = Hidden::open(Db::open(&path).unwrap()).unwrap();
        let list = h.list();
        assert_eq!(list.songs.len(), 1);
        assert_eq!(list.songs[0].song, song("b", "Y"));
        assert_eq!(list.artists[0].name, "Zed");
        let set = h.snapshot();
        assert!(set.songs.contains("b") && set.artists.contains("zed"));
    }

    #[test]
    fn serializes_song_fields_and_rfc3339_time() {
        let h = Hidden::in_memory();
        h.hide_song(song("a", "X")).unwrap();
        h.hide_artist("Y").unwrap();
        let v = serde_json::to_value(h.list()).unwrap();
        assert_eq!(v["songs"][0]["videoId"], "a");
        assert_eq!(v["songs"][0]["artworkUrl"], "https://art/a");
        assert_eq!(v["artists"][0]["name"], "Y");
        for at in [&v["songs"][0]["hiddenAt"], &v["artists"][0]["hiddenAt"]] {
            let at = at.as_str().unwrap();
            assert!(chrono::DateTime::parse_from_rfc3339(at).is_ok(), "{at}");
        }
    }

    #[test]
    fn failed_save_leaves_the_list_unchanged() {
        let db = Db::in_memory();
        let h = Hidden::open(db.clone()).unwrap();
        h.hide_song(song("a", "X")).unwrap();
        db.with(|c| c.execute("DROP TABLE hidden_songs", []).map(|_| ()))
            .unwrap();
        assert!(h.hide_song(song("b", "X")).is_err());
        assert_eq!(ids(&h), ["a"]);
        assert!(!h.snapshot().songs.contains("b"));
    }
}
