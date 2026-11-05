//! YouTube Music search through `ytmapi-rs`.

use std::path::PathBuf;

use anyhow::Context;
use async_trait::async_trait;
use tokio::sync::Mutex;
use ytmapi_rs::YtMusic;
use ytmapi_rs::auth::BrowserToken;
use ytmapi_rs::auth::noauth::NoAuthToken;
use ytmapi_rs::common::{Thumbnail, YoutubeID};
use ytmapi_rs::parse::SearchResultSong;

use crate::song::{Song, parse_duration};

/// Most results returned by one search.
pub const MAX_RESULTS: usize = 20;

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
            None => Client::Anonymous(YtMusic::new_unauthenticated().await?),
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
            .map(|t| t.url.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn thumb(w: u64, url: &str) -> Thumbnail {
        Thumbnail {
            height: w,
            width: w,
            url: url.into(),
        }
    }

    #[test]
    fn maps_search_result_song() {
        let thumbs = [thumb(60, "small"), thumb(544, "large"), thumb(120, "mid")];
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
                artwork_url: Some("large".into()),
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
