//! One-time import of the old `favorites.json` / `playlists.json` files.

use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::de::DeserializeOwned;

use crate::db::Db;
use crate::favorites::save_favorites;
use crate::playlists::{Playlist, save_playlists};
use crate::song::Song;

/// What [`import_json_once`] did. `None` means that file did not exist.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImportReport {
    pub already_done: bool,
    pub favorites: Option<usize>,
    pub playlists: Option<usize>,
}

/// Moves the old JSON files into the database, once. A file that is
/// unreadable or unparsable fails the whole import and nothing is written.
/// Imported files are renamed to `<file>.imported`.
pub fn import_json_once(
    db: &Db,
    favorites_file: &Path,
    playlists_file: &Path,
) -> anyhow::Result<ImportReport> {
    if db.setting("import_done")?.is_some() {
        return Ok(ImportReport {
            already_done: true,
            favorites: None,
            playlists: None,
        });
    }
    let favorites: Option<Vec<Song>> = read_json(favorites_file)?;
    let playlists: Option<Vec<Playlist>> = read_json(playlists_file)?;
    db.with(|c| {
        let tx = c.transaction()?;
        if let Some(songs) = &favorites {
            save_favorites(&tx, songs)?;
        }
        if let Some(all) = &playlists {
            save_playlists(&tx, all)?;
        }
        tx.execute(
            "INSERT INTO settings (key, value) VALUES ('import_done', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [chrono::Utc::now().to_rfc3339()],
        )?;
        tx.commit()
    })?;
    if favorites.is_some() {
        mark_imported(favorites_file);
    }
    if playlists.is_some() {
        mark_imported(playlists_file);
    }
    Ok(ImportReport {
        already_done: false,
        favorites: favorites.map(|v| v.len()),
        playlists: playlists.map(|v| v.len()),
    })
}

fn read_json<T: DeserializeOwned>(path: &Path) -> anyhow::Result<Option<T>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    serde_json::from_str(&text)
        .map(Some)
        .with_context(|| format!("parsing {}", path.display()))
}

fn mark_imported(path: &Path) {
    let mut name = path.as_os_str().to_owned();
    name.push(".imported");
    let target = PathBuf::from(name);
    if let Err(e) = std::fs::rename(path, &target) {
        tracing::warn!("renaming {} to {}: {e}", path.display(), target.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::favorites::Favorites;
    use crate::playlists::{Playlist, Playlists};
    use crate::song::Song;
    use std::path::PathBuf;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "pinson-import-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
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
            title: format!("Title {id}"),
            artists: "Artist".into(),
            album: Some("Album".into()),
            duration_secs: Some(200),
            artwork_url: None,
        }
    }

    fn sample_playlist() -> Playlist {
        Playlist {
            id: "abc1234567".into(),
            name: "Soirée".into(),
            songs: vec![song("X"), song("Y")],
            created_at: "2026-01-01T10:00:00+00:00".into(),
            updated_at: "2026-01-02T11:30:00+00:00".into(),
        }
    }

    fn write_favorites(path: &Path, ids: &[&str]) {
        let songs: Vec<Song> = ids.iter().map(|i| song(i)).collect();
        std::fs::write(path, serde_json::to_string(&songs).unwrap()).unwrap();
    }

    fn write_playlists(path: &Path) {
        std::fs::write(path, serde_json::to_string(&[sample_playlist()]).unwrap()).unwrap();
    }

    fn imported(path: &Path) -> PathBuf {
        let mut s = path.as_os_str().to_owned();
        s.push(".imported");
        PathBuf::from(s)
    }

    fn fav_ids(db: &Db) -> Vec<String> {
        Favorites::open(db.clone())
            .unwrap()
            .list()
            .into_iter()
            .map(|s| s.video_id)
            .collect()
    }

    #[test]
    fn imports_both_files_preserving_everything() {
        let dir = TempDir::new("both");
        let (f, p) = (dir.0.join("favorites.json"), dir.0.join("playlists.json"));
        write_favorites(&f, &["B", "A"]);
        write_playlists(&p);
        let db = Db::in_memory();
        let report = import_json_once(&db, &f, &p).unwrap();
        assert_eq!(
            report,
            ImportReport {
                already_done: false,
                favorites: Some(2),
                playlists: Some(1)
            }
        );
        assert_eq!(fav_ids(&db), ["B", "A"]);
        assert_eq!(
            Playlists::open(db.clone()).unwrap().get("abc1234567"),
            Some(sample_playlist())
        );
        assert!(db.setting("import_done").unwrap().is_some());
        assert!(!f.exists() && !p.exists());
        assert!(imported(&f).exists() && imported(&p).exists());
    }

    #[test]
    fn second_run_does_nothing() {
        let dir = TempDir::new("twice");
        let (f, p) = (dir.0.join("favorites.json"), dir.0.join("playlists.json"));
        write_favorites(&f, &["B", "A"]);
        let db = Db::in_memory();
        import_json_once(&db, &f, &p).unwrap();
        write_favorites(&f, &["Z"]);
        let report = import_json_once(&db, &f, &p).unwrap();
        assert!(report.already_done);
        assert_eq!(report.favorites, None);
        assert_eq!(fav_ids(&db), ["B", "A"]);
        assert!(f.exists());
    }

    #[test]
    fn no_files_sets_the_marker() {
        let dir = TempDir::new("none");
        let db = Db::in_memory();
        let report = import_json_once(&db, &dir.0.join("f.json"), &dir.0.join("p.json")).unwrap();
        assert!(!report.already_done);
        assert_eq!((report.favorites, report.playlists), (None, None));
        assert!(db.setting("import_done").unwrap().is_some());
    }

    #[test]
    fn imports_only_the_files_that_exist() {
        let dir = TempDir::new("one");
        let (f, p) = (dir.0.join("favorites.json"), dir.0.join("playlists.json"));
        write_favorites(&f, &["A"]);
        let db = Db::in_memory();
        let report = import_json_once(&db, &f, &p).unwrap();
        assert_eq!((report.favorites, report.playlists), (Some(1), None));
        assert!(imported(&f).exists());
        assert!(!imported(&p).exists());
        assert!(db.setting("import_done").unwrap().is_some());
    }

    #[test]
    fn bad_json_fails_and_changes_nothing() {
        let dir = TempDir::new("bad");
        let (f, p) = (dir.0.join("favorites.json"), dir.0.join("playlists.json"));
        std::fs::write(&f, "{not json").unwrap();
        write_playlists(&p);
        let db = Db::in_memory();
        assert!(import_json_once(&db, &f, &p).is_err());
        assert!(db.setting("import_done").unwrap().is_none());
        assert!(fav_ids(&db).is_empty());
        assert!(Playlists::open(db.clone()).unwrap().list().is_empty());
        assert!(f.exists() && p.exists());
        assert!(!imported(&f).exists() && !imported(&p).exists());
    }
}
