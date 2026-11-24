//! The SQLite database behind favorites, playlists and play history.
//!
//! One connection behind a mutex; the store is cheap to clone. Writes are
//! tiny, so blocking calls inside the lock are acceptable.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Context;
use rusqlite::{Connection, Row, ToSql, TransactionBehavior};

use crate::song::Song;

/// The public metadata columns of the song catalog.
pub(crate) const SONG_COLUMNS: &str = "video_id, title, artists, album, duration_secs, artwork_url";
pub(crate) const CATALOG_SONG_COLUMNS: &str = "songs.video_id, songs.title, songs.artists, songs.album, songs.duration_secs, songs.artwork_url";

const SCHEMA_V1: &str = "
CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE favorites (
    position INTEGER PRIMARY KEY,
    video_id TEXT NOT NULL, title TEXT NOT NULL, artists TEXT NOT NULL,
    album TEXT, duration_secs INTEGER, artwork_url TEXT
);
CREATE TABLE playlists (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE TABLE playlist_songs (
    playlist_id TEXT NOT NULL REFERENCES playlists(id) ON DELETE CASCADE,
    position INTEGER NOT NULL,
    video_id TEXT NOT NULL, title TEXT NOT NULL, artists TEXT NOT NULL,
    album TEXT, duration_secs INTEGER, artwork_url TEXT,
    PRIMARY KEY (playlist_id, position)
);
CREATE TABLE history (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    played_at TEXT NOT NULL,
    video_id TEXT NOT NULL, title TEXT NOT NULL, artists TEXT NOT NULL,
    album TEXT, duration_secs INTEGER, artwork_url TEXT
);
";

const SCHEMA_V2: &str = "
CREATE TABLE hidden_songs (
    video_id TEXT PRIMARY KEY, title TEXT NOT NULL, artists TEXT NOT NULL,
    album TEXT, duration_secs INTEGER, artwork_url TEXT,
    hidden_at INTEGER NOT NULL
);
CREATE TABLE hidden_artists (
    key TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    hidden_at INTEGER NOT NULL
);
";

const SCHEMA_V3: &str = "
CREATE TABLE followed_artists (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    artwork_url TEXT,
    followed_at INTEGER NOT NULL,
    checked_at INTEGER
);
CREATE TABLE artist_releases (
    artist_id TEXT NOT NULL,
    id TEXT NOT NULL,
    kind TEXT NOT NULL,
    title TEXT NOT NULL,
    subtitle TEXT,
    artwork_url TEXT,
    year TEXT,
    first_seen INTEGER NOT NULL,
    is_new INTEGER NOT NULL,
    PRIMARY KEY (artist_id, id)
);
";

const SCHEMA_V4: &str = "
CREATE TABLE songs (
    id INTEGER PRIMARY KEY,
    video_id TEXT UNIQUE,
    local_path TEXT UNIQUE CHECK (local_path IS NULL OR length(local_path) > 0),
    local_is_cache INTEGER NOT NULL DEFAULT 0 CHECK (
        local_is_cache = 0 OR (local_is_cache = 1 AND video_id IS NOT NULL AND local_path IS NOT NULL)
    ),
    title TEXT NOT NULL, artists TEXT NOT NULL,
    album TEXT, duration_secs INTEGER, artwork_url TEXT,
    CHECK (video_id IS NOT NULL OR local_path IS NOT NULL)
);

ALTER TABLE playlists ADD COLUMN kind TEXT NOT NULL DEFAULT 'user' CHECK (kind IN ('user', 'favorites'));
CREATE UNIQUE INDEX playlists_favorites ON playlists(kind) WHERE kind = 'favorites';
INSERT INTO playlists (id, name, created_at, updated_at, kind)
VALUES ('@' || lower(hex(randomblob(16))), 'Favorites',
        strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), 'favorites');

INSERT INTO songs (video_id, title, artists, album, duration_secs, artwork_url)
SELECT video_id, title, artists, album, duration_secs, artwork_url
FROM favorites WHERE true ORDER BY position
ON CONFLICT(video_id) DO NOTHING;

INSERT INTO songs (video_id, title, artists, album, duration_secs, artwork_url)
SELECT ps.video_id, ps.title, ps.artists, ps.album, ps.duration_secs, ps.artwork_url
FROM playlist_songs ps JOIN playlists p ON p.id = ps.playlist_id
WHERE true ORDER BY p.updated_at DESC, p.rowid, ps.position
ON CONFLICT(video_id) DO UPDATE SET
    album = COALESCE(songs.album, excluded.album),
    duration_secs = COALESCE(songs.duration_secs, excluded.duration_secs),
    artwork_url = COALESCE(songs.artwork_url, excluded.artwork_url);

INSERT INTO songs (video_id, title, artists, album, duration_secs, artwork_url)
SELECT video_id, title, artists, album, duration_secs, artwork_url
FROM history WHERE true ORDER BY id DESC
ON CONFLICT(video_id) DO UPDATE SET
    album = COALESCE(songs.album, excluded.album),
    duration_secs = COALESCE(songs.duration_secs, excluded.duration_secs),
    artwork_url = COALESCE(songs.artwork_url, excluded.artwork_url);

ALTER TABLE playlist_songs RENAME TO playlist_songs_v3;
CREATE TABLE playlist_songs (
    playlist_id TEXT NOT NULL REFERENCES playlists(id) ON DELETE CASCADE,
    position INTEGER NOT NULL,
    song_id INTEGER NOT NULL REFERENCES songs(id),
    PRIMARY KEY (playlist_id, position)
);
INSERT INTO playlist_songs (playlist_id, position, song_id)
SELECT old.playlist_id, old.position, songs.id
FROM playlist_songs_v3 old JOIN songs ON songs.video_id = old.video_id;
INSERT INTO playlist_songs (playlist_id, position, song_id)
SELECT p.id, old.position, songs.id
FROM favorites old JOIN songs ON songs.video_id = old.video_id
CROSS JOIN playlists p WHERE p.kind = 'favorites';
DROP TABLE favorites;
DROP TABLE playlist_songs_v3;
CREATE INDEX playlist_songs_song ON playlist_songs(song_id);

ALTER TABLE history RENAME TO history_v3;
CREATE TABLE history (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    played_at TEXT NOT NULL,
    song_id INTEGER NOT NULL REFERENCES songs(id)
);
INSERT INTO history (id, played_at, song_id)
SELECT old.id, old.played_at, songs.id
FROM history_v3 old JOIN songs ON songs.video_id = old.video_id;
INSERT INTO sqlite_sequence (name, seq)
SELECT 'history', seq FROM sqlite_sequence
WHERE name = 'history_v3' AND NOT EXISTS (SELECT 1 FROM sqlite_sequence WHERE name = 'history');
UPDATE sqlite_sequence
SET seq = MAX(seq, COALESCE((SELECT seq FROM sqlite_sequence WHERE name = 'history_v3'), 0))
WHERE name = 'history';
DROP TABLE history_v3;
CREATE INDEX history_song ON history(song_id);
CREATE INDEX history_played_at ON history(played_at);
";

#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
    song_revision: Arc<AtomicU64>,
}

impl Db {
    /// Opens (creating it and its parent directories if needed) and migrates.
    pub fn open(path: &Path) -> anyhow::Result<Db> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("creating directory {}", dir.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("opening database {}", path.display()))?;
        Self::init(conn).with_context(|| format!("initializing database {}", path.display()))
    }

    /// A private in-memory database (tests). Panics on failure.
    pub fn in_memory() -> Db {
        let conn = Connection::open_in_memory().expect("open in-memory database");
        Self::init(conn).expect("initialize in-memory database")
    }

    fn init(mut conn: Connection) -> anyhow::Result<Db> {
        // `journal_mode` returns a row, so it can't go through execute.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_millis(5000))?;
        migrate(&mut conn)?;
        Ok(Db {
            conn: Arc::new(Mutex::new(conn)),
            song_revision: Arc::new(AtomicU64::new(0)),
        })
    }

    /// Runs `f` with the connection locked. A poisoned lock is recovered: a
    /// panic elsewhere can't leave the connection half-written, because
    /// writes go through transactions.
    pub fn with<T>(
        &self,
        f: impl FnOnce(&mut Connection) -> rusqlite::Result<T>,
    ) -> anyhow::Result<T> {
        let mut conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        Ok(f(&mut conn)?)
    }

    pub fn setting(&self, key: &str) -> anyhow::Result<Option<String>> {
        self.with(|c| {
            let mut stmt = c.prepare("SELECT value FROM settings WHERE key = ?1")?;
            let mut rows = stmt.query([key])?;
            rows.next()?.map(|r| r.get(0)).transpose()
        })
    }

    pub(crate) fn song_revision(&self) -> u64 {
        self.song_revision.load(Ordering::Relaxed)
    }

    pub(crate) fn songs_changed(&self) {
        self.song_revision.fetch_add(1, Ordering::Relaxed);
    }

    pub fn set_setting(&self, key: &str, value: &str) -> anyhow::Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO settings (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                [key, value],
            )
            .map(|_| ())
        })
    }
}

fn migrate(conn: &mut Connection) -> rusqlite::Result<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let version: i64 = tx.pragma_query_value(None, "user_version", |r| r.get(0))?;
    for (target, schema) in [
        (1, SCHEMA_V1),
        (2, SCHEMA_V2),
        (3, SCHEMA_V3),
        (4, SCHEMA_V4),
    ] {
        if version < target {
            tx.execute_batch(schema)?;
            tx.pragma_update(None, "user_version", target)?;
        }
    }
    tx.commit()
}

/// Finds the shared record or registers a YouTube song without changing any local reference.
pub(crate) fn register_song(tx: &rusqlite::Transaction, song: &Song) -> rusqlite::Result<i64> {
    tx.execute(
        "INSERT INTO songs (video_id, title, artists, album, duration_secs, artwork_url)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6) ON CONFLICT(video_id) DO NOTHING",
        song_params(song),
    )?;
    tx.query_row(
        "SELECT id FROM songs WHERE video_id = ?1",
        [&song.video_id],
        |r| r.get(0),
    )
}

/// Updates the public metadata supplied when a track is added to favorites.
pub(crate) fn refresh_song(tx: &rusqlite::Transaction, song: &Song) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO songs (video_id, title, artists, album, duration_secs, artwork_url)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(video_id) DO UPDATE SET title = excluded.title, artists = excluded.artists,
            album = COALESCE(excluded.album, songs.album),
            duration_secs = COALESCE(excluded.duration_secs, songs.duration_secs),
            artwork_url = COALESCE(excluded.artwork_url, songs.artwork_url)",
        song_params(song),
    )?;
    Ok(())
}

pub(crate) fn playlist_tracks(conn: &Connection, playlist_id: &str) -> rusqlite::Result<Vec<Song>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {CATALOG_SONG_COLUMNS} FROM playlist_songs JOIN songs ON songs.id = song_id
         WHERE playlist_id = ?1 ORDER BY position"
    ))?;
    stmt.query_map([playlist_id], |r| read_song(r, 0))?
        .collect()
}

pub(crate) fn replace_playlist_tracks(
    tx: &rusqlite::Transaction,
    playlist_id: &str,
    songs: &[Song],
) -> rusqlite::Result<()> {
    tx.execute(
        "DELETE FROM playlist_songs WHERE playlist_id = ?1",
        [playlist_id],
    )?;
    let mut insert = tx.prepare_cached(
        "INSERT INTO playlist_songs (playlist_id, position, song_id) VALUES (?1, ?2, ?3)",
    )?;
    for (position, song) in songs.iter().enumerate() {
        let song_id = register_song(tx, song)?;
        insert.execute((playlist_id, position as i64, song_id))?;
    }
    Ok(())
}

/// Reads the six [`SONG_COLUMNS`] starting at column `first`.
pub(crate) fn read_song(row: &Row, first: usize) -> rusqlite::Result<Song> {
    Ok(Song {
        video_id: row.get(first)?,
        title: row.get(first + 1)?,
        artists: row.get(first + 2)?,
        album: row.get(first + 3)?,
        duration_secs: row.get(first + 4)?,
        artwork_url: row.get(first + 5)?,
    })
}

/// A song's values in [`SONG_COLUMNS`] order.
pub(crate) fn song_params(song: &Song) -> [&dyn ToSql; 6] {
    [
        &song.video_id,
        &song.title,
        &song.artists,
        &song.album,
        &song.duration_secs,
        &song.artwork_url,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A fresh directory under the system temp dir, removed on drop.
    pub(crate) struct TempDir(pub PathBuf);

    impl TempDir {
        pub(crate) fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "pinson-db-{tag}-{}-{:?}",
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

    fn user_version(db: &Db) -> i64 {
        db.with(|c| c.pragma_query_value(None, "user_version", |r| r.get(0)))
            .unwrap()
    }

    #[test]
    fn open_creates_the_schema() {
        let dir = TempDir::new("schema");
        let db = Db::open(&dir.0.join("pinson.db")).unwrap();
        assert_eq!(user_version(&db), 4);
        let tables: Vec<String> = db
            .with(|c| {
                let mut stmt = c.prepare(
                    "SELECT name FROM sqlite_master WHERE type = 'table'
                     AND name NOT LIKE 'sqlite_%' ORDER BY name",
                )?;
                stmt.query_map([], |r| r.get(0))?.collect()
            })
            .unwrap();
        assert_eq!(
            tables,
            [
                "artist_releases",
                "followed_artists",
                "hidden_artists",
                "hidden_songs",
                "history",
                "playlist_songs",
                "playlists",
                "settings",
                "songs"
            ]
        );
    }

    #[test]
    fn reopen_keeps_data_and_does_not_remigrate() {
        let dir = TempDir::new("reopen");
        let path = dir.0.join("pinson.db");
        let db = Db::open(&path).unwrap();
        db.set_setting("k", "v").unwrap();
        drop(db);
        let db = Db::open(&path).unwrap();
        assert_eq!(db.setting("k").unwrap().as_deref(), Some("v"));
        assert_eq!(user_version(&db), 4);
    }

    #[test]
    fn a_v1_database_gets_the_hidden_tables() {
        let dir = TempDir::new("upgrade");
        let path = dir.0.join("pinson.db");
        std::fs::create_dir_all(&dir.0).unwrap();
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(SCHEMA_V1).unwrap();
            conn.pragma_update(None, "user_version", 1).unwrap();
        }
        let db = Db::open(&path).unwrap();
        assert_eq!(user_version(&db), 4);
        assert!(
            db.with(|c| c.prepare("SELECT key FROM hidden_artists").map(|_| ()))
                .is_ok()
        );
    }

    #[test]
    fn a_v2_database_gets_the_follow_tables_keeping_its_data() {
        let dir = TempDir::new("upgrade-v2");
        let path = dir.0.join("pinson.db");
        std::fs::create_dir_all(&dir.0).unwrap();
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(SCHEMA_V1).unwrap();
            conn.execute_batch(SCHEMA_V2).unwrap();
            conn.pragma_update(None, "user_version", 2).unwrap();
            conn.execute("INSERT INTO settings (key, value) VALUES ('k', 'v')", [])
                .unwrap();
        }
        let db = Db::open(&path).unwrap();
        assert_eq!(user_version(&db), 4);
        assert_eq!(db.setting("k").unwrap().as_deref(), Some("v"));
        let follows: i64 = db
            .with(|c| c.query_row("SELECT COUNT(*) FROM followed_artists", [], |r| r.get(0)))
            .unwrap();
        assert_eq!(follows, 0);
    }

    #[test]
    fn v3_migrates_favorites_playlists_and_history_to_shared_songs() {
        use crate::{favorites::Favorites, history::History, playlists::Playlists};

        let dir = TempDir::new("upgrade-v3");
        let path = dir.0.join("pinson.db");
        std::fs::create_dir_all(&dir.0).unwrap();
        {
            let conn = Connection::open(&path).unwrap();
            for schema in [SCHEMA_V1, SCHEMA_V2, SCHEMA_V3] {
                conn.execute_batch(schema).unwrap();
            }
            conn.pragma_update(None, "user_version", 3).unwrap();
            conn.execute_batch("
                INSERT INTO playlists VALUES
                    ('p1', 'First', '2026-11-01', '2026-11-02'),
                    ('p2', 'Second', '2026-11-02', '2026-11-03');
                INSERT INTO playlist_songs VALUES
                    ('p1', 0, 'b', 'Beta', 'Artist', NULL, NULL, NULL),
                    ('p1', 1, 'a', 'Album title', 'Artist', 'Album', 120, NULL),
                    ('p2', 0, 'a', 'Live title', 'Artist', NULL, NULL, NULL);
                INSERT INTO favorites VALUES
                    (0, 'a', 'Favorite title', 'Artist', NULL, NULL, NULL),
                    (1, 'c', 'Gamma', 'Artist', NULL, NULL, NULL);
                INSERT INTO history VALUES
                    (3, '2026-11-04T12:00:00+00:00', 'a', 'Old title', 'Artist', NULL, NULL, NULL),
                    (8, '2026-11-05T12:00:00+00:00', 'd', 'Delta', 'Artist', NULL, NULL, NULL),
                    (12, '2026-11-06T12:00:00+00:00', 'a', 'Another title', 'Artist', NULL, NULL, NULL),
                    (20, '2026-11-07T12:00:00+00:00', 'a', 'Deleted play', 'Artist', NULL, NULL, NULL);
                DELETE FROM history WHERE id = 20;
            ").unwrap();
        }

        let db = Db::open(&path).unwrap();
        let playlists = Playlists::open(db.clone()).unwrap();
        let favorites = Favorites::open(db.clone()).unwrap();
        let history = History::new(db.clone());
        assert_eq!(playlists.list().len(), 2);
        let first = playlists.get("p1").unwrap();
        assert_eq!(first.created_at, "2026-11-01");
        assert_eq!(first.updated_at, "2026-11-02");
        assert_eq!(
            first
                .songs
                .iter()
                .map(|song| song.video_id.as_str())
                .collect::<Vec<_>>(),
            ["b", "a"]
        );
        let liked = favorites.list();
        assert_eq!(
            liked
                .iter()
                .map(|song| song.video_id.as_str())
                .collect::<Vec<_>>(),
            ["a", "c"]
        );
        assert_eq!(liked[0], first.songs[1]);
        assert_eq!(liked[0].title, "Favorite title");
        assert_eq!(liked[0].album.as_deref(), Some("Album"));

        let page = history.page(None, None, 10).unwrap();
        assert_eq!(
            page.iter().map(|play| play.id).collect::<Vec<_>>(),
            [12, 8, 3]
        );
        assert_eq!(page[0].played_at, "2026-11-06T12:00:00+00:00");
        assert_eq!(page[0].song, page[2].song);
        assert_eq!(page[0].song, liked[0]);
        let count: i64 = db
            .with(|c| c.query_row("SELECT COUNT(*) FROM songs", [], |r| r.get(0)))
            .unwrap();
        assert_eq!(count, 4);

        let mut refreshed = liked[0].clone();
        refreshed.title = "Updated favorite title".into();
        favorites.add(refreshed.clone()).unwrap();
        assert_eq!(playlists.get("p2").unwrap().songs, [refreshed.clone()]);
        assert_eq!(history.page(None, None, 1).unwrap()[0].song, refreshed);
        playlists.delete("p1").unwrap();
        assert_eq!(favorites.list()[0], refreshed);
        assert_eq!(favorites.list()[1], liked[1]);
        history.record(&liked[0]).unwrap();
        assert_eq!(history.page(None, None, 1).unwrap()[0].id, 21);
        assert_eq!(user_version(&Db::open(&path).unwrap()), 4);
    }

    #[test]
    fn songs_can_reference_youtube_local_audio_or_both() {
        let db = Db::in_memory();
        db.with(|conn| {
            let mut insert = conn.prepare("INSERT INTO songs (video_id, local_path, title, artists) VALUES (?1, ?2, 'Title', 'Artist')")?;
            insert.execute((Some("a"), None::<&str>))?;
            insert.execute((None::<&str>, Some("local/a.mp3")))?;
            insert.execute((Some("b"), Some("local/b.mp3")))?;
            assert!(insert.execute((None::<&str>, None::<&str>)).is_err());
            Ok(())
        }).unwrap();
    }

    #[test]
    fn settings_upsert() {
        let db = Db::in_memory();
        assert_eq!(db.setting("x").unwrap(), None);
        db.set_setting("x", "1").unwrap();
        db.set_setting("x", "2").unwrap();
        assert_eq!(db.setting("x").unwrap().as_deref(), Some("2"));
    }

    #[test]
    fn open_creates_parent_dirs() {
        let dir = TempDir::new("parents");
        let path = dir.0.join("a").join("b").join("pinson.db");
        Db::open(&path).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn song_row_round_trips() {
        let bare = Song {
            video_id: "a".into(),
            title: "Été 🎵".into(),
            artists: "artist".into(),
            album: None,
            duration_secs: None,
            artwork_url: None,
        };
        let full = Song {
            video_id: "b".into(),
            title: "t".into(),
            artists: "x, y".into(),
            album: Some("alb".into()),
            duration_secs: Some(225),
            artwork_url: Some("http://art".into()),
        };
        let db = Db::in_memory();
        let back: Vec<Song> = db
            .with(|c| {
                let tx = c.transaction()?;
                register_song(&tx, &bare)?;
                register_song(&tx, &full)?;
                let back = tx
                    .prepare(&format!("SELECT {SONG_COLUMNS} FROM songs ORDER BY id"))?
                    .query_map([], |r| read_song(r, 0))?
                    .collect::<rusqlite::Result<_>>()?;
                tx.commit()?;
                Ok(back)
            })
            .unwrap();
        assert_eq!(back, [bare, full]);
    }
}
