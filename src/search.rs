//! YouTube Music search through `ytmapi-rs`.

use std::path::PathBuf;

use anyhow::{Context, bail};
use async_trait::async_trait;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::Mutex;
use ytmapi_rs::YtMusic;
use ytmapi_rs::auth::noauth::NoAuthToken;
use ytmapi_rs::auth::{AuthToken, BrowserToken};
use ytmapi_rs::common::{SearchSuggestion, SuggestionType, TextRun, Thumbnail, YoutubeID};
use ytmapi_rs::parse::SearchResultSong;
use ytmapi_rs::query::SearchQuery;
use ytmapi_rs::query::search::SongsFilter;

use crate::artwork::upscale;
use crate::song::{Song, parse_duration};

/// Most results returned by one search.
pub const MAX_RESULTS: usize = 20;

/// Most suggestions returned for one query.
pub const MAX_SUGGESTIONS: usize = 5;

const YTM_URL: &str = "https://music.youtube.com";
/// The user agent `ytmapi-rs` sends with every API call.
const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:88.0) Gecko/20100101 Firefox/88.0";

/// The `VISITOR_DATA` value from the page's `ytcfg.set({...})`.
fn visitor_id_from_page(page: &str) -> Option<String> {
    let (_, rest) = page.split_once(r#""VISITOR_DATA":""#)?;
    let (id, _) = rest.split_once('"')?;
    (!id.is_empty()).then(|| id.to_owned())
}

/// An anonymous session.
///
/// `ytmapi-rs` 0.3.3 can no longer fetch the visitor id itself: YouTube
/// answers its `X-Origin` header with 400 (an HTTP/2 reset, "error sending
/// request"), and from the EU it redirects to a cookie-consent page. Fetch
/// the page without that header, with the consent cookie, over HTTP/1.1, and
/// hand the token to `ytmapi-rs`.
async fn anonymous_session() -> anyhow::Result<YtMusic<NoAuthToken>> {
    // HTTP/1.1: over HTTP/2 the `h2` crate rejects YouTube's page response
    // for a browser user agent ("unspecific protocol error").
    let http = reqwest::Client::builder()
        .http1_only()
        .build()
        .context("building the HTTP client")?;
    let response = http
        .get(YTM_URL)
        .header("User-Agent", USER_AGENT)
        // Skips the EU cookie-consent redirect, as ytmusicapi does.
        .header("Cookie", "SOCS=CAI")
        .send()
        .await
        .context("fetching music.youtube.com")?;
    let url = response.url();
    if url.path().starts_with("/sorry") {
        bail!("YouTube is rate-limiting this address (google.com/sorry); try again later");
    }
    if url.host_str() == Some("consent.youtube.com") {
        bail!("YouTube asked for cookie consent; use --ytm-cookie-file");
    }
    let status = response.status();
    let page = response.text().await.context("reading music.youtube.com")?;
    let Some(visitor_id) = visitor_id_from_page(&page) else {
        bail!("music.youtube.com answered {status} without a visitor id");
    };
    let token: NoAuthToken = serde_json::from_value(serde_json::json!({
        "create_time": chrono::Utc::now().to_rfc3339(),
        "visitor_id": visitor_id,
    }))
    .context("building the YouTube Music session")?;
    Ok(YtMusic::from_auth_token(token))
}

/// One piece of a suggestion; `bold` marks the part YouTube Music completed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SuggestionRun {
    pub text: String,
    pub bold: bool,
}

/// An autocomplete suggestion. `text` is the runs joined, as YouTube Music
/// sent it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Suggestion {
    pub text: String,
    pub runs: Vec<SuggestionRun>,
}

/// Predictions only, in YouTube Music's order: drops history items, blank
/// ones and repeats, and keeps at most [`MAX_SUGGESTIONS`].
pub fn to_suggestions(found: &[SearchSuggestion]) -> Vec<Suggestion> {
    let mut out: Vec<Suggestion> = Vec::new();
    for item in found {
        if out.len() >= MAX_SUGGESTIONS {
            break;
        }
        if matches!(item.suggestion_type, SuggestionType::History) {
            continue;
        }
        let runs: Vec<SuggestionRun> = item
            .runs
            .iter()
            .map(|run| match run {
                TextRun::Bold(text) => SuggestionRun {
                    text: text.clone(),
                    bold: true,
                },
                TextRun::Normal(text) => SuggestionRun {
                    text: text.clone(),
                    bold: false,
                },
            })
            .collect();
        let text: String = runs.iter().map(|r| r.text.as_str()).collect();
        if text.trim().is_empty() || out.iter().any(|s| s.text == text) {
            continue;
        }
        out.push(Suggestion { text, runs });
    }
    out
}

#[async_trait]
pub trait Searcher: Send + Sync + 'static {
    async fn search(&self, query: &str) -> anyhow::Result<Vec<Song>>;
    async fn suggestions(&self, query: &str) -> anyhow::Result<Vec<Suggestion>>;
}

/// Searches YouTube Music. Connects on first use and retries after a failed
/// connect, so the server starts even when YouTube is unreachable.
pub struct YtmSearcher {
    cookie_file: Option<PathBuf>,
    client: Mutex<Option<Client>>,
}

#[derive(Clone)]
pub(crate) enum Client {
    Anonymous(YtMusic<NoAuthToken>),
    Browser(YtMusic<BrowserToken>),
}

impl YtmSearcher {
    /// Connects now, without auth.
    pub async fn new() -> anyhow::Result<Self> {
        let searcher = Self::lazy(None);
        searcher.client().await?;
        Ok(searcher)
    }

    /// Connects on the first search; with `cookie_file`, uses a browser
    /// cookie (for accounts where anonymous search doesn't work).
    pub fn lazy(cookie_file: Option<PathBuf>) -> Self {
        Self {
            cookie_file,
            client: Mutex::new(None),
        }
    }

    pub(crate) async fn client(&self) -> anyhow::Result<Client> {
        let mut slot = self.client.lock().await;
        if let Some(client) = slot.as_ref() {
            return Ok(client.clone());
        }
        let client = match &self.cookie_file {
            None => Client::Anonymous(anonymous_session().await?),
            Some(path) => Client::Browser(
                YtMusic::from_cookie_file(path)
                    .await
                    .with_context(|| format!("reading YouTube Music cookie {}", path.display()))?,
            ),
        };
        *slot = Some(client.clone());
        Ok(client)
    }
}

#[async_trait]
impl Searcher for YtmSearcher {
    async fn search(&self, query: &str) -> anyhow::Result<Vec<Song>> {
        match self.client().await? {
            Client::Anonymous(yt) => search_songs(&yt, query).await,
            Client::Browser(yt) => search_songs(&yt, query).await,
        }
    }

    async fn suggestions(&self, query: &str) -> anyhow::Result<Vec<Suggestion>> {
        let found = match self.client().await? {
            Client::Anonymous(yt) => yt.get_search_suggestions(query).await?,
            Client::Browser(yt) => yt.get_search_suggestions(query).await?,
        };
        Ok(to_suggestions(&found))
    }
}

/// One page of song results. `ytmapi-rs` fails the whole page when one item
/// doesn't match its layout (0.3.3: an album name that isn't a link), so
/// when it does, read the page leniently and skip only the unreadable items.
async fn search_songs<A: AuthToken>(yt: &YtMusic<A>, query: &str) -> anyhow::Result<Vec<Song>> {
    let query = SearchQuery::new_filtered(query, SongsFilter);
    let json = yt.raw_json_query(query.clone()).await?;
    let songs = match ytmapi_rs::process_json::<_, A>(json.clone(), query) {
        Ok(results) => results.iter().map(song_from_result).collect(),
        Err(e) => {
            let songs = serde_json::from_str(&json)
                .map(|page| songs_from_search_page(&page))
                .unwrap_or_default();
            if songs.is_empty() {
                return Err(e.into());
            }
            tracing::debug!("ytmapi-rs rejected a search page, read it leniently: {e}");
            songs
        }
    };
    Ok(songs.into_iter().take(MAX_RESULTS).collect())
}

/// The songs of a filtered search page, skipping items without a title or
/// video id. Reads only what [`Song`] needs, so it survives layout changes
/// that trip `ytmapi-rs`.
pub fn songs_from_search_page(page: &Value) -> Vec<Song> {
    let sections = page
        .pointer("/contents/tabbedSearchResultsRenderer/tabs/0/tabRenderer/content/sectionListRenderer/contents")
        .and_then(Value::as_array);
    sections
        .into_iter()
        .flatten()
        .filter_map(|section| section.pointer("/musicShelfRenderer/contents")?.as_array())
        .flatten()
        .filter_map(|item| song_from_shelf_item(item.get("musicResponsiveListItemRenderer")?))
        .collect()
}

fn song_from_shelf_item(item: &Value) -> Option<Song> {
    let column_runs = |i: usize| {
        item.pointer(&format!(
            "/flexColumns/{i}/musicResponsiveListItemFlexColumnRenderer/text/runs"
        ))
        .and_then(Value::as_array)
        .map(|runs| {
            runs.iter()
                .filter_map(|r| r.get("text")?.as_str())
                .collect::<String>()
        })
    };
    let title = column_runs(0).filter(|t| !t.is_empty())?;
    let video_id = item.pointer("/playlistItemData/videoId")?.as_str()?;
    // "artist • album • duration" or "artist • duration".
    let byline = column_runs(1).unwrap_or_default();
    let parts: Vec<&str> = byline.split(" • ").collect();
    let artist = parts.first().copied().unwrap_or_default();
    let duration = if parts.len() >= 2 {
        parts[parts.len() - 1]
    } else {
        ""
    };
    let album = (parts.len() >= 3).then(|| parts[1]);
    let thumbnails: Vec<Thumbnail> = item
        .pointer("/thumbnail/musicThumbnailRenderer/thumbnail/thumbnails")
        .and_then(|t| serde_json::from_value(t.clone()).ok())
        .unwrap_or_default();
    Some(map_fields(
        &title,
        artist,
        album,
        duration,
        video_id,
        &thumbnails,
    ))
}

pub fn song_from_result(r: &SearchResultSong) -> Song {
    map_fields(
        &r.title,
        &r.artist,
        r.album.as_ref().map(|a| a.name.as_str()),
        &r.duration,
        r.video_id.get_raw(),
        &r.thumbnails,
    )
}

pub(crate) fn map_fields(
    title: &str,
    artist: &str,
    album: Option<&str>,
    duration: &str,
    video_id: &str,
    thumbnails: &[Thumbnail],
) -> Song {
    Song {
        video_id: video_id.to_owned(),
        title: title.to_owned(),
        artists: artist.to_owned(),
        album: album.map(str::to_owned),
        duration_secs: parse_duration(duration),
        artwork_url: thumbnails
            .iter()
            .max_by_key(|t| t.width * t.height)
            .map(|t| upscale(&t.url)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(runs: &[(&str, bool)], history: bool) -> ytmapi_rs::common::SearchSuggestion {
        let runs: Vec<Value> = runs
            .iter()
            .map(|(t, bold)| serde_json::json!({ if *bold { "Bold" } else { "Normal" }: t }))
            .collect();
        serde_json::from_value(serde_json::json!({
            "runs": runs,
            "suggestion_type": if history { "History" } else { "Prediction" },
        }))
        .unwrap()
    }

    fn plain(text: &str) -> ytmapi_rs::common::SearchSuggestion {
        found(&[(text, false)], false)
    }

    #[test]
    fn suggestions_keep_bold_runs() {
        let got = to_suggestions(&[found(&[("daft pu", true), ("nk", false)], false)]);
        let run = |text: &str, bold| SuggestionRun {
            text: text.into(),
            bold,
        };
        assert_eq!(
            got,
            vec![Suggestion {
                text: "daft punk".into(),
                runs: vec![run("daft pu", true), run("nk", false)],
            }]
        );
    }

    #[test]
    fn suggestions_drop_history_blank_and_repeats() {
        let got = to_suggestions(&[
            plain("a"),
            found(&[("b", false)], true),
            plain(" "),
            plain("a"),
            plain("c"),
        ]);
        let texts: Vec<&str> = got.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(texts, ["a", "c"]);
    }

    #[test]
    fn suggestions_stop_at_five() {
        let many: Vec<_> = (0..8).map(|i| plain(&format!("s{i}"))).collect();
        let got = to_suggestions(&many);
        let texts: Vec<&str> = got.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(texts, ["s0", "s1", "s2", "s3", "s4"]);
    }

    #[test]
    fn suggestions_keep_text_verbatim() {
        let raw = "AC/DC & <b>\"x\"";
        let got = to_suggestions(&[plain(raw)]);
        assert_eq!(got[0].text, raw);
    }

    #[tokio::test]
    #[ignore = "hits YouTube Music"]
    async fn live_suggestions_return_predictions() {
        let searcher = YtmSearcher::new().await.expect("connect");
        let got = searcher.suggestions("daft pu").await.unwrap();
        assert!((1..=5).contains(&got.len()), "{got:?}");
        assert!(got.iter().all(|s| !s.text.trim().is_empty()), "{got:?}");
        assert!(got.iter().any(|s| s.runs.iter().any(|r| r.bold)), "{got:?}");
    }

    #[test]
    fn visitor_id_is_read_from_the_page() {
        let page = r#"<script>ytcfg.set({"INNERTUBE_API_KEY":"k","VISITOR_DATA":"CgtYYz1fUjVfRGxkayi1","GL":"FR"});</script>"#;
        assert_eq!(
            visitor_id_from_page(page).as_deref(),
            Some("CgtYYz1fUjVfRGxkayi1")
        );
        assert_eq!(visitor_id_from_page("<html>consent</html>"), None);
        assert_eq!(visitor_id_from_page(r#""VISITOR_DATA":"""#), None);
    }

    fn thumb(w: u64, url: &str) -> Thumbnail {
        Thumbnail {
            height: w,
            width: w,
            url: url.into(),
        }
    }

    #[test]
    fn maps_search_result_song() {
        let thumbs = [
            thumb(60, "small"),
            thumb(
                544,
                "https://lh3.googleusercontent.com/abc=w544-h544-l90-rj",
            ),
            thumb(120, "mid"),
        ];
        let song = map_fields(
            "One More Time",
            "Daft Punk",
            Some("Discovery"),
            "5:21",
            "FGBhQbmPwH8",
            &thumbs,
        );
        assert_eq!(
            song,
            Song {
                video_id: "FGBhQbmPwH8".into(),
                title: "One More Time".into(),
                artists: "Daft Punk".into(),
                album: Some("Discovery".into()),
                duration_secs: Some(321),
                artwork_url: Some(
                    "https://lh3.googleusercontent.com/abc=w1200-h1200-l90-rj".into()
                ),
            }
        );
    }

    #[test]
    fn maps_missing_fields_to_none() {
        let song = map_fields("T", "A", None, "", "id", &[]);
        assert_eq!(song.album, None);
        assert_eq!(song.duration_secs, None);
        assert_eq!(song.artwork_url, None);
    }

    fn run(text: &str, browse_id: Option<&str>) -> serde_json::Value {
        match browse_id {
            Some(id) => serde_json::json!({
                "text": text,
                "navigationEndpoint": {"browseEndpoint": {"browseId": id}},
            }),
            None => serde_json::json!({"text": text}),
        }
    }

    fn song_item(
        title: &str,
        byline: Vec<serde_json::Value>,
        video_id: Option<&str>,
    ) -> serde_json::Value {
        let column = |runs: serde_json::Value| serde_json::json!({"musicResponsiveListItemFlexColumnRenderer": {"text": {"runs": runs}}});
        let mut item = serde_json::json!({
            "thumbnail": {"musicThumbnailRenderer": {"thumbnail": {"thumbnails": [
                {"url": "https://lh3.googleusercontent.com/x=w120-h120-l90-rj", "width": 120, "height": 120},
            ]}}},
            "flexColumns": [
                column(serde_json::json!([{"text": title}])),
                column(serde_json::Value::Array(byline)),
                column(serde_json::json!([{"text": "1.2M plays"}])),
            ],
        });
        if let Some(id) = video_id {
            item["playlistItemData"] = serde_json::json!({"videoId": id});
        }
        serde_json::json!({"musicResponsiveListItemRenderer": item})
    }

    /// A filtered song search page; the second song's album isn't a link,
    /// which `ytmapi-rs` 0.3.3 rejects for the whole page.
    fn page_with_unlinked_album() -> serde_json::Value {
        let sep = || run(" • ", None);
        serde_json::json!({"contents": {"tabbedSearchResultsRenderer": {"tabs": [{"tabRenderer": {
            "content": {"sectionListRenderer": {"contents": [
                {"itemSectionRenderer": {"contents": []}},
                {"musicShelfRenderer": {"contents": [
                    song_item(
                        "One More Time",
                        vec![run("Daft Punk", Some("UCdaft")), sep(), run("Discovery", Some("MPREb_disc")), sep(), run("5:21", None)],
                        Some("FGBhQbmPwH8"),
                    ),
                    song_item(
                        "Get Lucky",
                        vec![run("Daft Punk", Some("UCdaft")), run(" & ", None), run("Pharrell", Some("UCphar")), sep(), run("Live Session", None), sep(), run("6:09", None)],
                        Some("5NV6Rdv1a3I"),
                    ),
                    song_item("Unavailable", vec![run("Nobody", None), sep(), run("1:00", None)], None),
                ]}},
            ]}},
        }}]}}})
    }

    #[test]
    fn ytmapi_rejects_a_page_with_an_unlinked_album() {
        let query = SearchQuery::new_filtered("daft punk", SongsFilter);
        let err = ytmapi_rs::process_json::<_, NoAuthToken>(
            page_with_unlinked_album().to_string(),
            query,
        )
        .expect_err("ytmapi-rs 0.3.3 fails on this page; drop the fallback once it doesn't");
        assert!(err.to_string().contains("2 or 3 string fields"), "{err}");
    }

    #[test]
    fn lenient_parse_keeps_songs_with_an_unlinked_album() {
        let songs = songs_from_search_page(&page_with_unlinked_album());
        let ids: Vec<_> = songs.iter().map(|s| s.video_id.as_str()).collect();
        assert_eq!(
            ids,
            ["FGBhQbmPwH8", "5NV6Rdv1a3I"],
            "skips the item without a video id"
        );
        assert_eq!(songs[0].album.as_deref(), Some("Discovery"));
        let lucky = &songs[1];
        assert_eq!(lucky.title, "Get Lucky");
        assert_eq!(lucky.artists, "Daft Punk & Pharrell");
        assert_eq!(lucky.album.as_deref(), Some("Live Session"));
        assert_eq!(lucky.duration_secs, Some(369));
        assert_eq!(
            lucky.artwork_url.as_deref(),
            Some("https://lh3.googleusercontent.com/x=w1200-h1200-l90-rj")
        );
    }

    #[test]
    fn lenient_parse_of_an_unknown_page_is_empty() {
        assert!(songs_from_search_page(&serde_json::json!({"error": "nope"})).is_empty());
    }

    #[tokio::test]
    #[ignore = "hits YouTube Music"]
    async fn live_search_returns_songs() {
        let searcher = YtmSearcher::new().await.expect("connect to YouTube Music");
        let songs = searcher.search("daft punk").await.expect("search");
        assert!(!songs.is_empty());
        assert!(songs.len() <= MAX_RESULTS);
        assert!(songs.iter().all(|s| !s.video_id.is_empty()));
        eprintln!("{:#?}", songs[0]);
    }
}
