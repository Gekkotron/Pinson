//! Favorite songs, kept newest-added first and saved to SQLite.
//!
//! The store is cheap to clone (it shares one `Arc`). Every change is written
//! to the database before it becomes visible, and subscribers are notified
//! through a `watch` channel. The list is tiny, so blocking database calls
//! inside the mutex are acceptable.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use anyhow::Context;
use tokio::sync::watch;

use crate::db::{Db, playlist_tracks, refresh_song, replace_playlist_tracks};
use crate::song::Song;

/// Why [`Favorites::reorder`] refused an order.
#[derive(Debug, thiserror::Error)]
pub enum FavoritesOrderError {
    #[error("videoIds must not repeat")]
    Repeat,
    #[error("videoIds must list exactly the favorites")]
    Mismatch,
    #[error("{0:#}")]
    Io(anyhow::Error),
}

struct Inner {
    db: Db,
    songs: Mutex<Vec<Song>>,
    changes: watch::Sender<Vec<Song>>,
}

#[derive(Clone)]
pub struct Favorites {
    inner: Arc<Inner>,
}

impl Favorites {
    /// Loads the favorites stored in `db`, newest-added first.
    pub fn open(db: Db) -> anyhow::Result<Self> {
        let songs = db.with(|c| playlist_tracks(c, &favorites_id(c)?))?;
        let (changes, _) = watch::channel(songs.clone());
        Ok(Self {
            inner: Arc::new(Inner {
                db,
                songs: Mutex::new(songs),
                changes,
            }),
        })
    }

    /// A store backed by a private in-memory database.
    pub fn in_memory() -> Self {
        Self::open(Db::in_memory()).expect("open in-memory favorites")
    }

    /// All favorites, newest-added first.
    pub fn list(&self) -> Vec<Song> {
        self.lock().clone()
    }

    /// Adds a favorite at the front. A song that is already a favorite keeps
    /// its position and gets the new metadata.
    pub fn add(&self, song: Song) -> anyhow::Result<()> {
        let mut guard = self.lock();
        let mut songs = guard.clone();
        if !songs.iter().any(|s| s.video_id == song.video_id) {
            songs.insert(0, song.clone());
        }
        self.commit(&mut guard, &songs, Some(&song))
    }

    /// Removes a favorite; returns whether it was one.
    pub fn remove(&self, video_id: &str) -> anyhow::Result<bool> {
        let mut guard = self.lock();
        let Some(index) = guard.iter().position(|s| s.video_id == video_id) else {
            return Ok(false);
        };
        let mut songs = guard.clone();
        songs.remove(index);
        self.commit(&mut guard, &songs, None)?;
        Ok(true)
    }

    /// Puts the favorites in the given order. `video_ids` must name every
    /// favorite exactly once.
    pub fn reorder(&self, video_ids: Vec<String>) -> Result<(), FavoritesOrderError> {
        let mut seen = HashSet::new();
        if !video_ids.iter().all(|v| seen.insert(v.as_str())) {
            return Err(FavoritesOrderError::Repeat);
        }
        let mut guard = self.lock();
        if guard.len() != video_ids.len()
            || !guard.iter().all(|s| seen.contains(s.video_id.as_str()))
        {
            return Err(FavoritesOrderError::Mismatch);
        }
        let mut songs = guard.clone();
        songs.sort_by_key(|s| video_ids.iter().position(|v| *v == s.video_id));
        self.commit(&mut guard, &songs, None)
            .map_err(FavoritesOrderError::Io)
    }

    /// The current list, updated after every change.
    pub fn subscribe(&self) -> watch::Receiver<Vec<Song>> {
        self.inner.changes.subscribe()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Song>> {
        // A poisoned lock only means another thread panicked mid-read; the
        // list itself is always replaced whole, so it is still consistent.
        self.inner.songs.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Saves `songs`, then makes them the current list and notifies subscribers.
    fn commit(
        &self,
        guard: &mut Vec<Song>,
        songs: &[Song],
        refresh: Option<&Song>,
    ) -> anyhow::Result<()> {
        let songs = self
            .inner
            .db
            .with(|c| {
                let tx = c.transaction()?;
                if let Some(song) = refresh {
                    refresh_song(&tx, song)?;
                }
                save_favorites(&tx, songs)?;
                let stored = playlist_tracks(&tx, &favorites_id(&tx)?)?;
                tx.commit()?;
                Ok(stored)
            })
            .context("saving favorites")?;
        if refresh.is_some() {
            self.inner.db.songs_changed();
        }
        *guard = songs.clone();
        self.inner.changes.send_replace(songs);
        Ok(())
    }
}

/// Replaces the stored favorites with `songs`, `position` being the index.
pub(crate) fn save_favorites(tx: &rusqlite::Transaction, songs: &[Song]) -> rusqlite::Result<()> {
    replace_playlist_tracks(tx, &favorites_id(tx)?, songs)
}

fn favorites_id(conn: &rusqlite::Connection) -> rusqlite::Result<String> {
    conn.query_row(
        "SELECT id FROM playlists WHERE kind = 'favorites'",
        [],
        |r| r.get(0),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn song(id: &str) -> Song {
        Song {
            video_id: id.into(),
            title: id.into(),
            artists: "artist".into(),
            album: None,
            duration_secs: None,
            artwork_url: None,
        }
    }

    fn ids(f: &Favorites) -> Vec<String> {
        f.list().into_iter().map(|s| s.video_id).collect()
    }

    /// A fresh directory under the system temp dir, removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "pinson-favorites-{tag}-{}-{:?}",
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
    fn newest_added_first() {
        let f = Favorites::in_memory();
        f.add(song("a")).unwrap();
        f.add(song("b")).unwrap();
        f.add(song("c")).unwrap();
        assert_eq!(ids(&f), ["c", "b", "a"]);
    }

    #[test]
    fn add_is_idempotent_keeps_position_and_refreshes_metadata() {
        let f = Favorites::in_memory();
        f.add(song("a")).unwrap();
        f.add(song("b")).unwrap();
        let mut a = song("a");
        a.title = "new title".into();
        f.add(a).unwrap();
        assert_eq!(ids(&f), ["b", "a"]);
        assert_eq!(f.list()[1].title, "new title");
    }

    #[test]
    fn remove_reports_whether_it_was_a_favorite() {
        let f = Favorites::in_memory();
        f.add(song("a")).unwrap();
        f.add(song("b")).unwrap();
        assert!(f.remove("a").unwrap());
        assert!(!f.remove("a").unwrap());
        assert!(!f.remove("zzz").unwrap());
        assert_eq!(ids(&f), ["b"]);
    }

    #[test]
    fn subscribers_see_changes() {
        let f = Favorites::in_memory();
        let mut rx = f.subscribe();
        assert!(rx.borrow_and_update().is_empty());
        f.add(song("a")).unwrap();
        assert!(rx.has_changed().unwrap());
        assert_eq!(rx.borrow_and_update().len(), 1);
        f.remove("a").unwrap();
        assert!(rx.has_changed().unwrap());
        assert!(rx.borrow_and_update().is_empty());
    }

    fn strings(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn reorder_sets_the_order() {
        let f = Favorites::in_memory();
        for id in ["a", "b", "c"] {
            f.add(song(id)).unwrap();
        }
        let mut rx = f.subscribe();
        rx.borrow_and_update();
        f.reorder(strings(&["a", "c", "b"])).unwrap();
        assert_eq!(ids(&f), ["a", "c", "b"]);
        assert!(rx.has_changed().unwrap());
    }

    #[test]
    fn reorder_rejects_repeat() {
        let f = Favorites::in_memory();
        f.add(song("a")).unwrap();
        f.add(song("b")).unwrap();
        assert!(matches!(
            f.reorder(strings(&["a", "a", "b"])),
            Err(FavoritesOrderError::Repeat)
        ));
        assert_eq!(ids(&f), ["b", "a"]);
    }

    #[test]
    fn reorder_rejects_wrong_set() {
        let f = Favorites::in_memory();
        f.add(song("a")).unwrap();
        f.add(song("b")).unwrap();
        for bad in [&["a"][..], &["a", "b", "c"], &["a", "z"]] {
            assert!(matches!(
                f.reorder(strings(bad)),
                Err(FavoritesOrderError::Mismatch)
            ));
        }
        assert_eq!(ids(&f), ["b", "a"]);
    }

    #[test]
    fn reorder_persists() {
        let dir = TempDir::new("reorder");
        let path = dir.0.join("pinson.db");
        let f = Favorites::open(Db::open(&path).unwrap()).unwrap();
        for id in ["A", "B", "C"] {
            f.add(song(id)).unwrap();
        }
        f.reorder(strings(&["A", "C", "B"])).unwrap();
        drop(f);
        let f = Favorites::open(Db::open(&path).unwrap()).unwrap();
        assert_eq!(ids(&f), ["A", "C", "B"]);
    }

    #[test]
    fn add_after_reorder_goes_first() {
        let f = Favorites::in_memory();
        f.add(song("a")).unwrap();
        f.add(song("b")).unwrap();
        f.reorder(strings(&["a", "b"])).unwrap();
        f.add(song("c")).unwrap();
        assert_eq!(ids(&f), ["c", "a", "b"]);
    }

    #[test]
    fn survives_reopen() {
        let dir = TempDir::new("reopen");
        let path = dir.0.join("pinson.db");
        let f = Favorites::open(Db::open(&path).unwrap()).unwrap();
        assert!(f.list().is_empty());
        f.add(song("A")).unwrap();
        f.add(song("B")).unwrap();
        drop(f);
        let f = Favorites::open(Db::open(&path).unwrap()).unwrap();
        assert_eq!(ids(&f), ["B", "A"]);
    }

    #[test]
    fn remove_survives_reopen() {
        let dir = TempDir::new("remove");
        let path = dir.0.join("pinson.db");
        let f = Favorites::open(Db::open(&path).unwrap()).unwrap();
        f.add(song("A")).unwrap();
        f.add(song("B")).unwrap();
        f.remove("A").unwrap();
        drop(f);
        let f = Favorites::open(Db::open(&path).unwrap()).unwrap();
        assert_eq!(ids(&f), ["B"]);
    }

    #[test]
    fn failed_save_leaves_list_unchanged() {
        let db = Db::in_memory();
        let f = Favorites::open(db.clone()).unwrap();
        f.add(song("a")).unwrap();
        db.with(|c| c.execute("DROP TABLE playlist_songs", []).map(|_| ()))
            .unwrap();
        assert!(f.add(song("b")).is_err());
        assert_eq!(ids(&f), ["a"]);
    }
}
