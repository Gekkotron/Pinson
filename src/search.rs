//! YouTube Music search through `ytmapi-rs`.

use std::path::PathBuf;

use anyhow::{Context, bail};
use async_trait::async_trait;
use tokio::sync::Mutex;
use ytmapi_rs::YtMusic;
use ytmapi_rs::auth::BrowserToken;
use ytmapi_rs::auth::noauth::NoAuthToken;
use ytmapi_rs::common::{Thumbnail, YoutubeID};
use ytmapi_rs::parse::SearchResultSong;

use crate::artwork::upscale;
use crate::song::{Song, parse_duration};

/// Most results returned by one search.
pub const MAX_RESULTS: usize = 20;

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

#[async_trait]
pub trait Searcher: Send + Sync + 'static {
    async fn search(&self, query: &str) -> anyhow::Result<Vec<Song>>;
}

/// Searches YouTube Music. Connects on first use and retries after a failed
/// connect, so the server starts even when YouTube is unreachable.
pub struct YtmSearcher {
    cookie_file: Option<PathBuf>,
    client: Mutex<Option<Client>>,
}

#[derive(Clone)]
enum Client {
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

    async fn client(&self) -> anyhow::Result<Client> {
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
        let results = match self.client().await? {
            Client::Anonymous(yt) => yt.search_songs(query).await?,
            Client::Browser(yt) => yt.search_songs(query).await?,
        };
        Ok(results
            .iter()
            .take(MAX_RESULTS)
            .map(song_from_result)
            .collect())
    }
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

fn map_fields(
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
