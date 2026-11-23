//! The SQLite database behind favorites, playlists and play history.
//!
//! One connection behind a mutex; the store is cheap to clone. Writes are
//! tiny, so blocking calls inside the lock are acceptable.

use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::Context;
use rusqlite::{Connection, Row, ToSql};

use crate::song::Song;

/// The song columns shared by `favorites`, `playlist_songs` and `history`.
pub(crate) const SONG_COLUMNS: &str = "video_id, title, artists, album, duration_secs, artwork_url";

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

#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
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
    let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if version < 1 {
        let tx = conn.transaction()?;
        tx.execute_batch(SCHEMA_V1)?;
        tx.pragma_update(None, "user_version", 1)?;
        tx.commit()?;
    }
    if version < 2 {
        let tx = conn.transaction()?;
        tx.execute_batch(SCHEMA_V2)?;
        tx.pragma_update(None, "user_version", 2)?;
        tx.commit()?;
    }
    if version < 3 {
        let tx = conn.transaction()?;
        tx.execute_batch(SCHEMA_V3)?;
        tx.pragma_update(None, "user_version", 3)?;
        tx.commit()?;
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
        assert_eq!(user_version(&db), 3);
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
                "favorites",
                "followed_artists",
                "hidden_artists",
                "hidden_songs",
                "history",
                "playlist_songs",
                "playlists",
                "settings"
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
        assert_eq!(user_version(&db), 3);
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
        assert_eq!(user_version(&db), 3);
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
        assert_eq!(user_version(&db), 3);
        assert_eq!(db.setting("k").unwrap().as_deref(), Some("v"));
        let follows: i64 = db
            .with(|c| c.query_row("SELECT COUNT(*) FROM followed_artists", [], |r| r.get(0)))
            .unwrap();
        assert_eq!(follows, 0);
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
        let sql = format!(
            "INSERT INTO favorites (position, {SONG_COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)"
        );
        let back: Vec<Song> = db
            .with(|c| {
                for (i, s) in [&bare, &full].into_iter().enumerate() {
                    let pos = i as i64;
                    let p = song_params(s);
                    c.execute(
                        &sql,
                        rusqlite::params![pos, p[0], p[1], p[2], p[3], p[4], p[5]],
                    )?;
                }
                let mut stmt = c.prepare(&format!(
                    "SELECT position, {SONG_COLUMNS} FROM favorites ORDER BY position"
                ))?;
                stmt.query_map([], |r| read_song(r, 1))?.collect()
            })
            .unwrap();
        assert_eq!(back, [bare, full]);
    }
}
