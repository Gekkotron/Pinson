//! Lyrics for a song: synced from LRCLIB, else plain from LRCLIB, else plain
//! from YouTube Music (see `docs/api.md`).

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use reqwest::Url;
use serde::{Deserialize, Serialize};

use crate::catalog::Catalog;

/// Most songs whose lyrics (or lack of them) are remembered.
const CACHE_ENTRIES: usize = 200;
/// How far an LRCLIB record's length may differ from the song's.
const DURATION_TOLERANCE_SECS: f64 = 3.0;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LyricLine {
    pub time_ms: u64,
    pub text: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LyricsSource {
    Lrclib,
    Youtube,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Lyrics {
    pub video_id: String,
    pub source: LyricsSource,
    pub synced: bool,
    pub lines: Vec<LyricLine>,
    pub plain: Option<String>,
    pub instrumental: bool,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LyricsQuery {
    pub video_id: String,
    pub title: String,
    pub artists: String,
    pub album: Option<String>,
    pub duration_secs: Option<u32>,
}

/// One `[mm:ss.xx]` stamp at the start of `s`: (milliseconds, rest).
fn take_stamp(s: &str) -> Option<(u64, &str)> {
    let s = s.strip_prefix('[')?;
    let (stamp, rest) = s.split_once(']')?;
    let (min, secs) = stamp.split_once(':')?;
    let (secs, frac) = match secs.split_once('.') {
        Some((s, f)) => (s, f),
        None => (secs, ""),
    };
    let digits = |t: &str| !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit());
    if !digits(min) || !digits(secs) || !(frac.is_empty() || digits(frac)) {
        return None;
    }
    // The fraction is a decimal: ".5" is 500 ms, ".345" is 345 ms.
    let millis: u64 = frac
        .chars()
        .chain("000".chars())
        .take(3)
        .collect::<String>()
        .parse()
        .ok()?;
    let total = min.parse::<u64>().ok()? * 60_000 + secs.parse::<u64>().ok()? * 1000 + millis;
    Some((total, rest))
}

/// Parses LRC text. Lines without a valid time stamp (tags, noise) are
/// dropped; a line with several stamps yields one entry per stamp; the result
/// is sorted by time.
pub fn parse_lrc(text: &str) -> Vec<LyricLine> {
    let mut lines = Vec::new();
    for raw in text.lines() {
        let mut rest = raw.trim_start();
        let mut times = Vec::new();
        while let Some((ms, after)) = take_stamp(rest) {
            times.push(ms);
            rest = after;
        }
        let line_text = rest.trim();
        lines.extend(times.into_iter().map(|time_ms| LyricLine {
            time_ms,
            text: line_text.to_owned(),
        }));
    }
    lines.sort_by_key(|l| l.time_ms);
    lines
}

/// The first credited artist: the text before `", "`, `" & "`, `" feat. "` or `" x "`.
pub fn primary_artist(artists: &str) -> &str {
    let end = [", ", " & ", " feat. ", " x "]
        .iter()
        .filter_map(|sep| artists.find(sep))
        .min()
        .unwrap_or(artists.len());
    artists[..end].trim()
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LrcRecord {
    pub duration: Option<f64>,
    #[serde(default)]
    pub instrumental: bool,
    pub plain_lyrics: Option<String>,
    pub synced_lyrics: Option<String>,
}

#[async_trait]
pub trait LrcLibApi: Send + Sync + 'static {
    /// The record LRCLIB matches exactly, `None` on 404.
    async fn get(&self, q: &LyricsQuery) -> anyhow::Result<Option<LrcRecord>>;
    async fn search(&self, q: &LyricsQuery) -> anyhow::Result<Vec<LrcRecord>>;
}

pub struct LrcLib {
    base_url: String,
    http: reqwest::Client,
}

impl LrcLib {
    pub fn new(base_url: impl Into<String>) -> Self {
        let http = reqwest::Client::builder()
            .user_agent(concat!(
                "Music/",
                env!("CARGO_PKG_VERSION"),
                " (https://github.com/Gekkotron)"
            ))
            .timeout(Duration::from_secs(8))
            .build()
            .expect("build http client");
        Self {
            base_url: base_url.into(),
            http,
        }
    }

    fn url(&self, path: &str, params: &[(&str, String)]) -> anyhow::Result<Url> {
        let mut url = Url::parse(&format!("{}{path}", self.base_url.trim_end_matches('/')))?;
        url.query_pairs_mut()
            .extend_pairs(params.iter().map(|(k, v)| (*k, v.as_str())));
        Ok(url)
    }
}

#[async_trait]
impl LrcLibApi for LrcLib {
    async fn get(&self, q: &LyricsQuery) -> anyhow::Result<Option<LrcRecord>> {
        let mut params = vec![
            ("track_name", q.title.clone()),
            ("artist_name", primary_artist(&q.artists).to_owned()),
        ];
        if let Some(album) = &q.album {
            params.push(("album_name", album.clone()));
        }
        if let Some(d) = q.duration_secs {
            params.push(("duration", d.to_string()));
        }
        let res = self.http.get(self.url("/api/get", &params)?).send().await?;
        if res.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let res = res.error_for_status()?;
        Ok(Some(serde_json::from_slice(&res.bytes().await?)?))
    }

    async fn search(&self, q: &LyricsQuery) -> anyhow::Result<Vec<LrcRecord>> {
        let params = [
            ("track_name", q.title.clone()),
            ("artist_name", primary_artist(&q.artists).to_owned()),
        ];
        let res = self
            .http
            .get(self.url("/api/search", &params)?)
            .send()
            .await?
            .error_for_status()?;
        Ok(serde_json::from_slice(&res.bytes().await?)?)
    }
}

#[derive(Default)]
struct Cache {
    entries: HashMap<String, Option<Lyrics>>,
    order: VecDeque<String>,
}

pub struct LyricsService {
    lrclib: Arc<dyn LrcLibApi>,
    catalog: Arc<dyn Catalog>,
    cache: Mutex<Cache>,
}

/// What a record can show: instrumental, synced lines, or plain text.
fn lyrics_from_record(video_id: &str, rec: &LrcRecord) -> Option<Lyrics> {
    let base = |synced, lines, plain, instrumental| Lyrics {
        video_id: video_id.to_owned(),
        source: LyricsSource::Lrclib,
        synced,
        lines,
        plain,
        instrumental,
    };
    let plain = rec.plain_lyrics.clone().filter(|p| !p.trim().is_empty());
    if rec.instrumental {
        return Some(base(false, Vec::new(), None, true));
    }
    if let Some(synced) = &rec.synced_lyrics {
        let lines = parse_lrc(synced);
        if !lines.is_empty() {
            return Some(base(true, lines, plain, false));
        }
    }
    plain.map(|p| base(false, Vec::new(), Some(p), false))
}

fn duration_matches(rec: &LrcRecord, song_secs: Option<u32>) -> bool {
    match (song_secs, rec.duration) {
        (Some(song), Some(rec)) => (rec - f64::from(song)).abs() <= DURATION_TOLERANCE_SECS,
        _ => true,
    }
}

impl LyricsService {
    pub fn new(lrclib: Arc<dyn LrcLibApi>, catalog: Arc<dyn Catalog>) -> Self {
        Self {
            lrclib,
            catalog,
            cache: Mutex::new(Cache::default()),
        }
    }

    /// `None` when no source has lyrics. Errors are not cached.
    pub async fn lyrics(&self, q: &LyricsQuery) -> anyhow::Result<Option<Lyrics>> {
        if let Some(hit) = self.cache.lock().unwrap().entries.get(&q.video_id) {
            return Ok(hit.clone());
        }
        let found = self.resolve(q).await?;
        let mut cache = self.cache.lock().unwrap();
        if cache
            .entries
            .insert(q.video_id.clone(), found.clone())
            .is_none()
        {
            cache.order.push_back(q.video_id.clone());
            if cache.order.len() > CACHE_ENTRIES
                && let Some(oldest) = cache.order.pop_front()
            {
                cache.entries.remove(&oldest);
            }
        }
        Ok(found)
    }

    async fn resolve(&self, q: &LyricsQuery) -> anyhow::Result<Option<Lyrics>> {
        if let Some(rec) = self.lrclib.get(q).await?
            && duration_matches(&rec, q.duration_secs)
            && let Some(found) = lyrics_from_record(&q.video_id, &rec)
        {
            return Ok(Some(found));
        }
        // Not found, or a different cut of the song: look through the search.
        for rec in self.lrclib.search(q).await? {
            if duration_matches(&rec, q.duration_secs)
                && let Some(found) = lyrics_from_record(&q.video_id, &rec)
            {
                return Ok(Some(found));
            }
        }
        Ok(self
            .catalog
            .youtube_lyrics(&q.video_id)
            .await?
            .filter(|t| !t.trim().is_empty())
            .map(|plain| Lyrics {
                video_id: q.video_id.clone(),
                source: LyricsSource::Youtube,
                synced: false,
                lines: Vec::new(),
                plain: Some(plain),
                instrumental: false,
            }))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::catalog::{Collection, CollectionKind, CollectionSummary};

    fn line(time_ms: u64, text: &str) -> LyricLine {
        LyricLine {
            time_ms,
            text: text.into(),
        }
    }

    #[test]
    fn parse_lrc_basic_and_sorted() {
        let l = parse_lrc("[ar:Daft Punk]\n[00:12.34]One more time\n[00:05.00]Intro\n");
        assert_eq!(l, vec![line(5000, "Intro"), line(12340, "One more time")]);
    }

    #[test]
    fn parse_lrc_multiple_stamps_and_ms_precision() {
        let l = parse_lrc("[00:01.5][01:02.345]Hey");
        assert_eq!(l, vec![line(1500, "Hey"), line(62345, "Hey")]);
    }

    #[test]
    fn parse_lrc_keeps_empty_lines_as_spacing_and_skips_garbage() {
        let l = parse_lrc("[00:03.00]\n hello\n[xx:yy]bad");
        assert_eq!(l, vec![line(3000, "")]);
    }

    #[test]
    fn primary_artist_splits() {
        assert_eq!(primary_artist("Daft Punk, Pharrell Williams"), "Daft Punk");
        assert_eq!(primary_artist("Daft Punk"), "Daft Punk");
        assert_eq!(primary_artist("A & B"), "A");
        assert_eq!(primary_artist("A feat. B, C"), "A");
        assert_eq!(primary_artist("A x B"), "A");
    }

    #[derive(Default)]
    struct FakeLrc {
        gets: Mutex<VecDeque<anyhow::Result<Option<LrcRecord>>>>,
        searches: Mutex<VecDeque<anyhow::Result<Vec<LrcRecord>>>>,
        get_calls: AtomicUsize,
        search_calls: AtomicUsize,
    }

    #[async_trait]
    impl LrcLibApi for FakeLrc {
        async fn get(&self, _q: &LyricsQuery) -> anyhow::Result<Option<LrcRecord>> {
            self.get_calls.fetch_add(1, Ordering::SeqCst);
            self.gets.lock().unwrap().pop_front().unwrap_or(Ok(None))
        }
        async fn search(&self, _q: &LyricsQuery) -> anyhow::Result<Vec<LrcRecord>> {
            self.search_calls.fetch_add(1, Ordering::SeqCst);
            self.searches
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Ok(Vec::new()))
        }
    }

    #[derive(Default)]
    struct FakeCatalog {
        text: Option<String>,
        calls: AtomicUsize,
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
        async fn youtube_lyrics(&self, _: &str) -> anyhow::Result<Option<String>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.text.clone())
        }
    }

    fn rec(
        duration: f64,
        synced: Option<&str>,
        plain: Option<&str>,
        instrumental: bool,
    ) -> LrcRecord {
        LrcRecord {
            duration: Some(duration),
            instrumental,
            plain_lyrics: plain.map(Into::into),
            synced_lyrics: synced.map(Into::into),
        }
    }

    fn query(duration: Option<u32>) -> LyricsQuery {
        LyricsQuery {
            video_id: "v1".into(),
            title: "One More Time".into(),
            artists: "Daft Punk".into(),
            album: None,
            duration_secs: duration,
        }
    }

    fn service(lrc: &Arc<FakeLrc>, cat: &Arc<FakeCatalog>) -> LyricsService {
        LyricsService::new(lrc.clone(), cat.clone())
    }

    #[tokio::test]
    async fn synced_from_get_wins() {
        let lrc = Arc::new(FakeLrc::default());
        lrc.gets.lock().unwrap().push_back(Ok(Some(rec(
            200.0,
            Some("[00:01.00]Hi\n[00:02.00]There"),
            Some("Hi\nThere"),
            false,
        ))));
        let cat = Arc::new(FakeCatalog::default());
        let l = service(&lrc, &cat)
            .lyrics(&query(Some(200)))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(l.source, LyricsSource::Lrclib);
        assert!(l.synced);
        assert_eq!(l.lines, vec![line(1000, "Hi"), line(2000, "There")]);
        assert_eq!(lrc.search_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn get_with_duration_off_by_more_than_3s_falls_to_search() {
        let lrc = Arc::new(FakeLrc::default());
        lrc.gets.lock().unwrap().push_back(Ok(Some(rec(
            210.0,
            Some("[00:01.00]Long"),
            None,
            false,
        ))));
        lrc.searches.lock().unwrap().push_back(Ok(vec![
            rec(230.0, Some("[00:01.00]Far"), None, false),
            rec(202.5, Some("[00:01.00]Close"), None, false),
            rec(200.0, Some("[00:01.00]Later"), None, false),
        ]));
        let cat = Arc::new(FakeCatalog::default());
        let l = service(&lrc, &cat)
            .lyrics(&query(Some(200)))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(l.lines, vec![line(1000, "Close")]);
        assert_eq!(lrc.search_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn lrclib_plain_beats_youtube() {
        let lrc = Arc::new(FakeLrc::default());
        lrc.gets
            .lock()
            .unwrap()
            .push_back(Ok(Some(rec(200.0, None, Some("la la"), false))));
        let cat = Arc::new(FakeCatalog {
            text: Some("yt".into()),
            ..Default::default()
        });
        let l = service(&lrc, &cat)
            .lyrics(&query(Some(200)))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(l.source, LyricsSource::Lrclib);
        assert!(!l.synced);
        assert_eq!(l.plain.as_deref(), Some("la la"));
        assert_eq!(cat.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn youtube_used_when_lrclib_has_nothing() {
        let lrc = Arc::new(FakeLrc::default());
        let cat = Arc::new(FakeCatalog {
            text: Some("from yt".into()),
            ..Default::default()
        });
        let l = service(&lrc, &cat)
            .lyrics(&query(Some(200)))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(l.source, LyricsSource::Youtube);
        assert!(!l.synced);
        assert_eq!(l.plain.as_deref(), Some("from yt"));
    }

    #[tokio::test]
    async fn instrumental() {
        let lrc = Arc::new(FakeLrc::default());
        lrc.gets
            .lock()
            .unwrap()
            .push_back(Ok(Some(rec(200.0, None, None, true))));
        let cat = Arc::new(FakeCatalog::default());
        let l = service(&lrc, &cat)
            .lyrics(&query(Some(200)))
            .await
            .unwrap()
            .unwrap();
        assert!(l.instrumental);
        assert!(l.lines.is_empty());
        assert_eq!(l.plain, None);
    }

    #[tokio::test]
    async fn none_anywhere_is_none_and_cached() {
        let lrc = Arc::new(FakeLrc::default());
        let cat = Arc::new(FakeCatalog::default());
        let svc = service(&lrc, &cat);
        assert!(svc.lyrics(&query(None)).await.unwrap().is_none());
        assert!(svc.lyrics(&query(None)).await.unwrap().is_none());
        assert_eq!(lrc.get_calls.load(Ordering::SeqCst), 1);
        assert_eq!(cat.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn errors_are_not_cached() {
        let lrc = Arc::new(FakeLrc::default());
        lrc.gets
            .lock()
            .unwrap()
            .push_back(Err(anyhow::anyhow!("lrclib down")));
        lrc.gets
            .lock()
            .unwrap()
            .push_back(Ok(Some(rec(200.0, None, Some("back"), false))));
        let cat = Arc::new(FakeCatalog::default());
        let svc = service(&lrc, &cat);
        assert!(svc.lyrics(&query(Some(200))).await.is_err());
        let l = svc.lyrics(&query(Some(200))).await.unwrap();
        assert_eq!(l.unwrap().plain.as_deref(), Some("back"));
    }

    #[tokio::test]
    async fn cache_evicts_oldest_beyond_limit() {
        let lrc = Arc::new(FakeLrc::default());
        let cat = Arc::new(FakeCatalog::default());
        let svc = service(&lrc, &cat);
        for i in 0..=CACHE_ENTRIES {
            let mut q = query(None);
            q.video_id = format!("v{i}");
            svc.lyrics(&q).await.unwrap();
        }
        let calls = lrc.get_calls.load(Ordering::SeqCst);
        let mut q = query(None);
        q.video_id = "v0".into();
        svc.lyrics(&q).await.unwrap();
        assert_eq!(lrc.get_calls.load(Ordering::SeqCst), calls + 1);
    }

    #[tokio::test]
    async fn lrclib_http_against_local_server() {
        use axum::extract::Query;
        use axum::http::{HeaderMap, StatusCode};
        use axum::routing::get;
        use axum::{Json, Router};
        use serde_json::json;

        async fn get_h(
            Query(p): Query<HashMap<String, String>>,
            headers: HeaderMap,
        ) -> Result<Json<serde_json::Value>, StatusCode> {
            assert!(
                headers["user-agent"]
                    .to_str()
                    .unwrap()
                    .starts_with("Music/")
            );
            if p["track_name"] == "Missing" {
                return Err(StatusCode::NOT_FOUND);
            }
            assert_eq!(p["artist_name"], "Daft Punk");
            assert_eq!(p["duration"], "320");
            Ok(Json(json!({
                "id": 1, "trackName": "x", "artistName": "y", "albumName": "z",
                "duration": 320.0, "instrumental": false,
                "plainLyrics": "plain", "syncedLyrics": "[00:01.00]hi"
            })))
        }
        async fn search_h(Query(p): Query<HashMap<String, String>>) -> Json<serde_json::Value> {
            assert_eq!(p["artist_name"], "Daft Punk");
            Json(json!([{
                "id": 2, "duration": 319.0, "instrumental": false,
                "plainLyrics": null, "syncedLyrics": null
            }]))
        }
        let app = Router::new()
            .route("/api/get", get(get_h))
            .route("/api/search", get(search_h));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let api = LrcLib::new(base);
        let mut q = query(Some(320));
        q.artists = "Daft Punk, Pharrell Williams".into();
        let found = api.get(&q).await.unwrap().unwrap();
        assert_eq!(found.synced_lyrics.as_deref(), Some("[00:01.00]hi"));
        assert_eq!(found.duration, Some(320.0));
        q.title = "Missing".into();
        assert!(api.get(&q).await.unwrap().is_none());
        let found = api.search(&q).await.unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].plain_lyrics, None);
    }
}
