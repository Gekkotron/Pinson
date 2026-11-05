//! Favorite songs, kept newest-added first and saved to a JSON file.
//!
//! The store is cheap to clone (it shares one `Arc`). Every change is written
//! to disk before it becomes visible, and subscribers are notified through a
//! `watch` channel. The file is tiny, so blocking `std::fs` calls inside the
//! mutex are acceptable.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Context;
use tokio::sync::watch;

use crate::song::Song;

struct Inner {
    /// `None` for an in-memory store (tests): nothing is persisted.
    path: Option<PathBuf>,
    songs: Mutex<Vec<Song>>,
    changes: watch::Sender<Vec<Song>>,
}

#[derive(Clone)]
pub struct Favorites {
    inner: Arc<Inner>,
}

impl Favorites {
    /// Loads the favorites file. A missing file is an empty list; a file that
    /// can't be read or parsed is an error.
    pub fn load(path: PathBuf) -> anyhow::Result<Self> {
        let songs: Vec<Song> = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("parsing favorites file {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("reading favorites file {}", path.display()));
            }
        };
        Ok(Self::new(Some(path), songs))
    }

    /// A store that is never written to disk.
    pub fn in_memory() -> Self {
        Self::new(None, Vec::new())
    }

    fn new(path: Option<PathBuf>, songs: Vec<Song>) -> Self {
        let (changes, _) = watch::channel(songs.clone());
        Self {
            inner: Arc::new(Inner {
                path,
                songs: Mutex::new(songs),
                changes,
            }),
        }
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
        match songs.iter_mut().find(|s| s.video_id == song.video_id) {
            Some(existing) => *existing = song,
            None => songs.insert(0, song),
        }
        self.commit(&mut guard, songs)
    }

    /// Removes a favorite; returns whether it was one.
    pub fn remove(&self, video_id: &str) -> anyhow::Result<bool> {
        let mut guard = self.lock();
        let Some(index) = guard.iter().position(|s| s.video_id == video_id) else {
            return Ok(false);
        };
        let mut songs = guard.clone();
        songs.remove(index);
        self.commit(&mut guard, songs)?;
        Ok(true)
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
    fn commit(&self, guard: &mut Vec<Song>, songs: Vec<Song>) -> anyhow::Result<()> {
        if let Some(path) = &self.inner.path {
            save(path, &songs)?;
        }
        *guard = songs.clone();
        self.inner.changes.send_replace(songs);
        Ok(())
    }
}

/// Writes `<path>.tmp`, then renames it over `path`.
fn save(path: &Path, songs: &[Song]) -> anyhow::Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating directory {}", dir.display()))?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let json = serde_json::to_vec_pretty(songs).context("serializing favorites")?;
    std::fs::write(&tmp, json).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("renaming {} to {}", tmp.display(), path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
                "music-favorites-{tag}-{}-{:?}",
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

    #[test]
    fn persist_roundtrip() {
        let dir = TempDir::new("roundtrip");
        let path = dir.0.join("nested").join("favorites.json");
        let f = Favorites::load(path.clone()).unwrap();
        assert!(f.list().is_empty(), "missing file is empty");
        f.add(song("a")).unwrap();
        f.add(song("b")).unwrap();
        f.remove("a").unwrap();
        f.add(song("c")).unwrap();
        assert!(!path.with_extension("json.tmp").exists());

        let reloaded = Favorites::load(path.clone()).unwrap();
        assert_eq!(ids(&reloaded), ["c", "b"]);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"videoId\": \"c\""), "pretty JSON: {text}");
    }

    #[test]
    fn unparseable_file_is_an_error_with_context() {
        let dir = TempDir::new("corrupt");
        std::fs::create_dir_all(&dir.0).unwrap();
        let path = dir.0.join("favorites.json");
        std::fs::write(&path, "not json").unwrap();
        let err = Favorites::load(path).err().expect("load fails");
        assert!(format!("{err:#}").contains("favorites.json"), "{err:#}");
    }

    #[test]
    fn failed_save_leaves_list_unchanged() {
        let dir = TempDir::new("failsave");
        let f = Favorites::load(dir.0.join("blocker").join("favorites.json")).unwrap();
        // The parent "directory" is a file, so saving must fail.
        std::fs::create_dir_all(&dir.0).unwrap();
        std::fs::write(dir.0.join("blocker"), "x").unwrap();
        assert!(f.add(song("a")).is_err());
        assert!(f.list().is_empty());
    }
}
