//! Followed artists and their new releases, kept in SQLite.
//!
//! Following stores the artist and every album and single the artist page
//! lists now, as already known. A background refresh later fetches each
//! followed artist and records the releases it has not seen as new, so Home
//! renders from the database without calling YouTube Music.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use chrono::{Datelike, Utc};
use rusqlite::{OptionalExtension, params};
use serde::Serialize;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::catalog::{Artist, Catalog, CollectionSummary};
use crate::db::Db;

/// How long a release counts as new, in days.
pub const NEW_WINDOW_DAYS: i64 = 60;
/// Most releases `GET /api/follows/releases` lists.
pub const MAX_NEW_RELEASES: usize = 12;
/// How often the refresher checks every followed artist.
pub const REFRESH_INTERVAL: Duration = Duration::from_secs(12 * 60 * 60);
/// The pause between two artists in one refresh, to be gentle on YouTube.
pub const REFRESH_PAUSE: Duration = Duration::from_secs(2);

/// A followed artist. Times are Unix seconds.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FollowedArtist {
    pub id: String,
    pub name: String,
    pub artwork_url: Option<String>,
    pub followed_at: i64,
    /// Releases first seen inside the new window.
    pub new_count: u32,
}

/// A new album or single of a followed artist.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Release {
    pub id: String,
    /// `"album"` or `"single"`.
    pub kind: String,
    pub title: String,
    pub subtitle: Option<String>,
    pub artwork_url: Option<String>,
    pub year: Option<String>,
    pub artist_id: String,
    pub artist_name: String,
    /// Unix seconds.
    pub first_seen: i64,
}

struct Inner {
    db: Db,
    catalog: Arc<dyn Catalog>,
    pause: Duration,
    refresh_now: Notify,
}

/// Cheap to clone; clones share one database and one refresh signal.
#[derive(Clone)]
pub struct Follows {
    inner: Arc<Inner>,
}

impl Follows {
    /// `pause` is waited between two artists during a refresh.
    pub fn new(db: Db, catalog: Arc<dyn Catalog>, pause: Duration) -> Self {
        Self {
            inner: Arc::new(Inner {
                db,
                catalog,
                pause,
                refresh_now: Notify::new(),
            }),
        }
    }

    /// Follows an artist: fetches its page and stores it with every release
    /// the page lists as already known. Only releases of the current year
    /// count as new. `Ok(None)` when the catalog does not know the id.
    /// Following an artist already followed changes nothing. Wakes the
    /// refresher.
    pub async fn follow(&self, artist_id: &str) -> anyhow::Result<Option<FollowedArtist>> {
        if let Some(existing) = self.get(artist_id)? {
            return Ok(Some(existing));
        }
        let Some(artist) = self.inner.catalog.artist(artist_id).await? else {
            return Ok(None);
        };
        let now = Utc::now().timestamp();
        let year = Utc::now().year().to_string();
        let id = artist.id.clone();
        self.inner.db.with(|c| {
            let tx = c.transaction()?;
            tx.execute(
                "INSERT OR IGNORE INTO followed_artists
                     (id, name, artwork_url, followed_at, checked_at)
                 VALUES (?1, ?2, ?3, ?4, ?4)",
                params![id, artist.name, artist.artwork_url, now],
            )?;
            for (kind, list) in [("album", &artist.albums), ("single", &artist.singles)] {
                for r in list {
                    let release_year = r.subtitle.as_deref().and_then(release_year);
                    let is_new = release_year.as_deref() == Some(year.as_str());
                    insert_release(&tx, &id, kind, r, release_year, now, is_new)?;
                }
            }
            tx.commit()
        })?;
        self.inner.refresh_now.notify_one();
        self.get(&id)
    }

    /// Unfollows an artist and drops its stored releases; returns whether it
    /// was followed.
    pub fn unfollow(&self, artist_id: &str) -> anyhow::Result<bool> {
        self.inner.db.with(|c| {
            let tx = c.transaction()?;
            tx.execute(
                "DELETE FROM artist_releases WHERE artist_id = ?1",
                [artist_id],
            )?;
            let removed = tx.execute("DELETE FROM followed_artists WHERE id = ?1", [artist_id])?;
            tx.commit()?;
            Ok(removed > 0)
        })
    }

    /// Whether the artist is followed.
    pub fn is_following(&self, artist_id: &str) -> anyhow::Result<bool> {
        self.inner.db.with(|c| {
            c.query_row(
                "SELECT 1 FROM followed_artists WHERE id = ?1",
                [artist_id],
                |_| Ok(()),
            )
            .optional()
            .map(|r| r.is_some())
        })
    }

    /// The followed artists, most recently followed first.
    pub fn list(&self) -> anyhow::Result<Vec<FollowedArtist>> {
        self.list_at(Utc::now().timestamp())
    }

    fn list_at(&self, now: i64) -> anyhow::Result<Vec<FollowedArtist>> {
        let cutoff = now - NEW_WINDOW_DAYS * 86_400;
        self.inner.db.with(|c| {
            let mut stmt = c.prepare(&format!(
                "{ARTIST_SELECT} ORDER BY a.followed_at DESC, a.name"
            ))?;
            stmt.query_map([cutoff], read_artist)?.collect()
        })
    }

    fn get(&self, artist_id: &str) -> anyhow::Result<Option<FollowedArtist>> {
        let cutoff = Utc::now().timestamp() - NEW_WINDOW_DAYS * 86_400;
        self.inner.db.with(|c| {
            c.query_row(
                &format!("{ARTIST_SELECT} WHERE a.id = ?2"),
                params![cutoff, artist_id],
                read_artist,
            )
            .optional()
        })
    }

    /// New releases of followed artists seen in the last
    /// [`NEW_WINDOW_DAYS`], newest first, at most `max`.
    pub fn new_releases(&self, max: usize) -> anyhow::Result<Vec<Release>> {
        self.new_releases_at(Utc::now().timestamp(), max)
    }

    fn new_releases_at(&self, now: i64, max: usize) -> anyhow::Result<Vec<Release>> {
        let cutoff = now - NEW_WINDOW_DAYS * 86_400;
        let max = i64::try_from(max).unwrap_or(i64::MAX);
        self.inner.db.with(|c| {
            let mut stmt = c.prepare(
                "SELECT r.id, r.kind, r.title, r.subtitle, r.artwork_url, r.year,
                        r.artist_id, a.name, r.first_seen
                 FROM artist_releases r JOIN followed_artists a ON a.id = r.artist_id
                 WHERE r.is_new = 1 AND r.first_seen >= ?1
                 ORDER BY r.first_seen DESC, r.year DESC, r.title
                 LIMIT ?2",
            )?;
            stmt.query_map(params![cutoff, max], |r| {
                Ok(Release {
                    id: r.get(0)?,
                    kind: r.get(1)?,
                    title: r.get(2)?,
                    subtitle: r.get(3)?,
                    artwork_url: r.get(4)?,
                    year: r.get(5)?,
                    artist_id: r.get(6)?,
                    artist_name: r.get(7)?,
                    first_seen: r.get(8)?,
                })
            })?
            .collect()
        })
    }

    /// Checks every followed artist, least recently checked first, one at a
    /// time with a pause between them. An artist that fails is skipped until
    /// the next round. Returns how many new releases were found.
    pub async fn refresh_all(&self) -> anyhow::Result<usize> {
        let ids: Vec<String> = self.inner.db.with(|c| {
            let mut stmt = c.prepare(
                "SELECT id FROM followed_artists
                 ORDER BY checked_at IS NOT NULL, checked_at, followed_at",
            )?;
            stmt.query_map([], |r| r.get(0))?.collect()
        })?;
        let mut found = 0;
        for (i, id) in ids.iter().enumerate() {
            if i > 0 {
                tokio::time::sleep(self.inner.pause).await;
            }
            match self.refresh_one(id).await {
                Ok(n) => found += n,
                Err(e) => tracing::warn!("refreshing followed artist {id}: {e:#}"),
            }
        }
        Ok(found)
    }

    async fn refresh_one(&self, id: &str) -> anyhow::Result<usize> {
        let artist: Artist = self
            .inner
            .catalog
            .artist(id)
            .await?
            .with_context(|| format!("the catalog no longer knows artist {id}"))?;
        let now = Utc::now().timestamp();
        self.inner.db.with(|c| {
            let tx = c.transaction()?;
            // Unfollowed while fetching: store nothing.
            let touched = tx.execute(
                "UPDATE followed_artists SET name = ?2, artwork_url = ?3, checked_at = ?4
                 WHERE id = ?1",
                params![id, artist.name, artist.artwork_url, now],
            )?;
            let mut found = 0;
            if touched > 0 {
                for (kind, list) in [("album", &artist.albums), ("single", &artist.singles)] {
                    for r in list {
                        let year = r.subtitle.as_deref().and_then(release_year);
                        found += insert_release(&tx, id, kind, r, year, now, true)?;
                    }
                }
            }
            tx.commit()?;
            Ok(found)
        })
    }

    /// How long until the least recently checked artist is `interval` old:
    /// zero when it is overdue or was never checked, `interval` when nobody is
    /// followed.
    pub fn next_due(&self, interval: Duration) -> anyhow::Result<Duration> {
        let oldest: Option<i64> = self.inner.db.with(|c| {
            c.query_row(
                "SELECT MIN(COALESCE(checked_at, 0)) FROM followed_artists",
                [],
                |r| r.get(0),
            )
        })?;
        Ok(match oldest {
            None => interval,
            Some(checked) => {
                let age = Utc::now().timestamp().saturating_sub(checked).max(0) as u64;
                interval.saturating_sub(Duration::from_secs(age))
            }
        })
    }
}

/// Shortest wait before the first refresh after a start, so a restart doesn't
/// hit YouTube while the server is still starting up.
const FIRST_REFRESH_MIN: Duration = Duration::from_secs(60);

/// Runs [`Follows::refresh_all`] when the oldest check is `interval` old (at
/// least [`FIRST_REFRESH_MIN`] after start, so restarts don't reset the clock),
/// then every `interval`, and whenever someone follows an artist. Runs until
/// the task is aborted.
pub fn spawn_refresher(follows: Follows, interval: Duration) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut wait = follows
            .next_due(interval)
            .unwrap_or(interval)
            .max(FIRST_REFRESH_MIN);
        loop {
            tokio::select! {
                () = tokio::time::sleep(wait) => {}
                () = follows.inner.refresh_now.notified() => {}
            }
            match follows.refresh_all().await {
                Ok(0) => {}
                Ok(n) => tracing::info!("{n} new releases from followed artists"),
                Err(e) => tracing::warn!("refreshing followed artists: {e:#}"),
            }
            wait = interval;
        }
    })
}

/// Selects artists with their new count; `?1` is the window cutoff. Callers
/// append a WHERE and an ORDER BY (the WHERE may use `?2`).
const ARTIST_SELECT: &str = "SELECT a.id, a.name, a.artwork_url, a.followed_at,
        (SELECT COUNT(*) FROM artist_releases r
         WHERE r.artist_id = a.id AND r.is_new = 1 AND r.first_seen >= ?1)
     FROM followed_artists a";

fn read_artist(r: &rusqlite::Row) -> rusqlite::Result<FollowedArtist> {
    Ok(FollowedArtist {
        id: r.get(0)?,
        name: r.get(1)?,
        artwork_url: r.get(2)?,
        followed_at: r.get(3)?,
        new_count: r.get(4)?,
    })
}

/// Inserts a release unless the artist already has it; returns 1 when it was
/// inserted.
fn insert_release(
    tx: &rusqlite::Transaction,
    artist_id: &str,
    kind: &str,
    r: &CollectionSummary,
    year: Option<String>,
    now: i64,
    is_new: bool,
) -> rusqlite::Result<usize> {
    tx.execute(
        "INSERT OR IGNORE INTO artist_releases
             (artist_id, id, kind, title, subtitle, artwork_url, year, first_seen, is_new)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            artist_id,
            r.id,
            kind,
            r.title,
            r.subtitle,
            r.artwork_url,
            year,
            now,
            is_new
        ],
    )
}

/// The release year in an album or single subtitle: the first run of exactly
/// four digits that starts with 19 or 20.
pub fn release_year(subtitle: &str) -> Option<String> {
    let bytes = subtitle.as_bytes();
    let mut start = 0;
    while start < bytes.len() {
        if !bytes[start].is_ascii_digit() {
            start += 1;
            continue;
        }
        let end = bytes[start..]
            .iter()
            .position(|b| !b.is_ascii_digit())
            .map_or(bytes.len(), |n| start + n);
        let run = &subtitle[start..end];
        if run.len() == 4 && (run.starts_with("19") || run.starts_with("20")) {
            return Some(run.to_owned());
        }
        start = end;
    }
    None
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use async_trait::async_trait;

    use super::*;
    use crate::catalog::{ArtistSummary, Collection, CollectionKind};

    #[derive(Default)]
    struct FakeCatalog {
        artists: Mutex<HashMap<String, Artist>>,
        failing: Mutex<Vec<String>>,
    }

    impl FakeCatalog {
        fn set(&self, id: &str, albums: &[(&str, &str)], singles: &[(&str, &str)]) {
            let summaries = |list: &[(&str, &str)]| {
                list.iter()
                    .map(|(rid, year)| CollectionSummary {
                        kind: CollectionKind::Album,
                        id: (*rid).into(),
                        title: format!("title {rid}"),
                        subtitle: Some((*year).into()),
                        artwork_url: None,
                    })
                    .collect()
            };
            self.artists.lock().unwrap().insert(
                id.into(),
                Artist {
                    id: id.into(),
                    name: format!("name {id}"),
                    artwork_url: Some(format!("https://example.com/{id}.jpg")),
                    description: None,
                    subscribers: None,
                    songs: vec![],
                    albums: summaries(albums),
                    singles: summaries(singles),
                    related: vec![],
                },
            );
        }
    }

    #[async_trait]
    impl Catalog for FakeCatalog {
        async fn search_collections(
            &self,
            _: &str,
            _: CollectionKind,
        ) -> anyhow::Result<Vec<CollectionSummary>> {
            unimplemented!()
        }
        async fn album(&self, _: &str) -> anyhow::Result<Option<Collection>> {
            unimplemented!()
        }
        async fn playlist(&self, _: &str) -> anyhow::Result<Option<Collection>> {
            unimplemented!()
        }
        async fn artist(&self, id: &str) -> anyhow::Result<Option<Artist>> {
            if self.failing.lock().unwrap().iter().any(|f| f == id) {
                anyhow::bail!("YouTube Music is down");
            }
            Ok(self.artists.lock().unwrap().get(id).cloned())
        }
        async fn find_artist(&self, _: &str) -> anyhow::Result<Option<String>> {
            unimplemented!()
        }
        async fn search_artists(&self, _: &str) -> anyhow::Result<Vec<ArtistSummary>> {
            unimplemented!()
        }
        async fn youtube_lyrics(&self, _: &str) -> anyhow::Result<Option<String>> {
            unimplemented!()
        }
    }

    fn setup() -> (Follows, Arc<FakeCatalog>) {
        let catalog = Arc::new(FakeCatalog::default());
        let follows = Follows::new(Db::in_memory(), catalog.clone(), Duration::ZERO);
        (follows, catalog)
    }

    fn this_year() -> String {
        Utc::now().year().to_string()
    }

    #[tokio::test]
    async fn next_due_counts_from_the_oldest_check() {
        let (follows, catalog) = setup();
        let day = Duration::from_secs(12 * 3600);
        assert_eq!(follows.next_due(day).unwrap(), day, "nobody followed");
        catalog.set("UC1", &[("a", "2001")], &[]);
        follows.follow("UC1").await.unwrap();
        let due = follows.next_due(day).unwrap();
        assert!(due > day - Duration::from_secs(5), "just checked: {due:?}");
        let six_hours_ago = Utc::now().timestamp() - 6 * 3600;
        follows
            .inner
            .db
            .with(|c| {
                c.execute(
                    "UPDATE followed_artists SET checked_at = ?1",
                    [six_hours_ago],
                )
            })
            .unwrap();
        let due = follows.next_due(day).unwrap().as_secs();
        assert!((6 * 3600 - 5..=6 * 3600).contains(&due), "half way: {due}");
        follows
            .inner
            .db
            .with(|c| c.execute("UPDATE followed_artists SET checked_at = NULL", []))
            .unwrap();
        assert_eq!(
            follows.next_due(day).unwrap(),
            Duration::ZERO,
            "never checked"
        );
    }

    #[test]
    fn release_year_finds_a_standalone_year() {
        assert_eq!(release_year("2023").as_deref(), Some("2023"));
        assert_eq!(release_year("Album \u{b7} 1999").as_deref(), Some("1999"));
        assert_eq!(
            release_year("Single, 2008 reissue").as_deref(),
            Some("2008")
        );
        assert_eq!(release_year("2023 \u{b7} 2024").as_deref(), Some("2023"));
    }

    #[test]
    fn release_year_ignores_other_numbers() {
        assert_eq!(release_year(""), None);
        assert_eq!(release_year("Single"), None);
        assert_eq!(release_year("12345"), None);
        assert_eq!(release_year("1850"), None);
        assert_eq!(release_year("2105"), None);
        assert_eq!(release_year("120230"), None);
        assert_eq!(release_year("5 songs").as_deref(), None);
    }

    #[tokio::test]
    async fn follow_seeds_known_releases_and_counts_only_this_year_as_new() {
        let (follows, catalog) = setup();
        let y = this_year();
        catalog.set("UC1", &[("old", "2001"), ("fresh", &y)], &[("s", &y)]);
        let artist = follows.follow("UC1").await.unwrap().unwrap();
        assert_eq!(artist.id, "UC1");
        assert_eq!(artist.name, "name UC1");
        assert_eq!(artist.new_count, 2);
        let new = follows.new_releases(12).unwrap();
        let mut ids: Vec<_> = new.iter().map(|r| r.id.as_str()).collect();
        ids.sort();
        assert_eq!(ids, ["fresh", "s"]);
        assert_eq!(new.iter().find(|r| r.id == "s").unwrap().kind, "single");
        assert_eq!(new[0].artist_name, "name UC1");
        assert!(follows.is_following("UC1").unwrap());
    }

    #[tokio::test]
    async fn follow_unknown_artist_is_none_and_catalog_failure_is_an_error() {
        let (follows, catalog) = setup();
        assert!(follows.follow("UCnope").await.unwrap().is_none());
        catalog.failing.lock().unwrap().push("UC1".into());
        assert!(follows.follow("UC1").await.is_err());
        assert!(!follows.is_following("UC1").unwrap());
    }

    #[tokio::test]
    async fn following_twice_keeps_the_first_follow() {
        let (follows, catalog) = setup();
        catalog.set("UC1", &[("a", "2001")], &[]);
        let first = follows.follow("UC1").await.unwrap().unwrap();
        // A refetch would fail, so a second follow must not call the catalog.
        catalog.failing.lock().unwrap().push("UC1".into());
        let second = follows.follow("UC1").await.unwrap().unwrap();
        assert_eq!(first, second);
        assert_eq!(follows.list().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn refresh_records_only_unseen_releases_as_new() {
        let (follows, catalog) = setup();
        catalog.set("UC1", &[("old", "2001")], &[]);
        follows.follow("UC1").await.unwrap();
        assert_eq!(follows.refresh_all().await.unwrap(), 0);
        assert!(follows.new_releases(12).unwrap().is_empty());

        catalog.set("UC1", &[("old", "2001")], &[("brand-new", "2001")]);
        assert_eq!(follows.refresh_all().await.unwrap(), 1);
        let new = follows.new_releases(12).unwrap();
        assert_eq!(new.len(), 1);
        assert_eq!(new[0].id, "brand-new");
        assert_eq!(new[0].kind, "single");
        assert_eq!(follows.list().unwrap()[0].new_count, 1);

        // Seen now: another refresh finds nothing more.
        assert_eq!(follows.refresh_all().await.unwrap(), 0);
        assert_eq!(follows.new_releases(12).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn refresh_updates_the_name_and_skips_failing_artists() {
        let (follows, catalog) = setup();
        catalog.set("UC1", &[], &[]);
        catalog.set("UC2", &[], &[]);
        follows.follow("UC1").await.unwrap();
        follows.follow("UC2").await.unwrap();
        catalog.artists.lock().unwrap().get_mut("UC1").unwrap().name = "Renamed".into();
        catalog.failing.lock().unwrap().push("UC2".into());
        catalog
            .artists
            .lock()
            .unwrap()
            .get_mut("UC2")
            .unwrap()
            .albums = vec![CollectionSummary {
            kind: CollectionKind::Album,
            id: "x".into(),
            title: "x".into(),
            subtitle: None,
            artwork_url: None,
        }];
        assert_eq!(follows.refresh_all().await.unwrap(), 0);
        let names: Vec<_> = follows
            .list()
            .unwrap()
            .into_iter()
            .map(|a| a.name)
            .collect();
        assert!(names.contains(&"Renamed".to_owned()));
        assert!(names.contains(&"name UC2".to_owned()));
    }

    /// Stores a new release first seen `age_days` ago.
    fn seed(follows: &Follows, artist: &str, id: &str, year: &str, age_days: i64, is_new: bool) {
        let seen = Utc::now().timestamp() - age_days * 86_400;
        follows
            .inner
            .db
            .with(|c| {
                c.execute(
                    "INSERT INTO artist_releases
                         (artist_id, id, kind, title, subtitle, artwork_url, year, first_seen, is_new)
                     VALUES (?1, ?2, 'album', ?2, NULL, NULL, ?3, ?4, ?5)",
                    params![artist, id, year, seen, is_new],
                )
            })
            .unwrap();
    }

    #[tokio::test]
    async fn new_releases_use_a_window_newest_first_and_a_limit() {
        let (follows, catalog) = setup();
        catalog.set("UC1", &[], &[]);
        follows.follow("UC1").await.unwrap();
        seed(&follows, "UC1", "ten-days", "2026", 10, true);
        seed(&follows, "UC1", "one-day", "2026", 1, true);
        seed(&follows, "UC1", "thirty-days", "2026", 30, true);
        seed(&follows, "UC1", "too-old", "2026", 61, true);
        seed(&follows, "UC1", "not-new", "2026", 2, false);

        let ids = |max| -> Vec<String> {
            follows
                .new_releases(max)
                .unwrap()
                .into_iter()
                .map(|r| r.id)
                .collect()
        };
        assert_eq!(ids(12), ["one-day", "ten-days", "thirty-days"]);
        assert_eq!(ids(2), ["one-day", "ten-days"]);
        assert_eq!(follows.list().unwrap()[0].new_count, 3);
    }

    #[tokio::test]
    async fn list_puts_the_latest_follow_first() {
        let (follows, catalog) = setup();
        catalog.set("UC1", &[], &[]);
        catalog.set("UC2", &[], &[]);
        follows.follow("UC1").await.unwrap();
        follows.follow("UC2").await.unwrap();
        follows
            .inner
            .db
            .with(|c| {
                c.execute(
                    "UPDATE followed_artists SET followed_at = 100 WHERE id = 'UC1'",
                    [],
                )
            })
            .unwrap();
        let ids: Vec<_> = follows.list().unwrap().into_iter().map(|a| a.id).collect();
        assert_eq!(ids, ["UC2", "UC1"]);
    }

    #[tokio::test]
    async fn unfollow_removes_the_artist_and_its_releases() {
        let (follows, catalog) = setup();
        let y = this_year();
        catalog.set("UC1", &[("a", &y)], &[]);
        catalog.set("UC2", &[("b", &y)], &[]);
        follows.follow("UC1").await.unwrap();
        follows.follow("UC2").await.unwrap();
        assert!(follows.unfollow("UC1").unwrap());
        assert!(!follows.unfollow("UC1").unwrap());
        assert!(!follows.is_following("UC1").unwrap());
        let new = follows.new_releases(12).unwrap();
        assert_eq!(new.len(), 1);
        assert_eq!(new[0].artist_id, "UC2");
        let stored: i64 = follows
            .inner
            .db
            .with(|c| {
                c.query_row(
                    "SELECT COUNT(*) FROM artist_releases WHERE artist_id = 'UC1'",
                    [],
                    |r| r.get(0),
                )
            })
            .unwrap();
        assert_eq!(stored, 0);
    }
}
