//! YouTube Music radios: the endless list of songs "like" a given song.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::time::Instant;
use ytmapi_rs::common::{VideoID, YoutubeID};
use ytmapi_rs::parse::WatchPlaylistTrack;

use crate::search::{Client, YtmSearcher, map_fields};
use crate::song::Song;

/// How long a fetched radio is served from memory.
pub const RADIO_TTL: Duration = Duration::from_secs(3 * 60 * 60);

#[async_trait]
pub trait Radio: Send + Sync + 'static {
    /// The radio for `video_id`, in YouTube Music's order (usually starts
    /// with the seed itself).
    async fn radio(&self, video_id: &str) -> anyhow::Result<Vec<Song>>;
}

pub fn watch_track_song(t: &WatchPlaylistTrack) -> Song {
    map_fields(
        &t.title,
        &t.author,
        None,
        &t.duration,
        t.video_id.get_raw(),
        &t.thumbnails,
    )
}

#[async_trait]
impl Radio for YtmSearcher {
    async fn radio(&self, video_id: &str) -> anyhow::Result<Vec<Song>> {
        let tracks = match self.client().await? {
            Client::Anonymous(yt) => {
                yt.get_watch_playlist_from_video_id(VideoID::from_raw(video_id))
                    .await?
            }
            Client::Browser(yt) => {
                yt.get_watch_playlist_from_video_id(VideoID::from_raw(video_id))
                    .await?
            }
        };
        Ok(tracks.iter().map(watch_track_song).collect())
    }
}

/// Remembers each radio for `ttl`. Failures are not remembered.
pub struct RadioCache {
    inner: Arc<dyn Radio>,
    ttl: Duration,
    entries: Mutex<HashMap<String, (Instant, Vec<Song>)>>,
}

impl RadioCache {
    pub fn new(inner: Arc<dyn Radio>, ttl: Duration) -> RadioCache {
        RadioCache {
            inner,
            ttl,
            entries: Mutex::new(HashMap::new()),
        }
    }
}

#[async_trait]
impl Radio for RadioCache {
    async fn radio(&self, video_id: &str) -> anyhow::Result<Vec<Song>> {
        {
            let entries = self.entries.lock().unwrap();
            if let Some((at, songs)) = entries.get(video_id)
                && at.elapsed() < self.ttl
            {
                return Ok(songs.clone());
            }
        }
        let songs = self.inner.radio(video_id).await?;
        self.entries
            .lock()
            .unwrap()
            .insert(video_id.to_owned(), (Instant::now(), songs.clone()));
        Ok(songs)
    }
}

/// A scriptable radio for tests.
pub mod fake {
    use super::*;

    #[derive(Default)]
    struct State {
        songs: HashMap<String, Vec<Song>>,
        failing: std::collections::HashSet<String>,
        delays: HashMap<String, Duration>,
        calls: Vec<String>,
    }

    /// Answers from what `set` was given; an id never set gives no songs.
    #[derive(Clone, Default)]
    pub struct FakeRadio {
        state: Arc<Mutex<State>>,
    }

    impl FakeRadio {
        pub fn set(&self, video_id: &str, songs: Vec<Song>) {
            let mut s = self.state.lock().unwrap();
            s.failing.remove(video_id);
            s.songs.insert(video_id.to_owned(), songs);
        }

        pub fn fail(&self, video_id: &str) {
            self.state
                .lock()
                .unwrap()
                .failing
                .insert(video_id.to_owned());
        }

        pub fn delay(&self, video_id: &str, d: Duration) {
            self.state
                .lock()
                .unwrap()
                .delays
                .insert(video_id.to_owned(), d);
        }

        /// Every `radio` call, in order.
        pub fn calls(&self) -> Vec<String> {
            self.state.lock().unwrap().calls.clone()
        }
    }

    #[async_trait]
    impl Radio for FakeRadio {
        async fn radio(&self, video_id: &str) -> anyhow::Result<Vec<Song>> {
            let delay = {
                let mut s = self.state.lock().unwrap();
                s.calls.push(video_id.to_owned());
                s.delays.get(video_id).copied()
            };
            if let Some(d) = delay {
                tokio::time::sleep(d).await;
            }
            let s = self.state.lock().unwrap();
            if s.failing.contains(video_id) {
                anyhow::bail!("radio for {video_id} failed");
            }
            Ok(s.songs.get(video_id).cloned().unwrap_or_default())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::FakeRadio;
    use super::*;

    fn song(id: &str) -> Song {
        Song {
            video_id: id.into(),
            title: format!("t{id}"),
            artists: "a".into(),
            album: None,
            duration_secs: Some(60),
            artwork_url: None,
        }
    }

    #[test]
    fn watch_track_maps_to_song() {
        let track: WatchPlaylistTrack = serde_json::from_value(serde_json::json!({
            "title": "One More Time",
            "author": "Daft Punk",
            "duration": "3:45",
            "thumbnails": [
                {"height": 60, "width": 60, "url": "https://lh3.googleusercontent.com/abc=w60-h60"},
                {"height": 544, "width": 544, "url": "https://lh3.googleusercontent.com/abc=w544-h544"}
            ],
            "video_id": "FGBhQbmPwH8"
        }))
        .unwrap();
        assert_eq!(
            watch_track_song(&track),
            Song {
                video_id: "FGBhQbmPwH8".into(),
                title: "One More Time".into(),
                artists: "Daft Punk".into(),
                album: None,
                duration_secs: Some(225),
                artwork_url: Some("https://lh3.googleusercontent.com/abc=w1200-h1200".into()),
            }
        );
    }

    #[tokio::test(start_paused = true)]
    async fn cache_serves_repeat_requests() {
        let inner = FakeRadio::default();
        inner.set("A", vec![song("A"), song("x")]);
        let cache = RadioCache::new(Arc::new(inner.clone()), RADIO_TTL);
        let first = cache.radio("A").await.unwrap();
        let second = cache.radio("A").await.unwrap();
        assert_eq!(first, second);
        assert_eq!(inner.calls(), ["A"]);
    }

    #[tokio::test(start_paused = true)]
    async fn cache_expires_after_ttl() {
        let inner = FakeRadio::default();
        let cache = RadioCache::new(Arc::new(inner.clone()), RADIO_TTL);
        cache.radio("A").await.unwrap();
        tokio::time::sleep(RADIO_TTL + Duration::from_secs(1)).await;
        cache.radio("A").await.unwrap();
        assert_eq!(inner.calls(), ["A", "A"]);
    }

    #[tokio::test(start_paused = true)]
    async fn cache_does_not_keep_failures() {
        let inner = FakeRadio::default();
        inner.fail("A");
        let cache = RadioCache::new(Arc::new(inner.clone()), RADIO_TTL);
        assert!(cache.radio("A").await.is_err());
        inner.set("A", vec![song("A")]);
        assert_eq!(cache.radio("A").await.unwrap(), vec![song("A")]);
        assert_eq!(inner.calls(), ["A", "A"]);
    }

    #[tokio::test]
    #[ignore = "network"]
    async fn live_radio_returns_songs() {
        let songs = YtmSearcher::lazy(None).radio("FGBhQbmPwH8").await.unwrap();
        assert!(songs.len() >= 10, "got {}", songs.len());
        assert!(songs.iter().all(|s| !s.video_id.is_empty()));
    }
}
