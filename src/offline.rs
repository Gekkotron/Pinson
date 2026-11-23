//! Favorites kept on the server's disk, so they play without YouTube.
//!
//! [`OfflineIndex`] knows which songs have a saved file; the source plays
//! from it. [`Offline`] is the background service that downloads every
//! favorite one at a time and deletes the files of songs that are no longer
//! favorites.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use serde::Serialize;
use tokio::sync::watch;
use tokio::time::Instant;

use crate::db::Db;
use crate::favorites::Favorites;
use crate::song::Song;
use crate::source::YtDlpFfmpegSource;

/// Settings key holding "on" or "off".
pub const OFFLINE_SETTING: &str = "offline_favorites";
/// Pause between two downloads, so playback and YouTube rate limits are spared.
const PAUSE_BETWEEN: Duration = Duration::from_secs(3);
/// Longest time one song may take to download.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// How long a failed song is left alone before it is tried again.
const RETRY_AFTER: Duration = Duration::from_secs(6 * 3600);
/// How long favorites must stay unchanged before a sync starts.
const DEBOUNCE: Duration = Duration::from_secs(30);
/// How often a sync runs on its own, to retry failed songs.
const RESYNC_EVERY: Duration = Duration::from_secs(6 * 3600);

/// Off unless the stored value is "on".
pub fn offline_enabled(db: &Db) -> anyhow::Result<bool> {
    Ok(db.setting(OFFLINE_SETTING)?.as_deref() == Some("on"))
}

/// Stores the switch.
pub fn set_offline_enabled(db: &Db, on: bool) -> anyhow::Result<()> {
    db.set_setting(OFFLINE_SETTING, if on { "on" } else { "off" })
}

/// Whether `id` could be a YouTube video id; it becomes part of a file name.
pub fn valid_video_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// The saved files: video id to its path and size in bytes. Cheap to clone;
/// clones share the same map.
#[derive(Clone, Default)]
pub struct OfflineIndex {
    files: Arc<RwLock<HashMap<String, (PathBuf, u64)>>>,
}

impl OfflineIndex {
    /// Reads the files in `dir`. Folders (such as `tmp/`) and names that are
    /// not `<video id>.<ext>` are ignored; a missing `dir` gives an empty index.
    pub fn scan(dir: &Path) -> Self {
        let index = Self::default();
        let Ok(entries) = std::fs::read_dir(dir) else {
            return index;
        };
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            if !meta.is_file() {
                continue;
            }
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let mut parts = name.split('.');
            let (Some(id), Some(ext), None) = (parts.next(), parts.next(), parts.next()) else {
                continue;
            };
            if valid_video_id(id) && !ext.is_empty() {
                index.insert(id, entry.path(), meta.len());
            }
        }
        index
    }

    /// The saved file of `video_id`, if there is one.
    pub fn path(&self, video_id: &str) -> Option<PathBuf> {
        self.read().get(video_id).map(|(path, _)| path.clone())
    }

    /// Records a saved file.
    pub fn insert(&self, video_id: &str, path: PathBuf, bytes: u64) {
        self.write().insert(video_id.to_owned(), (path, bytes));
    }

    /// Forgets a song's file (the file itself is not touched); returns its path.
    pub fn remove(&self, video_id: &str) -> Option<PathBuf> {
        self.write().remove(video_id).map(|(path, _)| path)
    }

    /// Total size of the saved files.
    pub fn bytes(&self) -> u64 {
        self.read().values().map(|(_, b)| b).sum()
    }

    /// Every saved song id with its path.
    fn entries(&self) -> Vec<(String, PathBuf)> {
        self.read()
            .iter()
            .map(|(id, (path, _))| (id.clone(), path.clone()))
            .collect()
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, HashMap<String, (PathBuf, u64)>> {
        self.files.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, HashMap<String, (PathBuf, u64)>> {
        self.files.write().unwrap_or_else(|e| e.into_inner())
    }
}

/// Fetches one song's audio into a folder.
#[async_trait]
pub trait Downloader: Send + Sync + 'static {
    /// Downloads `video_id` into `tmp_dir` and returns the file it wrote.
    async fn download(&self, video_id: &str, tmp_dir: &Path) -> anyhow::Result<PathBuf>;
}

#[async_trait]
impl Downloader for YtDlpFfmpegSource {
    async fn download(&self, video_id: &str, tmp_dir: &Path) -> anyhow::Result<PathBuf> {
        YtDlpFfmpegSource::download(self, video_id, tmp_dir).await
    }
}

/// A downloader that always fails, for services that must never reach YouTube.
struct NoDownloads;

#[async_trait]
impl Downloader for NoDownloads {
    async fn download(&self, _video_id: &str, _tmp_dir: &Path) -> anyhow::Result<PathBuf> {
        anyhow::bail!("downloads are not available")
    }
}

/// The song being downloaded.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Downloading {
    pub video_id: String,
    pub title: String,
    pub artist: String,
}

/// What `GET /api/offline` returns.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OfflineStatus {
    /// The switch.
    pub enabled: bool,
    /// Favorites that have a saved file.
    pub saved: usize,
    /// Number of favorites.
    pub total: usize,
    /// Size of the saved files.
    pub bytes: u64,
    /// The storage cap; `null` when there is none.
    pub limit_bytes: Option<u64>,
    /// The cap is reached and some favorites are not saved.
    pub full: bool,
    /// The song being downloaded; `null` when idle.
    pub downloading: Option<Downloading>,
    /// Favorites waiting for a retry after a failed download.
    pub failed: usize,
}

struct Timing {
    pause: Duration,
    retry_after: Duration,
    debounce: Duration,
    resync: Duration,
}

#[derive(Default)]
struct State {
    downloading: Option<Song>,
    failed: HashMap<String, Instant>,
}

struct Inner {
    index: OfflineIndex,
    dir: PathBuf,
    db: Db,
    favorites: Favorites,
    max_bytes: Option<u64>,
    downloader: Arc<dyn Downloader>,
    enabled: watch::Sender<bool>,
    state: Mutex<State>,
    timing: Timing,
}

/// The background service that keeps favorites on disk. Cheap to clone.
#[derive(Clone)]
pub struct Offline {
    inner: Arc<Inner>,
}

impl Offline {
    /// A service saving into `dir`, which `index` must describe. `max_bytes`
    /// is the storage cap (`None`: no cap). The switch starts as stored in `db`.
    /// Nothing runs until [`Offline::start`].
    pub fn new(
        dir: PathBuf,
        index: OfflineIndex,
        db: Db,
        favorites: Favorites,
        max_bytes: Option<u64>,
        downloader: Arc<dyn Downloader>,
    ) -> anyhow::Result<Self> {
        let enabled = offline_enabled(&db)?;
        Ok(Self::build(
            dir,
            index,
            db,
            favorites,
            max_bytes,
            downloader,
            enabled,
            Timing {
                pause: PAUSE_BETWEEN,
                retry_after: RETRY_AFTER,
                debounce: DEBOUNCE,
                resync: RESYNC_EVERY,
            },
        ))
    }

    /// A service that can never download (for tests of the API): off, empty.
    pub fn idle(dir: PathBuf, db: Db, favorites: Favorites) -> Self {
        let index = OfflineIndex::scan(&dir);
        Self::new(dir, index, db, favorites, None, Arc::new(NoDownloads))
            .expect("read the offline setting")
    }

    #[allow(clippy::too_many_arguments)]
    fn build(
        dir: PathBuf,
        index: OfflineIndex,
        db: Db,
        favorites: Favorites,
        max_bytes: Option<u64>,
        downloader: Arc<dyn Downloader>,
        enabled: bool,
        timing: Timing,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                index,
                dir,
                db,
                favorites,
                max_bytes,
                downloader,
                enabled: watch::channel(enabled).0,
                state: Mutex::default(),
                timing,
            }),
        }
    }

    /// Starts the background task: syncs now, then whenever favorites change
    /// (after they stay still for 30 s), the switch flips, or 6 h pass.
    /// Leftover partial downloads are cleared first.
    pub fn start(&self) {
        let this = self.clone();
        tokio::spawn(async move {
            let _ = tokio::fs::remove_dir_all(this.inner.dir.join("tmp")).await;
            let mut favorites = this.inner.favorites.subscribe();
            let mut enabled = this.inner.enabled.subscribe();
            this.sync().await;
            loop {
                tokio::select! {
                    r = enabled.changed() => if r.is_err() { return },
                    r = favorites.changed() => {
                        if r.is_err() {
                            return;
                        }
                        tokio::time::sleep(this.inner.timing.debounce).await;
                        favorites.borrow_and_update();
                    }
                    () = tokio::time::sleep(this.inner.timing.resync) => {}
                }
                this.sync().await;
            }
        });
    }

    /// Whether favorites are being kept.
    pub fn enabled(&self) -> bool {
        *self.inner.enabled.borrow()
    }

    /// Turns the switch on or off and stores it. Off stops the download in
    /// progress and deletes every saved file.
    pub async fn set_enabled(&self, on: bool) -> anyhow::Result<()> {
        let db = self.inner.db.clone();
        tokio::task::spawn_blocking(move || set_offline_enabled(&db, on))
            .await
            .map_err(|e| anyhow::anyhow!("offline setting task failed: {e}"))??;
        self.inner.enabled.send_replace(on);
        if !on {
            self.delete_all().await;
        }
        Ok(())
    }

    /// Deletes every saved file and the partial downloads.
    async fn delete_all(&self) {
        let files = self.inner.index.entries();
        for (id, _) in &files {
            self.inner.index.remove(id);
        }
        let tmp = self.inner.dir.join("tmp");
        let removed = tokio::task::spawn_blocking(move || {
            for (_, path) in files {
                remove_file_logged(&path);
            }
            let _ = std::fs::remove_dir_all(tmp);
        })
        .await;
        if let Err(e) = removed {
            tracing::warn!("offline: deleting the saved songs failed: {e}");
        }
    }

    /// The switch, progress and what is downloading now.
    pub fn status(&self) -> OfflineStatus {
        let favorites = self.inner.favorites.list();
        let saved = favorites
            .iter()
            .filter(|s| self.inner.index.path(&s.video_id).is_some())
            .count();
        let bytes = self.inner.index.bytes();
        let state = self.state();
        let failed = favorites
            .iter()
            .filter(|s| {
                self.inner.index.path(&s.video_id).is_none() && self.in_backoff(&state, &s.video_id)
            })
            .count();
        let downloading = state.downloading.as_ref().map(|s| Downloading {
            video_id: s.video_id.clone(),
            title: s.title.clone(),
            artist: s.artists.clone(),
        });
        OfflineStatus {
            enabled: self.enabled(),
            saved,
            total: favorites.len(),
            bytes,
            limit_bytes: self.inner.max_bytes,
            full: self.cap_reached() && saved < favorites.len(),
            downloading,
            failed,
        }
    }

    /// One pass: deletes files of songs that are no longer favorites, then
    /// downloads the missing ones, newest favorite first. Does nothing when
    /// off; stops early when switched off or when the cap is reached.
    async fn sync(&self) {
        if !self.enabled() {
            return;
        }
        let favorites = self.inner.favorites.list();
        for (id, path) in self.inner.index.entries() {
            if !favorites.iter().any(|s| s.video_id == id) {
                self.inner.index.remove(&id);
                remove_file_logged(&path);
                tracing::info!("offline: {id} is no longer a favorite, copy deleted");
            }
        }
        for song in favorites {
            if !self.enabled() || self.cap_reached() {
                break;
            }
            if self.inner.index.path(&song.video_id).is_some() {
                continue;
            }
            if self.in_backoff(&self.state(), &song.video_id) {
                continue;
            }
            self.download_one(song).await;
            tokio::time::sleep(self.inner.timing.pause).await;
        }
    }

    /// Downloads one song and records the result.
    async fn download_one(&self, song: Song) {
        let id = song.video_id.clone();
        self.state().downloading = Some(song);
        let tmp = self.inner.dir.join("tmp");
        let mut off = self.inner.enabled.subscribe();
        let result = tokio::select! {
            r = tokio::time::timeout(DOWNLOAD_TIMEOUT, self.inner.downloader.download(&id, &tmp)) => Some(r),
            _ = off.wait_for(|on| !*on) => None,
        };
        self.state().downloading = None;
        let outcome = match result {
            None => {
                let _ = tokio::fs::remove_dir_all(&tmp).await;
                return;
            }
            Some(Err(_)) => Err(anyhow::anyhow!(
                "no finish after {} s",
                DOWNLOAD_TIMEOUT.as_secs()
            )),
            Some(Ok(Err(e))) => Err(e),
            Some(Ok(Ok(file))) => self.keep(&id, &file).await,
        };
        match outcome {
            Ok(bytes) => {
                self.state().failed.remove(&id);
                tracing::info!("offline: saved {id} ({bytes} bytes)");
            }
            Err(e) => {
                tracing::warn!("offline: {id} failed: {e:#}");
                self.state().failed.insert(id, Instant::now());
            }
        }
    }

    /// Moves a finished download out of `tmp/` and into the index; deletes it
    /// instead when the switch went off meanwhile. Returns its size.
    async fn keep(&self, id: &str, file: &Path) -> anyhow::Result<u64> {
        let name = file
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("the download has no file name"))?;
        if !self.enabled() {
            let _ = tokio::fs::remove_file(file).await;
            return Ok(0);
        }
        let dest = self.inner.dir.join(name);
        tokio::fs::rename(file, &dest).await?;
        let bytes = tokio::fs::metadata(&dest).await?.len();
        self.inner.index.insert(id, dest, bytes);
        Ok(bytes)
    }

    fn cap_reached(&self) -> bool {
        self.inner
            .max_bytes
            .is_some_and(|max| self.inner.index.bytes() >= max)
    }

    fn in_backoff(&self, state: &State, id: &str) -> bool {
        state
            .failed
            .get(id)
            .is_some_and(|at| at.elapsed() < self.inner.timing.retry_after)
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.inner.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn remove_file_logged(path: &Path) {
    if let Err(e) = std::fs::remove_file(path)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!("offline: deleting {}: {e}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A temp dir for one test, removed when dropped.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            use std::sync::atomic::{AtomicU32, Ordering};
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!(
                "pinson-offline-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::SeqCst)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn song(id: &str) -> Song {
        Song {
            video_id: id.into(),
            title: format!("title {id}"),
            artists: "artist".into(),
            album: None,
            duration_secs: None,
            artwork_url: None,
        }
    }

    /// Writes `<id>.webm` of `size` bytes, as a download would.
    struct FakeDownloader {
        size: usize,
        fail: Vec<String>,
        calls: Mutex<Vec<String>>,
    }

    impl FakeDownloader {
        fn new(size: usize, fail: &[&str]) -> Arc<Self> {
            Arc::new(Self {
                size,
                fail: fail.iter().map(|s| (*s).to_owned()).collect(),
                calls: Mutex::default(),
            })
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl Downloader for FakeDownloader {
        async fn download(&self, video_id: &str, tmp_dir: &Path) -> anyhow::Result<PathBuf> {
            self.calls.lock().unwrap().push(video_id.to_owned());
            if self.fail.iter().any(|f| f == video_id) {
                anyhow::bail!("Video unavailable");
            }
            std::fs::create_dir_all(tmp_dir)?;
            let path = tmp_dir.join(format!("{video_id}.webm"));
            std::fs::write(&path, vec![1u8; self.size])?;
            Ok(path)
        }
    }

    fn service(
        dir: &TempDir,
        favorites: &Favorites,
        max_bytes: Option<u64>,
        downloader: Arc<dyn Downloader>,
        retry_after: Duration,
    ) -> Offline {
        Offline::build(
            dir.0.clone(),
            OfflineIndex::scan(&dir.0),
            Db::in_memory(),
            favorites.clone(),
            max_bytes,
            downloader,
            true,
            Timing {
                pause: Duration::ZERO,
                retry_after,
                debounce: Duration::ZERO,
                resync: Duration::from_secs(3600),
            },
        )
    }

    fn favorites_of(ids: &[&str]) -> Favorites {
        let favorites = Favorites::in_memory();
        // `add` puts a song first, so add the oldest first.
        for id in ids.iter().rev() {
            favorites.add(song(id)).unwrap();
        }
        favorites
    }

    #[test]
    fn scan_keeps_only_video_id_files() {
        let dir = TempDir::new();
        std::fs::write(dir.0.join("abc_-1.webm"), b"12345").unwrap();
        std::fs::write(dir.0.join("def.m4a"), b"12").unwrap();
        std::fs::write(dir.0.join("noext"), b"1").unwrap();
        std::fs::write(dir.0.join("a b.webm"), b"1").unwrap();
        std::fs::write(dir.0.join("abc.webm.part"), b"1").unwrap();
        std::fs::write(dir.0.join(".webm"), b"1").unwrap();
        std::fs::create_dir_all(dir.0.join("tmp")).unwrap();
        std::fs::write(dir.0.join("tmp").join("zzz.webm"), b"1").unwrap();
        let index = OfflineIndex::scan(&dir.0);
        assert_eq!(index.path("abc_-1"), Some(dir.0.join("abc_-1.webm")));
        assert_eq!(index.path("def"), Some(dir.0.join("def.m4a")));
        assert_eq!(index.path("zzz"), None);
        assert_eq!(index.path("noext"), None);
        assert_eq!(index.bytes(), 7);
        assert!(
            OfflineIndex::scan(&dir.0.join("missing"))
                .path("abc")
                .is_none()
        );
    }

    #[test]
    fn setting_is_off_by_default_and_round_trips() {
        let db = Db::in_memory();
        assert!(!offline_enabled(&db).unwrap());
        set_offline_enabled(&db, true).unwrap();
        assert!(offline_enabled(&db).unwrap());
        set_offline_enabled(&db, false).unwrap();
        assert!(!offline_enabled(&db).unwrap());
    }

    #[tokio::test]
    async fn sync_downloads_every_favorite_newest_first() {
        let dir = TempDir::new();
        let favorites = favorites_of(&["new", "mid", "old"]);
        let downloader = FakeDownloader::new(10, &[]);
        let offline = service(&dir, &favorites, None, downloader.clone(), RETRY_AFTER);
        offline.sync().await;
        assert_eq!(downloader.calls(), ["new", "mid", "old"]);
        let status = offline.status();
        assert_eq!((status.saved, status.total, status.bytes), (3, 3, 30));
        assert!(status.enabled && !status.full && status.downloading.is_none());
        assert_eq!(status.failed, 0);
        assert!(dir.0.join("mid.webm").is_file());
    }

    #[tokio::test]
    async fn a_failed_song_waits_and_does_not_block_the_others() {
        let dir = TempDir::new();
        let favorites = favorites_of(&["a", "bad", "c"]);
        let downloader = FakeDownloader::new(5, &["bad"]);
        let offline = service(&dir, &favorites, None, downloader.clone(), RETRY_AFTER);
        offline.sync().await;
        let status = offline.status();
        assert_eq!((status.saved, status.total, status.failed), (2, 3, 1));
        offline.sync().await;
        assert_eq!(downloader.calls(), ["a", "bad", "c"], "no retry yet");
    }

    #[tokio::test]
    async fn a_failed_song_is_retried_after_the_backoff() {
        let dir = TempDir::new();
        let favorites = favorites_of(&["bad"]);
        let downloader = FakeDownloader::new(5, &["bad"]);
        let offline = service(&dir, &favorites, None, downloader.clone(), Duration::ZERO);
        offline.sync().await;
        offline.sync().await;
        assert_eq!(downloader.calls(), ["bad", "bad"]);
        assert_eq!(offline.status().failed, 0, "backoff is over");
    }

    #[tokio::test]
    async fn sync_deletes_files_of_songs_no_longer_favorites() {
        let dir = TempDir::new();
        let favorites = favorites_of(&["keep", "drop"]);
        let offline = service(
            &dir,
            &favorites,
            None,
            FakeDownloader::new(4, &[]),
            RETRY_AFTER,
        );
        offline.sync().await;
        assert!(dir.0.join("drop.webm").is_file());
        favorites.remove("drop").unwrap();
        offline.sync().await;
        assert!(!dir.0.join("drop.webm").exists());
        assert!(dir.0.join("keep.webm").is_file());
        let status = offline.status();
        assert_eq!((status.saved, status.total, status.bytes), (1, 1, 4));
    }

    #[tokio::test]
    async fn sync_stops_at_the_cap_and_reports_full() {
        let dir = TempDir::new();
        let favorites = favorites_of(&["a", "b", "c"]);
        let downloader = FakeDownloader::new(10, &[]);
        let offline = service(&dir, &favorites, Some(20), downloader.clone(), RETRY_AFTER);
        offline.sync().await;
        assert_eq!(downloader.calls(), ["a", "b"]);
        let status = offline.status();
        assert!(status.full);
        assert_eq!((status.saved, status.total), (2, 3));
        assert_eq!(status.limit_bytes, Some(20));
    }

    #[tokio::test]
    async fn at_the_cap_with_everything_saved_is_not_full() {
        let dir = TempDir::new();
        let favorites = favorites_of(&["a", "b"]);
        let offline = service(
            &dir,
            &favorites,
            Some(20),
            FakeDownloader::new(10, &[]),
            RETRY_AFTER,
        );
        offline.sync().await;
        assert!(!offline.status().full);
    }

    #[tokio::test]
    async fn off_does_nothing_and_turning_off_deletes_everything() {
        let dir = TempDir::new();
        let favorites = favorites_of(&["a", "b"]);
        let downloader = FakeDownloader::new(3, &[]);
        let offline = service(&dir, &favorites, None, downloader.clone(), RETRY_AFTER);
        offline.sync().await;
        assert_eq!(offline.status().saved, 2);
        offline.set_enabled(false).await.unwrap();
        assert!(!dir.0.join("a.webm").exists() && !dir.0.join("b.webm").exists());
        let status = offline.status();
        assert_eq!((status.enabled, status.saved, status.bytes), (false, 0, 0));
        assert!(!offline_enabled(&offline.inner.db).unwrap());
        offline.sync().await;
        assert_eq!(downloader.calls().len(), 2, "off: no new downloads");
        offline.set_enabled(true).await.unwrap();
        assert!(offline.status().enabled);
        assert!(offline_enabled(&offline.inner.db).unwrap());
    }

    #[tokio::test]
    async fn status_names_the_song_being_downloaded() {
        let dir = TempDir::new();
        let favorites = favorites_of(&["a"]);
        let offline = service(
            &dir,
            &favorites,
            None,
            FakeDownloader::new(1, &[]),
            RETRY_AFTER,
        );
        offline.state().downloading = Some(song("a"));
        let status = offline.status();
        assert_eq!(
            status.downloading,
            Some(Downloading {
                video_id: "a".into(),
                title: "title a".into(),
                artist: "artist".into()
            })
        );
        let json = serde_json::to_value(&status).unwrap();
        assert_eq!(json["downloading"]["videoId"], "a");
        assert_eq!(json["limitBytes"], serde_json::Value::Null);
    }
}
