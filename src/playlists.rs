//! My playlists: named, ordered song lists saved to SQLite.
//!
//! Same shape as `favorites`: cheap to clone (one `Arc`), every change is
//! written to the database before it becomes visible, and subscribers get a summary
//! list through a `watch` channel. Last write wins; there is no versioning.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use anyhow::Context;
use rand::Rng;
use rand::distr::Alphanumeric;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::db::{Db, SONG_COLUMNS, read_song, song_params};
use crate::song::Song;

/// Longest playlist name, in characters.
pub const MAX_NAME_CHARS: usize = 100;
const ID_LEN: usize = 10;
const SUMMARY_ARTWORKS: usize = 4;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Playlist {
    pub id: String,
    pub name: String,
    pub songs: Vec<Song>,
    /// RFC 3339.
    pub created_at: String,
    /// RFC 3339.
    pub updated_at: String,
}

/// A playlist without its songs, for lists and the WebSocket.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaylistSummary {
    pub id: String,
    pub name: String,
    pub song_count: usize,
    /// Up to four distinct artwork URLs taken from the first songs that have one.
    pub artwork_urls: Vec<String>,
    pub updated_at: String,
}

/// The first [`SUMMARY_ARTWORKS`] different artwork URLs, in song order (an
/// album saved as a playlist would otherwise repeat one cover four times).
fn distinct_artworks(songs: &[Song]) -> Vec<String> {
    let mut urls: Vec<String> = Vec::new();
    for url in songs.iter().filter_map(|s| s.artwork_url.as_ref()) {
        if urls.len() == SUMMARY_ARTWORKS {
            break;
        }
        if !urls.contains(url) {
            urls.push(url.clone());
        }
    }
    urls
}

impl From<&Playlist> for PlaylistSummary {
    fn from(p: &Playlist) -> Self {
        Self {
            id: p.id.clone(),
            name: p.name.clone(),
            song_count: p.songs.len(),
            artwork_urls: distinct_artworks(&p.songs),
            updated_at: p.updated_at.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct AddSongsOutcome {
    pub added: usize,
    pub skipped: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum PlaylistError {
    #[error("no playlist {0}")]
    NotFound(String),
    #[error("{0} is not in this playlist")]
    SongNotFound(String),
    #[error("{0}")]
    BadName(&'static str),
    #[error("{0}")]
    BadOrder(&'static str),
    #[error("videoIds must list exactly the songs in the playlist")]
    OrderMismatch,
    #[error("{0:#}")]
    Io(anyhow::Error),
}

impl From<anyhow::Error> for PlaylistError {
    fn from(e: anyhow::Error) -> Self {
        Self::Io(e)
    }
}

/// Trims `raw` and checks it: 1 to 100 characters, no control characters.
pub fn validate_name(raw: &str) -> Result<String, PlaylistError> {
    let name = raw.trim();
    if name.is_empty() {
        return Err(PlaylistError::BadName("name must not be blank"));
    }
    if name.chars().count() > MAX_NAME_CHARS {
        return Err(PlaylistError::BadName(
            "name must be at most 100 characters",
        ));
    }
    if name.chars().any(char::is_control) {
        return Err(PlaylistError::BadName(
            "name must not contain control characters",
        ));
    }
    Ok(name.to_owned())
}

struct Inner {
    db: Db,
    playlists: Mutex<Vec<Playlist>>,
    changes: watch::Sender<Vec<PlaylistSummary>>,
}

#[derive(Clone)]
pub struct Playlists {
    inner: Arc<Inner>,
}

impl Playlists {
    /// Loads the playlists stored in `db`, in creation order.
    pub fn open(db: Db) -> anyhow::Result<Self> {
        let playlists = db.with(|c| {
            let mut stmt =
                c.prepare("SELECT id, name, created_at, updated_at FROM playlists ORDER BY rowid")?;
            let mut all: Vec<Playlist> = stmt
                .query_map([], |r| {
                    Ok(Playlist {
                        id: r.get(0)?,
                        name: r.get(1)?,
                        songs: Vec::new(),
                        created_at: r.get(2)?,
                        updated_at: r.get(3)?,
                    })
                })?
                .collect::<rusqlite::Result<_>>()?;
            let mut stmt = c.prepare(&format!(
                "SELECT {SONG_COLUMNS} FROM playlist_songs
                 WHERE playlist_id = ?1 ORDER BY position"
            ))?;
            for p in &mut all {
                p.songs = stmt
                    .query_map([&p.id], |r| read_song(r, 0))?
                    .collect::<rusqlite::Result<_>>()?;
            }
            Ok(all)
        })?;
        let (changes, _) = watch::channel(summaries(&playlists));
        Ok(Self {
            inner: Arc::new(Inner {
                db,
                playlists: Mutex::new(playlists),
                changes,
            }),
        })
    }

    /// A store backed by a private in-memory database.
    pub fn in_memory() -> Self {
        Self::open(Db::in_memory()).expect("open in-memory playlists")
    }

    /// Summaries, most recently updated first.
    pub fn list(&self) -> Vec<PlaylistSummary> {
        summaries(&self.lock())
    }

    pub fn get(&self, id: &str) -> Option<Playlist> {
        self.lock().iter().find(|p| p.id == id).cloned()
    }

    /// Creates a playlist; repeated songs are kept once, in first-seen order.
    pub fn create(&self, name: &str, songs: Vec<Song>) -> Result<Playlist, PlaylistError> {
        let name = validate_name(name)?;
        let mut guard = self.lock();
        let mut all = guard.clone();
        let now = now();
        let mut seen = HashSet::new();
        let playlist = Playlist {
            id: new_id(&all),
            name,
            songs: songs
                .into_iter()
                .filter(|s| seen.insert(s.video_id.clone()))
                .collect(),
            created_at: now.clone(),
            updated_at: now,
        };
        all.push(playlist.clone());
        self.commit(&mut guard, all)?;
        Ok(playlist)
    }

    pub fn rename(&self, id: &str, name: &str) -> Result<Playlist, PlaylistError> {
        let name = validate_name(name)?;
        self.update(id, |p| {
            p.name = name;
            Ok(())
        })
        .map(|(p, ())| p)
    }

    pub fn delete(&self, id: &str) -> Result<(), PlaylistError> {
        let mut guard = self.lock();
        let Some(index) = guard.iter().position(|p| p.id == id) else {
            return Err(PlaylistError::NotFound(id.to_owned()));
        };
        let mut all = guard.clone();
        all.remove(index);
        self.commit(&mut guard, all)?;
        Ok(())
    }

    /// Appends the songs that aren't in the playlist yet, in order.
    pub fn add_songs(&self, id: &str, songs: Vec<Song>) -> Result<AddSongsOutcome, PlaylistError> {
        self.update(id, |p| {
            let mut present: HashSet<String> = p.songs.iter().map(|s| s.video_id.clone()).collect();
            let total = songs.len();
            let before = p.songs.len();
            p.songs.extend(
                songs
                    .into_iter()
                    .filter(|s| present.insert(s.video_id.clone())),
            );
            let added = p.songs.len() - before;
            Ok(AddSongsOutcome {
                added,
                skipped: total - added,
            })
        })
        .map(|(_, outcome)| outcome)
    }

    pub fn remove_song(&self, id: &str, video_id: &str) -> Result<(), PlaylistError> {
        self.update(id, |p| {
            let index = p
                .songs
                .iter()
                .position(|s| s.video_id == video_id)
                .ok_or_else(|| PlaylistError::SongNotFound(video_id.to_owned()))?;
            p.songs.remove(index);
            Ok(())
        })
        .map(|_| ())
    }

    /// Puts the songs in the given order. `video_ids` must name every song
    /// exactly once.
    pub fn reorder(&self, id: &str, video_ids: Vec<String>) -> Result<Playlist, PlaylistError> {
        let mut seen = HashSet::new();
        if !video_ids.iter().all(|v| seen.insert(v.as_str())) {
            // Checked before the lookup so a malformed request is a 400
            // whatever the playlist holds.
            return Err(PlaylistError::BadOrder("videoIds must not repeat"));
        }
        self.update(id, |p| {
            let current: HashSet<&str> = p.songs.iter().map(|s| s.video_id.as_str()).collect();
            if current.len() != video_ids.len()
                || !video_ids.iter().all(|v| current.contains(v.as_str()))
            {
                return Err(PlaylistError::OrderMismatch);
            }
            let mut songs = std::mem::take(&mut p.songs);
            songs.sort_by_key(|s| video_ids.iter().position(|v| *v == s.video_id));
            p.songs = songs;
            Ok(())
        })
        .map(|(p, ())| p)
    }

    /// The summaries, updated after every change.
    pub fn subscribe(&self) -> watch::Receiver<Vec<PlaylistSummary>> {
        self.inner.changes.subscribe()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Playlist>> {
        // A poisoned lock only means another thread panicked mid-read; the
        // list is always replaced whole, so it is still consistent.
        self.inner
            .playlists
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// Runs `f` on a copy of one playlist, bumps its `updated_at`, saves, and
    /// returns the updated playlist with `f`'s result. Nothing changes if `f`
    /// or the save fails.
    fn update<T>(
        &self,
        id: &str,
        f: impl FnOnce(&mut Playlist) -> Result<T, PlaylistError>,
    ) -> Result<(Playlist, T), PlaylistError> {
        let mut guard = self.lock();
        let Some(index) = guard.iter().position(|p| p.id == id) else {
            return Err(PlaylistError::NotFound(id.to_owned()));
        };
        let mut all = guard.clone();
        let out = f(&mut all[index])?;
        all[index].updated_at = now();
        let updated = all[index].clone();
        self.commit(&mut guard, all)?;
        Ok((updated, out))
    }

    /// Saves `all`, then makes it current and notifies subscribers.
    fn commit(&self, guard: &mut Vec<Playlist>, all: Vec<Playlist>) -> Result<(), PlaylistError> {
        self.inner
            .db
            .with(|c| {
                let tx = c.transaction()?;
                save_playlists(&tx, &all)?;
                tx.commit()
            })
            .context("saving playlists")
            .map_err(PlaylistError::from)?;
        let summary = summaries(&all);
        *guard = all;
        self.inner.changes.send_replace(summary);
        Ok(())
    }
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// Summaries, most recently updated first (RFC 3339 UTC sorts as text).
fn summaries(all: &[Playlist]) -> Vec<PlaylistSummary> {
    let mut out: Vec<PlaylistSummary> = all.iter().map(PlaylistSummary::from).collect();
    out.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    out
}

/// A random id that no playlist uses yet.
fn new_id(existing: &[Playlist]) -> String {
    loop {
        let id: String = rand::rng()
            .sample_iter(Alphanumeric)
            .take(ID_LEN)
            .map(char::from)
            .collect();
        if existing.iter().all(|p| p.id != id) {
            return id;
        }
    }
}

/// Replaces every stored playlist with `all`, songs positioned by index.
pub(crate) fn save_playlists(tx: &rusqlite::Transaction, all: &[Playlist]) -> rusqlite::Result<()> {
    tx.execute("DELETE FROM playlist_songs", [])?;
    tx.execute("DELETE FROM playlists", [])?;
    let mut insert_playlist = tx.prepare(
        "INSERT INTO playlists (id, name, created_at, updated_at) VALUES (?1, ?2, ?3, ?4)",
    )?;
    let mut insert_song = tx.prepare(&format!(
        "INSERT INTO playlist_songs (playlist_id, position, {SONG_COLUMNS})
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)"
    ))?;
    for p in all {
        insert_playlist.execute((&p.id, &p.name, &p.created_at, &p.updated_at))?;
        for (i, song) in p.songs.iter().enumerate() {
            let pos = i as i64;
            let sp = song_params(song);
            insert_song.execute(rusqlite::params![
                p.id, pos, sp[0], sp[1], sp[2], sp[3], sp[4], sp[5]
            ])?;
        }
    }
    Ok(())
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

    fn art_song(id: &str, art: Option<&str>) -> Song {
        Song {
            artwork_url: art.map(str::to_owned),
            ..song(id)
        }
    }

    fn ids(p: &Playlist) -> Vec<&str> {
        p.songs.iter().map(|s| s.video_id.as_str()).collect()
    }

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_owned()).collect()
    }

    /// A fresh directory under the system temp dir, removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "pinson-playlists-{tag}-{}-{:?}",
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
    fn create_trims_name_and_dedups_songs() {
        let p = Playlists::in_memory();
        let created = p
            .create("  Party ", vec![song("a"), song("a"), song("b")])
            .unwrap();
        assert_eq!(created.name, "Party");
        assert_eq!(ids(&created), ["a", "b"]);
        assert!(!created.id.is_empty());
        assert!(created.id.chars().all(|c| c.is_ascii_alphanumeric()));
        assert_eq!(p.get(&created.id), Some(created));
    }

    #[test]
    fn validate_name_rules() {
        assert!(matches!(validate_name(""), Err(PlaylistError::BadName(_))));
        assert!(matches!(
            validate_name("   "),
            Err(PlaylistError::BadName(_))
        ));
        assert!(matches!(
            validate_name(&"é".repeat(101)),
            Err(PlaylistError::BadName(_))
        ));
        assert_eq!(validate_name(&"é".repeat(100)).unwrap(), "é".repeat(100));
        assert!(matches!(
            validate_name("a\u{7}b"),
            Err(PlaylistError::BadName(_))
        ));
        assert_eq!(validate_name(" x ").unwrap(), "x");
    }

    #[test]
    fn add_songs_skips_duplicates_and_appends() {
        let p = Playlists::in_memory();
        let id = p.create("P", vec![song("a")]).unwrap().id;
        let outcome = p
            .add_songs(&id, vec![song("b"), song("a"), song("c")])
            .unwrap();
        assert_eq!(
            outcome,
            AddSongsOutcome {
                added: 2,
                skipped: 1
            }
        );
        assert_eq!(ids(&p.get(&id).unwrap()), ["a", "b", "c"]);
    }

    #[test]
    fn remove_song_unknown_is_song_not_found() {
        let p = Playlists::in_memory();
        let id = p.create("P", vec![song("a"), song("b")]).unwrap().id;
        assert!(matches!(
            p.remove_song(&id, "zzz"),
            Err(PlaylistError::SongNotFound(_))
        ));
        p.remove_song(&id, "a").unwrap();
        assert_eq!(ids(&p.get(&id).unwrap()), ["b"]);
    }

    #[test]
    fn reorder_requires_exact_set() {
        let p = Playlists::in_memory();
        let id = p
            .create("P", vec![song("a"), song("b"), song("c")])
            .unwrap()
            .id;
        let r = p.reorder(&id, strings(&["c", "a", "b"])).unwrap();
        assert_eq!(ids(&r), ["c", "a", "b"]);
        assert_eq!(ids(&p.get(&id).unwrap()), ["c", "a", "b"]);
        assert!(matches!(
            p.reorder(&id, strings(&["a", "b"])),
            Err(PlaylistError::OrderMismatch)
        ));
        assert!(matches!(
            p.reorder(&id, strings(&["a", "b", "d"])),
            Err(PlaylistError::OrderMismatch)
        ));
        assert!(matches!(
            p.reorder(&id, strings(&["a", "a", "b"])),
            Err(PlaylistError::BadOrder(_))
        ));
        assert_eq!(ids(&p.get(&id).unwrap()), ["c", "a", "b"]);
    }

    #[test]
    fn summary_artwork_urls_are_distinct() {
        let p = Playlists::in_memory();
        let songs = vec![
            art_song("a", Some("u1")),
            art_song("b", Some("u1")),
            art_song("c", Some("u2")),
            art_song("d", Some("u1")),
            art_song("e", Some("u3")),
            art_song("f", Some("u4")),
            art_song("g", Some("u5")),
        ];
        p.create("Album", songs).unwrap();
        assert_eq!(p.list()[0].artwork_urls, ["u1", "u2", "u3", "u4"]);
        let one = vec![art_song("a", Some("u1")), art_song("b", Some("u1"))];
        p.create("One cover", one).unwrap();
        assert!(p.list().iter().any(|s| s.artwork_urls == ["u1"]));
    }

    #[test]
    fn summary_has_up_to_four_artwork_urls_and_newest_first() {
        let p = Playlists::in_memory();
        let songs = vec![
            art_song("a", Some("u1")),
            art_song("b", None),
            art_song("c", Some("u2")),
            art_song("d", Some("u3")),
            art_song("e", Some("u4")),
            art_song("f", Some("u5")),
        ];
        let first = p.create("First", songs).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let second = p.create("Second", vec![]).unwrap();

        let list = p.list();
        assert_eq!(list[0].id, second.id);
        assert_eq!(list[1].id, first.id);
        assert_eq!(list[1].song_count, 6);
        assert_eq!(list[1].artwork_urls, ["u1", "u2", "u3", "u4"]);
        assert!(list[0].artwork_urls.is_empty());

        // Touching the older playlist moves it to the front.
        std::thread::sleep(std::time::Duration::from_millis(5));
        p.rename(&first.id, "Renamed").unwrap();
        assert_eq!(p.list()[0].id, first.id);
        assert_eq!(p.list()[0].name, "Renamed");
    }

    #[test]
    fn survives_reopen() {
        let dir = TempDir::new("reopen");
        let path = dir.0.join("pinson.db");
        let p = Playlists::open(Db::open(&path).unwrap()).unwrap();
        let mut b = song("B");
        b.title = "Été 🎵".into();
        b.album = Some("Alb".into());
        b.duration_secs = Some(200);
        b.artwork_url = Some("http://art".into());
        let first = p.create("Café ☕ 🎶", vec![song("A"), b]).unwrap().id;
        let second = p.create("Second", vec![song("C")]).unwrap().id;
        p.reorder(&first, strings(&["B", "A"])).unwrap();
        let (before_first, before_second, before_list) =
            (p.get(&first).unwrap(), p.get(&second).unwrap(), p.list());
        drop(p);

        let p = Playlists::open(Db::open(&path).unwrap()).unwrap();
        assert_eq!(p.get(&first).unwrap(), before_first);
        assert_eq!(p.get(&second).unwrap(), before_second);
        assert_eq!(p.list(), before_list);
        let reloaded = p.get(&first).unwrap();
        assert_eq!(reloaded.name, "Café ☕ 🎶");
        assert_eq!(ids(&reloaded), ["B", "A"]);
        assert_eq!(reloaded.songs[0].title, "Été 🎵");
        // Creation order is kept in memory.
        assert_eq!(
            p.lock().iter().map(|x| x.id.clone()).collect::<Vec<_>>(),
            [first, second]
        );
    }

    #[test]
    fn delete_survives_reopen() {
        let dir = TempDir::new("delete");
        let path = dir.0.join("pinson.db");
        let p = Playlists::open(Db::open(&path).unwrap()).unwrap();
        let keep = p.create("Keep", vec![song("a"), song("b")]).unwrap().id;
        let gone = p.create("Gone", vec![song("c")]).unwrap().id;
        p.delete(&gone).unwrap();
        drop(p);
        let p = Playlists::open(Db::open(&path).unwrap()).unwrap();
        assert_eq!(p.list().len(), 1);
        assert!(p.get(&gone).is_none());
        assert_eq!(ids(&p.get(&keep).unwrap()), ["a", "b"]);
    }

    #[test]
    fn unknown_id_is_not_found() {
        let p = Playlists::in_memory();
        assert!(matches!(
            p.rename("nope", "x"),
            Err(PlaylistError::NotFound(_))
        ));
        assert!(matches!(p.delete("nope"), Err(PlaylistError::NotFound(_))));
        assert!(matches!(
            p.add_songs("nope", vec![song("a")]),
            Err(PlaylistError::NotFound(_))
        ));
        assert!(matches!(
            p.remove_song("nope", "a"),
            Err(PlaylistError::NotFound(_))
        ));
        assert!(matches!(
            p.reorder("nope", vec![]),
            Err(PlaylistError::NotFound(_))
        ));
        assert!(p.get("nope").is_none());
    }

    #[test]
    fn subscribers_see_changes_and_failed_save_changes_nothing() {
        let p = Playlists::in_memory();
        let mut rx = p.subscribe();
        assert!(rx.borrow_and_update().is_empty());
        let id = p.create("P", vec![]).unwrap().id;
        assert!(rx.has_changed().unwrap());
        assert_eq!(rx.borrow_and_update().len(), 1);
        p.delete(&id).unwrap();
        assert!(rx.borrow_and_update().is_empty());

        let db = Db::in_memory();
        let f = Playlists::open(db.clone()).unwrap();
        db.with(|c| c.execute("DROP TABLE playlist_songs", []).map(|_| ()))
            .unwrap();
        assert!(matches!(f.create("P", vec![]), Err(PlaylistError::Io(_))));
        assert!(f.list().is_empty());
    }
}
