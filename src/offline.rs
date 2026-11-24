//! Favorites kept on the server's disk, so they play without YouTube.
//!
//! [`OfflineIndex`] knows which songs have a saved file; the source plays
//! from it. [`Offline`] is the background service that downloads every
//! favorite one at a time and deletes the files of songs that are no longer
//! favorites.

mod mp3;

use std::collections::{HashMap, HashSet};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use serde::Serialize;
use tokio::sync::{Mutex as AsyncMutex, watch};
use tokio::time::Instant;

use crate::db::Db;
use crate::favorites::Favorites;
use crate::song::Song;
pub use mp3::Mp3Downloader;

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

#[derive(Clone)]
struct CachedFile {
    path: PathBuf,
    bytes: u64,
    mp3: bool,
}

#[derive(Clone)]
struct CacheCatalog {
    db: Db,
    root: PathBuf,
}

const SET_CACHED_PATH: &str = "
UPDATE songs SET local_path = ?1, local_is_cache = (?1 IS NOT NULL)
WHERE video_id = ?2 AND (local_is_cache = 1 OR (local_path IS NULL AND ?1 IS NOT NULL))
";

/// The saved files by video id, with MP3 copies preferred to legacy files.
#[derive(Clone, Default)]
pub struct OfflineIndex {
    files: Arc<RwLock<HashMap<String, Vec<CachedFile>>>>,
    catalog: Option<CacheCatalog>,
}

impl OfflineIndex {
    /// Opens the disk index and reconciles the catalog with the files found.
    pub fn open(dir: &Path, db: Db) -> Self {
        let mut index = Self::scan(dir);
        index.catalog = Some(CacheCatalog {
            db,
            root: dir.to_path_buf(),
        });
        index.sync_catalog(&index.read());
        index
    }

    /// Reads tagged MP3s and legacy `<video id>.<ext>` files at the root.
    /// Incomplete downloads in `tmp/` are ignored.
    pub fn scan(dir: &Path) -> Self {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Self::default();
        };
        let mut files: HashMap<String, Vec<CachedFile>> = HashMap::new();
        for entry in entries.flatten() {
            if !entry.file_type().is_ok_and(|kind| kind.is_file()) {
                continue;
            }
            let path = entry.path();
            let Some(ext) = path.extension().filter(|ext| !ext.is_empty()) else {
                continue;
            };
            let Ok(meta) = entry.metadata() else { continue };
            if ext.eq_ignore_ascii_case("mp3")
                && let Some(id) = mp3::video_id(&path)
            {
                files.entry(id).or_default().insert(
                    0,
                    CachedFile {
                        path,
                        bytes: meta.len(),
                        mp3: true,
                    },
                );
            } else if let Some(id) = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .filter(|id| valid_video_id(id))
            {
                files.entry(id.to_owned()).or_default().push(CachedFile {
                    path,
                    bytes: meta.len(),
                    mp3: false,
                });
            }
        }
        Self {
            files: Arc::new(RwLock::new(files)),
            catalog: None,
        }
    }

    /// The saved file of `video_id`, if there is one.
    pub fn path(&self, video_id: &str) -> Option<PathBuf> {
        self.preferred(video_id).map(|file| file.path)
    }

    fn preferred(&self, video_id: &str) -> Option<CachedFile> {
        self.read().get(video_id)?.first().cloned()
    }

    #[cfg(test)]
    pub(crate) fn insert(&self, video_id: &str, path: PathBuf, bytes: u64) {
        self.write().insert(
            video_id.to_owned(),
            vec![CachedFile {
                path,
                bytes,
                mp3: false,
            }],
        );
    }

    /// Drops a failed copy only if it still belongs to this song. A conversion
    /// may have published a replacement while playback was opening the file.
    pub(crate) fn discard(&self, video_id: &str, path: &Path) {
        let mut index = self.write();
        if let Some(files) = index.get_mut(video_id) {
            let before = files.len();
            files.retain(|file| file.path != path || !delete_cached_file(file));
            if files.len() != before {
                self.sync_catalog_path(video_id, files.first());
            }
            if files.is_empty() {
                index.remove(video_id);
            }
        }
    }

    fn prune(&self, favorites: &HashSet<&str>) {
        let mut index = self.write();
        index.retain(|id, files| {
            let keep = usize::from(favorites.contains(id.as_str()));
            let mut position = 0;
            files.retain(|file| {
                let retain = position < keep || !delete_cached_file(file);
                position += 1;
                retain
            });
            !files.is_empty()
        });
        self.sync_catalog(&index);
    }

    fn sync_catalog(&self, files: &HashMap<String, Vec<CachedFile>>) {
        let Some(catalog) = &self.catalog else { return };
        // The disk index remains usable if a catalog write fails. A later scan
        // reconciles the database after an interrupted filesystem change.
        let result = catalog.db.with(|conn| {
            let tx = conn.transaction()?;
            tx.execute(
                "UPDATE songs SET local_path = NULL, local_is_cache = 0 WHERE local_is_cache = 1",
                [],
            )?;
            {
                let mut update = tx.prepare_cached(SET_CACHED_PATH)?;
                for (id, copies) in files {
                    update.execute((catalog.relative_path(copies.first()), id))?;
                }
            }
            tx.commit()
        });
        if let Err(e) = result {
            tracing::warn!("syncing offline catalog: {e:#}");
        }
    }

    fn sync_catalog_path(&self, video_id: &str, file: Option<&CachedFile>) {
        let Some(catalog) = &self.catalog else { return };
        if let Err(e) = catalog
            .db
            .with(|conn| conn.execute(SET_CACHED_PATH, (catalog.relative_path(file), video_id)))
        {
            tracing::warn!("updating offline catalog for {video_id}: {e:#}");
        }
    }

    /// Total size of the saved files.
    pub fn bytes(&self) -> u64 {
        self.read().values().flatten().map(|file| file.bytes).sum()
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, HashMap<String, Vec<CachedFile>>> {
        self.files.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, HashMap<String, Vec<CachedFile>>> {
        self.files.write().unwrap_or_else(|e| e.into_inner())
    }
}

impl CacheCatalog {
    fn relative_path<'a>(&self, file: Option<&'a CachedFile>) -> Option<&'a str> {
        file?.path.strip_prefix(&self.root).ok()?.to_str()
    }
}

/// Prepares a tagged MP3 in a staging folder.
#[async_trait]
pub trait Downloader: Send + Sync + 'static {
    async fn download(
        &self,
        song: &Song,
        legacy: Option<&Path>,
        tmp_dir: &Path,
    ) -> anyhow::Result<PathBuf>;
}

/// A downloader that always fails, for services that must never reach YouTube.
struct NoDownloads;

#[async_trait]
impl Downloader for NoDownloads {
    async fn download(
        &self,
        _song: &Song,
        _legacy: Option<&Path>,
        _tmp_dir: &Path,
    ) -> anyhow::Result<PathBuf> {
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
    changes: AsyncMutex<()>,
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
                changes: AsyncMutex::new(()),
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
        let _changes = self.inner.changes.lock().await;
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
        let index = self.inner.index.clone();
        let tmp = self.inner.dir.join("tmp");
        let removed = tokio::task::spawn_blocking(move || {
            index.prune(&HashSet::new());
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

    /// Removes obsolete copies, then downloads or converts favorites newest
    /// first. At the storage cap, only replacements can still be attempted.
    async fn sync(&self) {
        let favorites = self.inner.favorites.list();
        {
            let _changes = self.inner.changes.lock().await;
            if !self.enabled() {
                return;
            }
            self.inner
                .index
                .prune(&favorites.iter().map(|s| s.video_id.as_str()).collect());
        }
        for song in favorites {
            if !self.enabled() {
                break;
            }
            let saved = self.inner.index.preferred(&song.video_id);
            if saved.as_ref().is_some_and(|file| file.mp3)
                || (saved.is_none() && self.cap_reached())
            {
                continue;
            }
            if self.in_backoff(&self.state(), &song.video_id) {
                continue;
            }
            self.download_one(song, saved.map(|file| file.path)).await;
            tokio::time::sleep(self.inner.timing.pause).await;
        }
    }

    /// Downloads one song and records the result.
    async fn download_one(&self, song: Song, legacy: Option<PathBuf>) {
        let id = &song.video_id;
        if !valid_video_id(id) {
            return;
        }
        self.state().downloading = Some(song.clone());
        let tmp = self.inner.dir.join("tmp").join(id);
        let mut off = self.inner.enabled.subscribe();
        let result = tokio::select! {
            biased;
            _ = off.wait_for(|on| !*on) => None,
            r = tokio::time::timeout(DOWNLOAD_TIMEOUT, self.inner.downloader.download(&song, legacy.as_deref(), &tmp)) => Some(r),
        };
        let outcome = match result {
            None => Ok(None),
            Some(Err(_)) => Err(anyhow::anyhow!(
                "no finish after {} s",
                DOWNLOAD_TIMEOUT.as_secs()
            )),
            Some(Ok(Err(e))) => Err(e),
            Some(Ok(Ok(file))) => self.keep(&song, &file, legacy.is_some()).await,
        };
        let _ = tokio::fs::remove_dir_all(&tmp).await;
        self.state().downloading = None;
        match outcome {
            Ok(Some(bytes)) => {
                self.state().failed.remove(id);
                tracing::info!("offline: saved {id} ({bytes} bytes)");
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!("offline: {id} failed: {e:#}");
                self.state().failed.insert(id.clone(), Instant::now());
            }
        }
    }

    /// Publishes a complete MP3 before removing any previous copy.
    async fn keep(
        &self,
        song: &Song,
        file: &Path,
        converting: bool,
    ) -> anyhow::Result<Option<u64>> {
        let _changes = self.inner.changes.lock().await;
        if !self.enabled() {
            return Ok(None);
        }
        let bytes = std::fs::metadata(file)?.len();
        let mut index = self.inner.index.write();
        if converting && let Some(limit) = self.inner.max_bytes {
            let old_bytes = index
                .get(&song.video_id)
                .and_then(|files| files.first())
                .map_or(0, |old| old.bytes);
            if bytes > old_bytes {
                let used: u64 = index.values().flatten().map(|file| file.bytes).sum();
                anyhow::ensure!(
                    used.saturating_add(bytes - old_bytes) <= limit,
                    "converting this track would exceed the offline storage limit"
                );
            }
        }
        let path = mp3::publish(&self.inner.dir, song, file)?;
        let files = index.entry(song.video_id.clone()).or_default();
        files.retain(|old| !delete_cached_file(old));
        files.insert(
            0,
            CachedFile {
                path,
                bytes,
                mp3: true,
            },
        );
        self.inner
            .index
            .sync_catalog_path(&song.video_id, files.first());
        Ok(Some(bytes))
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

fn delete_cached_file(file: &CachedFile) -> bool {
    if let Err(e) = std::fs::remove_file(&file.path)
        && e.kind() != ErrorKind::NotFound
    {
        tracing::warn!("offline: deleting {}: {e}", file.path.display());
        return false;
    }
    true
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

    /// Writes a staged file of `size` bytes, without encoding audio.
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
        async fn download(
            &self,
            song: &Song,
            _legacy: Option<&Path>,
            tmp_dir: &Path,
        ) -> anyhow::Result<PathBuf> {
            let video_id = &song.video_id;
            self.calls.lock().unwrap().push(video_id.to_owned());
            if self.fail.iter().any(|f| f == video_id) {
                anyhow::bail!("Video unavailable");
            }
            std::fs::create_dir_all(tmp_dir)?;
            let path = tmp_dir.join("audio.mp3");
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

    fn track(dir: &TempDir, id: &str) -> PathBuf {
        dir.0.join(format!("artist - title {id}.mp3"))
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
        assert!(track(&dir, "mid").is_file());
    }

    #[tokio::test]
    async fn legacy_audio_becomes_tagged_mp3s_that_are_found_after_restart() {
        use axum::{Json, Router, http::header, routing::get};
        use id3::{Tag, TagLike};

        use crate::artwork::ArtworkProxy;
        use crate::lyrics::{LrcLib, LyricsService};
        use crate::search::YtmSearcher;
        use crate::source::{PcmSource, YtDlpFfmpegSource};

        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut jpeg)
            .encode_image(&image::RgbImage::new(8, 8))
            .unwrap();
        let app = Router::new()
            .route(
                "/cover",
                get(move || async move { ([(header::CONTENT_TYPE, "image/jpeg")], jpeg) }),
            )
            .route(
                "/api/get",
                get(|| async {
                    Json(serde_json::json!({
                        "duration": 2,
                        "syncedLyrics": "[00:01.25]Été"
                    }))
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let dir = TempDir::new();
        let first = dir.0.join("first.wav");
        let second = dir.0.join("second.wav");
        let fixture = include_bytes!("../tests/fixtures/sine2s.wav");
        std::fs::write(&first, fixture).unwrap();
        std::fs::write(&second, fixture).unwrap();
        let db = Db::in_memory();
        set_offline_enabled(&db, true).unwrap();
        let favorites = Favorites::open(db.clone()).unwrap();
        for (id, album) in [("second", "Winter"), ("first", "Été")] {
            favorites
                .add(Song {
                    video_id: id.into(),
                    title: "Titre?".into(),
                    artists: "Artiste".into(),
                    album: Some(album.into()),
                    duration_secs: Some(2),
                    artwork_url: Some(format!("{url}/cover")),
                })
                .unwrap();
        }
        let source = || {
            YtDlpFfmpegSource::new(
                "/nonexistent/yt-dlp".into(),
                "ffmpeg".into(),
                None,
                None,
                None,
                None,
                None,
            )
        };
        let downloader = Mp3Downloader::new(
            Arc::new(source()),
            Arc::new(ArtworkProxy::new(vec!["127.0.0.1".into()])),
            Arc::new(LyricsService::new(
                Arc::new(LrcLib::new(url)),
                Arc::new(YtmSearcher::lazy(None)),
            )),
        );
        let local_path = |id: &str| -> Option<String> {
            db.with(|c| {
                c.query_row(
                    "SELECT local_path FROM songs WHERE video_id = ?1",
                    [id],
                    |r| r.get(0),
                )
            })
            .unwrap()
        };
        let index = OfflineIndex::open(&dir.0, db.clone());
        assert_eq!(local_path("first").as_deref(), Some("first.wav"));
        let offline = Offline::build(
            dir.0.clone(),
            index,
            db.clone(),
            favorites,
            Some(2 * fixture.len() as u64),
            Arc::new(downloader),
            true,
            Timing {
                pause: Duration::ZERO,
                retry_after: RETRY_AFTER,
                debounce: Duration::ZERO,
                resync: RESYNC_EVERY,
            },
        );
        offline.sync().await;
        assert!(!first.exists() && !second.exists());
        assert_eq!(local_path("first").as_deref(), Some("Artiste - Titre_.mp3"));

        let restarted = OfflineIndex::open(&dir.0, db.clone());
        let path = dir.0.join("Artiste - Titre_.mp3");
        assert_eq!(restarted.path("first"), Some(path.clone()));
        assert_eq!(
            restarted.path("second"),
            Some(dir.0.join("Artiste - Titre_ [second].mp3"))
        );
        assert_eq!(restarted.bytes(), offline.status().bytes);
        let tag = Tag::read_from_path(&path).unwrap();
        assert_eq!(tag.artist(), Some("Artiste"));
        assert_eq!(tag.title(), Some("Titre?"));
        assert_eq!(tag.album(), Some("Été"));
        let cover = tag.pictures().next().unwrap();
        assert_eq!(cover.mime_type, "image/jpeg");
        assert_eq!(image::load_from_memory(&cover.data).unwrap().width(), 600);
        assert_eq!(tag.lyrics().next().unwrap().text, "Été");
        assert_eq!(
            tag.synchronised_lyrics().next().unwrap().content,
            vec![(1250, "Été\n".into())]
        );
        let source = source().with_offline(restarted);
        let mut reader = source.open("first", Duration::ZERO).await.unwrap();
        assert!(reader.read(&mut [0; 4096]).await.unwrap() > 0);
        drop(reader);
        offline.set_enabled(false).await.unwrap();
        assert_eq!(local_path("first"), None);
        assert_eq!(local_path("second"), None);
    }

    #[tokio::test]
    async fn migration_keeps_the_legacy_copy_if_the_mp3_would_exceed_the_limit() {
        let dir = TempDir::new();
        let legacy = dir.0.join("a.webm");
        std::fs::write(&legacy, b"1234").unwrap();
        let offline = service(
            &dir,
            &favorites_of(&["a"]),
            Some(5),
            FakeDownloader::new(6, &[]),
            RETRY_AFTER,
        );
        offline.sync().await;
        assert_eq!(offline.inner.index.path("a"), Some(legacy.clone()));
        assert_eq!(std::fs::read(legacy).unwrap(), b"1234");
        assert!(!track(&dir, "a").exists());
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
        assert!(track(&dir, "drop").is_file());
        favorites.remove("drop").unwrap();
        offline.sync().await;
        assert!(!track(&dir, "drop").exists());
        assert!(track(&dir, "keep").is_file());
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
        assert!(!track(&dir, "a").exists());
        assert!(!track(&dir, "b").exists());
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
