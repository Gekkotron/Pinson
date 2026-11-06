//! Albums, playlists and lyrics from YouTube Music (see `docs/api.md`).
//!
//! Album ids (`MPREb_…`) are passed to `ytmapi-rs` as the search returns them.
//! `get_playlist_tracks` and `get_playlist_details` send the id as a browse
//! id, so they need the `VL` form (`VLPL…`), which is also what playlist
//! search returns (verified live). [`api_playlist_id`] strips the prefix and
//! `browse_playlist_id` adds it back, so clients may send either form.

use async_trait::async_trait;
use futures_util::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use ytmapi_rs::common::{AlbumID, PlaylistID, Thumbnail, YoutubeID};
use ytmapi_rs::parse::{GetAlbum, PlaylistItem, SearchResultAlbum, SearchResultPlaylist};
use ytmapi_rs::query::GetPlaylistTracksQuery;

use crate::artwork::upscale;
use crate::search::{Client, MAX_RESULTS, YtmSearcher, map_fields};
use crate::song::Song;

/// Most songs kept from one YouTube playlist.
pub const MAX_PLAYLIST_SONGS: usize = 200;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CollectionKind {
    Album,
    Playlist,
    Mix,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CollectionSummary {
    pub kind: CollectionKind,
    pub id: String,
    pub title: String,
    pub subtitle: Option<String>,
    pub artwork_url: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Collection {
    #[serde(flatten)]
    pub summary: CollectionSummary,
    pub songs: Vec<Song>,
}

#[async_trait]
pub trait Catalog: Send + Sync + 'static {
    async fn search_collections(
        &self,
        query: &str,
        kind: CollectionKind,
    ) -> anyhow::Result<Vec<CollectionSummary>>;
    async fn album(&self, id: &str) -> anyhow::Result<Option<Collection>>;
    async fn playlist(&self, id: &str) -> anyhow::Result<Option<Collection>>;
    /// Plain lyrics of a song, `None` when YouTube has none.
    async fn youtube_lyrics(&self, video_id: &str) -> anyhow::Result<Option<String>>;
}

/// The id `ytmapi-rs` wants for a playlist: browse ids start with `VL`.
pub fn api_playlist_id(raw: &str) -> &str {
    raw.strip_prefix("VL").unwrap_or(raw)
}

/// The browse id (`VL` + playlist id) that `ytmapi-rs` needs to open a playlist.
fn browse_playlist_id(raw: &str) -> String {
    format!("VL{}", api_playlist_id(raw))
}

fn best_artwork(thumbnails: &[Thumbnail]) -> Option<String> {
    thumbnails
        .iter()
        .max_by_key(|t| t.width * t.height)
        .map(|t| upscale(&t.url))
}

fn join_nonblank(parts: &[&str]) -> Option<String> {
    let parts: Vec<&str> = parts
        .iter()
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .collect();
    (!parts.is_empty()).then(|| parts.join(" \u{b7} "))
}

fn album_summary(r: &SearchResultAlbum) -> CollectionSummary {
    CollectionSummary {
        kind: CollectionKind::Album,
        id: r.album_id.get_raw().to_owned(),
        title: r.title.clone(),
        subtitle: join_nonblank(&[&r.artist, &r.year]),
        artwork_url: best_artwork(&r.thumbnails),
    }
}

/// Podcasts are not playlists: they map to `None`.
fn playlist_summary(r: &SearchResultPlaylist) -> Option<CollectionSummary> {
    let (title, subtitle, id, thumbnails) = match r {
        SearchResultPlaylist::Featured(p) => (
            &p.title,
            join_nonblank(&[&p.author, &p.songs]),
            &p.playlist_id,
            &p.thumbnails,
        ),
        SearchResultPlaylist::Community(p) => (
            &p.title,
            join_nonblank(&[&p.author]),
            &p.playlist_id,
            &p.thumbnails,
        ),
        _ => return None,
    };
    Some(CollectionSummary {
        kind: CollectionKind::Playlist,
        id: api_playlist_id(id.get_raw()).to_owned(),
        title: title.clone(),
        subtitle,
        artwork_url: best_artwork(thumbnails),
    })
}

/// A track as a `Song`; `None` without a video id. A track without its own
/// artwork takes `fallback_art`.
fn track_song(
    title: &str,
    artists: &str,
    album: Option<&str>,
    duration: &str,
    video_id: &str,
    thumbnails: &[Thumbnail],
    fallback_art: Option<&str>,
) -> Option<Song> {
    if video_id.trim().is_empty() {
        return None;
    }
    let mut song = map_fields(title, artists, album, duration, video_id, thumbnails);
    if song.artwork_url.is_none() {
        song.artwork_url = fallback_art.map(str::to_owned);
    }
    Some(song)
}

fn album_collection(id: &str, album: &GetAlbum) -> Collection {
    let artists = album
        .artists
        .iter()
        .map(|a| a.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let artwork_url = best_artwork(&album.thumbnails);
    let songs = album
        .tracks
        .iter()
        .filter_map(|t| {
            track_song(
                &t.title,
                &artists,
                Some(&album.title),
                &t.duration,
                t.video_id.get_raw(),
                &[],
                artwork_url.as_deref(),
            )
        })
        .collect();
    Collection {
        summary: CollectionSummary {
            kind: CollectionKind::Album,
            id: id.to_owned(),
            title: album.title.clone(),
            subtitle: join_nonblank(&[&artists, &album.year]),
            artwork_url,
        },
        songs,
    }
}

/// Playable songs of a playlist, at most [`MAX_PLAYLIST_SONGS`].
fn playlist_songs(items: &[PlaylistItem]) -> Vec<Song> {
    items
        .iter()
        .filter_map(|item| match item {
            PlaylistItem::Song(s) if s.is_available => {
                let artists = s
                    .artists
                    .iter()
                    .map(|a| a.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                track_song(
                    &s.title,
                    &artists,
                    Some(&s.album.name),
                    &s.duration,
                    s.video_id.get_raw(),
                    &s.thumbnails,
                    None,
                )
            }
            PlaylistItem::Video(v) if v.is_available => track_song(
                &v.title,
                &v.channel_name,
                None,
                &v.duration,
                v.video_id.get_raw(),
                &v.thumbnails,
                None,
            ),
            _ => None,
        })
        .take(MAX_PLAYLIST_SONGS)
        .collect()
}

/// Songs of a playlist from its pages (`ytmapi-rs` streams them through the
/// continuation API; the plain query only returns the first ~100). Stops
/// reading pages once [`MAX_PLAYLIST_SONGS`] playable songs are collected.
async fn collect_playlist_songs<S, E>(pages: S) -> Result<Vec<Song>, E>
where
    S: Stream<Item = Result<Vec<PlaylistItem>, E>>,
    E: std::fmt::Display,
{
    let mut pages = std::pin::pin!(pages);
    let mut songs = Vec::new();
    while songs.len() < MAX_PLAYLIST_SONGS {
        let Some(page) = pages.next().await else {
            break;
        };
        match page {
            Ok(page) => songs.extend(playlist_songs(&page)),
            // A later page `ytmapi-rs` cannot parse (an item without an
            // artist link, say) must not lose the songs already read.
            Err(e) if !songs.is_empty() => {
                tracing::warn!("playlist paging stopped at {} songs: {e}", songs.len());
                break;
            }
            Err(e) => return Err(e),
        }
    }
    songs.truncate(MAX_PLAYLIST_SONGS);
    Ok(songs)
}

/// YouTube answers an unknown album or playlist id with a page that lacks
/// the browse layout; `ytmapi-rs` reports that as a missing JSON key.
fn is_not_found(e: &ytmapi_rs::Error) -> bool {
    let msg = e.to_string();
    msg.contains("/contents/twoColumnBrowseResultsRenderer") && msg.contains("not found")
}

/// A song without lyrics: either the lyrics tab is missing from the
/// watch-next response, or the lyrics page has no description shelf.
fn is_no_lyrics(e: &ytmapi_rs::Error) -> bool {
    no_lyrics_message(&e.to_string())
}

fn no_lyrics_message(msg: &str) -> bool {
    (msg.contains("watchNextTabbedResultsRenderer/tabs/1")
        || msg.contains("musicDescriptionShelfRenderer"))
        && msg.contains("not found")
}

macro_rules! with_client {
    ($client:expr, $yt:ident => $call:expr) => {
        match $client {
            Client::Anonymous($yt) => $call,
            Client::Browser($yt) => $call,
        }
    };
}

#[async_trait]
impl Catalog for YtmSearcher {
    async fn search_collections(
        &self,
        query: &str,
        kind: CollectionKind,
    ) -> anyhow::Result<Vec<CollectionSummary>> {
        let client = self.client().await?;
        let mut out = match kind {
            CollectionKind::Album => {
                let found = with_client!(&client, yt => yt.search_albums(query).await?);
                found.iter().map(album_summary).collect::<Vec<_>>()
            }
            CollectionKind::Playlist => {
                let found = with_client!(&client, yt => yt.search_playlists(query).await?);
                found.iter().filter_map(playlist_summary).collect()
            }
            CollectionKind::Mix => anyhow::bail!("mixes are not searchable"),
        };
        out.truncate(MAX_RESULTS);
        Ok(out)
    }

    async fn album(&self, id: &str) -> anyhow::Result<Option<Collection>> {
        let client = self.client().await?;
        let album = with_client!(&client, yt => yt.get_album(AlbumID::from_raw(id)).await);
        match album {
            Ok(album) => Ok(Some(album_collection(id, &album))),
            Err(e) if is_not_found(&e) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn playlist(&self, id: &str) -> anyhow::Result<Option<Collection>> {
        let client = self.client().await?;
        let pid = browse_playlist_id(id);
        let tracks = GetPlaylistTracksQuery::new(PlaylistID::from_raw(pid.as_str()));
        let fetched = with_client!(&client, yt => tokio::try_join!(
            yt.get_playlist_details(PlaylistID::from_raw(pid.as_str())),
            collect_playlist_songs(yt.stream(&tracks)),
        ));
        let (details, songs) = match fetched {
            Ok(both) => both,
            Err(e) if is_not_found(&e) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        Ok(Some(Collection {
            summary: CollectionSummary {
                kind: CollectionKind::Playlist,
                id: api_playlist_id(id).to_owned(),
                title: details.title,
                subtitle: join_nonblank(&[&details.author, &details.track_count_text]),
                artwork_url: best_artwork(&details.thumbnails),
            },
            songs,
        }))
    }

    async fn youtube_lyrics(&self, video_id: &str) -> anyhow::Result<Option<String>> {
        let client = self.client().await?;
        let lyrics_id = with_client!(&client, yt => yt
            .get_lyrics_id(ytmapi_rs::common::VideoID::from_raw(video_id))
            .await);
        let lyrics_id = match lyrics_id {
            Ok(id) => id,
            Err(e) if is_no_lyrics(&e) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let lyrics = with_client!(&client, yt => yt.get_lyrics(lyrics_id).await);
        let lyrics = match lyrics {
            Ok(lyrics) => lyrics,
            Err(e) if is_no_lyrics(&e) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let text = lyrics.lyrics.trim();
        Ok((!text.is_empty()).then(|| text.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn thumb(w: u64, url: &str) -> Thumbnail {
        Thumbnail {
            height: w,
            width: w,
            url: url.into(),
        }
    }

    #[test]
    fn api_playlist_id_strips_vl() {
        assert_eq!(api_playlist_id("VLPLabc"), "PLabc");
        assert_eq!(api_playlist_id("OLAKxyz"), "OLAKxyz");
    }

    #[test]
    fn search_playlist_id_loses_the_vl_prefix() {
        let r: SearchResultPlaylist = serde_json::from_value(json!({"Featured": {
            "title": "T", "author": "A", "songs": "10 songs",
            "playlist_id": "VLPLabc", "thumbnails": []
        }}))
        .unwrap();
        assert_eq!(playlist_summary(&r).unwrap().id, "PLabc");
    }

    #[test]
    fn only_a_missing_lyrics_tab_means_no_lyrics() {
        assert!(no_lyrics_message(
            "Key /contents/x/watchNextTabbedResultsRenderer/tabs/1/tabRenderer/endpoint/browseEndpoint/browseId not found in Api response."
        ));
        assert!(no_lyrics_message(
            "Key /contents/sectionListRenderer/contents/0/musicDescriptionShelfRenderer not found in Api response."
        ));
        assert!(!no_lyrics_message("error sending request"));
        assert!(!no_lyrics_message(
            "Http error code 429 recieved in response."
        ));
    }

    #[test]
    fn browse_playlist_id_adds_vl_once() {
        assert_eq!(browse_playlist_id("PLabc"), "VLPLabc");
        assert_eq!(browse_playlist_id("VLPLabc"), "VLPLabc");
    }

    #[test]
    fn collection_serializes_flat_camel_case() {
        let c = Collection {
            summary: CollectionSummary {
                kind: CollectionKind::Album,
                id: "MPREb_1".into(),
                title: "Discovery".into(),
                subtitle: Some("Daft Punk".into()),
                artwork_url: Some("u".into()),
            },
            songs: vec![],
        };
        assert_eq!(
            serde_json::to_value(&c).unwrap(),
            json!({"kind":"album","id":"MPREb_1","title":"Discovery",
                   "subtitle":"Daft Punk","artworkUrl":"u","songs":[]})
        );
    }

    #[test]
    fn tracks_without_video_id_are_dropped() {
        assert!(track_song("T", "A", None, "1:00", "", &[], None).is_none());
        assert!(track_song("T", "A", None, "1:00", "  ", &[], None).is_none());
        assert!(track_song("T", "A", None, "1:00", "id", &[], None).is_some());
    }

    #[test]
    fn track_without_artwork_takes_the_fallback() {
        let own = [thumb(60, "own")];
        let a = track_song("T", "A", None, "1:00", "id", &[], Some("album")).unwrap();
        assert_eq!(a.artwork_url.as_deref(), Some("album"));
        let b = track_song("T", "A", None, "1:00", "id", &own, Some("album")).unwrap();
        assert_eq!(b.artwork_url.as_deref(), Some("own"));
    }

    #[test]
    fn playlist_is_capped_at_200() {
        let items: Vec<PlaylistItem> = (0..250)
            .map(|i| {
                serde_json::from_value(json!({"Video": {
                    "video_id": format!("v{i}"), "track_no": i, "duration": "1:00",
                    "title": "T", "channel_name": "C", "channel_id": "UC1",
                    "like_status": "INDIFFERENT", "thumbnails": [],
                    "is_available": true, "playlist_id": "PL1"
                }}))
                .unwrap()
            })
            .collect();
        let songs = playlist_songs(&items);
        assert_eq!(songs.len(), MAX_PLAYLIST_SONGS);
        assert_eq!(songs[0].video_id, "v0");
    }

    #[test]
    fn featured_playlist_subtitle_has_author_and_track_count() {
        let r: SearchResultPlaylist = serde_json::from_value(json!({"Featured": {
            "title": "Mix", "author": "YouTube Music", "songs": "50 songs",
            "playlist_id": "VLPL1", "thumbnails": []
        }}))
        .unwrap();
        let s = playlist_summary(&r).unwrap();
        assert_eq!(s.subtitle.as_deref(), Some("YouTube Music \u{b7} 50 songs"));
        assert_eq!(s.id, "PL1");
    }

    fn video_page(range: std::ops::Range<usize>) -> Result<Vec<PlaylistItem>, String> {
        Ok(range
            .map(|i| {
                serde_json::from_value(json!({"Video": {
                    "video_id": format!("v{i}"), "track_no": i, "duration": "1:00",
                    "title": "T", "channel_name": "C", "channel_id": "UC1",
                    "like_status": "INDIFFERENT", "thumbnails": [],
                    "is_available": true, "playlist_id": "PL1"
                }}))
                .unwrap()
            })
            .collect())
    }

    #[tokio::test]
    async fn pages_are_joined_and_capped_at_200() {
        let pages = futures_util::stream::iter(vec![
            video_page(0..100),
            video_page(100..200),
            video_page(200..300),
        ]);
        let songs = collect_playlist_songs(pages).await.unwrap();
        assert_eq!(songs.len(), MAX_PLAYLIST_SONGS);
        assert_eq!(songs[199].video_id, "v199");
    }

    #[tokio::test]
    async fn a_failing_later_page_keeps_the_songs_read() {
        let bad = "bad page".to_string();
        let pages = futures_util::stream::iter(vec![video_page(0..100), Err(bad)]);
        assert_eq!(collect_playlist_songs(pages).await.unwrap().len(), 100);
        let bad = "bad page".to_string();
        let first_fails = futures_util::stream::iter(vec![Err(bad)]);
        assert!(collect_playlist_songs(first_fails).await.is_err());
    }

    #[tokio::test]
    async fn short_playlist_ends_with_the_stream() {
        let pages = futures_util::stream::iter(vec![video_page(0..100), video_page(100..130)]);
        assert_eq!(collect_playlist_songs(pages).await.unwrap().len(), 130);
    }

    #[tokio::test]
    async fn collecting_stops_reading_pages_at_the_cap() {
        let pages = futures_util::stream::iter(vec![video_page(0..200)]).chain(
            futures_util::stream::once(async { panic!("read a page past the cap") }),
        );
        assert_eq!(collect_playlist_songs(pages).await.unwrap().len(), 200);
    }

    async fn round_trip(kind: CollectionKind) {
        let searcher = YtmSearcher::new().await.expect("connect to YouTube Music");
        let found = searcher
            .search_collections("daft punk discovery", kind)
            .await
            .expect("search");
        assert!(!found.is_empty());
        let first = &found[0];
        assert!(!first.id.starts_with("VL"), "id {}", first.id);
        eprintln!("{first:#?}");
        let opened = match kind {
            CollectionKind::Album => searcher.album(&first.id).await,
            CollectionKind::Playlist => searcher.playlist(&first.id).await,
            CollectionKind::Mix => unreachable!("mixes are not searchable"),
        }
        .expect("open")
        .expect("found");
        eprintln!(
            "{} songs, first: {:#?}",
            opened.songs.len(),
            opened.songs[0]
        );
        assert_eq!(opened.summary.id, first.id);
        assert!(!opened.songs.is_empty());
        assert!(opened.songs.iter().all(|s| !s.video_id.is_empty()));
        assert!(opened.songs.len() <= MAX_PLAYLIST_SONGS);
        if kind == CollectionKind::Playlist {
            eprintln!("subtitle: {:?}", opened.summary.subtitle);
        }
    }

    #[tokio::test]
    #[ignore = "hits YouTube Music"]
    async fn live_album_round_trip() {
        round_trip(CollectionKind::Album).await;
    }

    #[tokio::test]
    #[ignore = "hits YouTube Music"]
    async fn live_playlist_round_trip() {
        round_trip(CollectionKind::Playlist).await;
    }

    /// A playlist with well over 100 tracks must page past the first answer.
    #[tokio::test]
    #[ignore = "hits YouTube Music"]
    async fn live_big_playlist_pages_up_to_200() {
        let searcher = YtmSearcher::new().await.expect("connect");
        let mut found = Vec::new();
        for q in [
            "best songs of all time",
            "top 200 songs playlist",
            "greatest hits 300 songs",
        ] {
            found.extend(
                searcher
                    .search_collections(q, CollectionKind::Playlist)
                    .await
                    .expect("search"),
            );
        }
        let mut best = 0;
        for c in &found {
            match searcher.playlist(&c.id).await {
                Ok(Some(p)) => {
                    eprintln!("{} -> {} songs ({:?})", c.title, p.songs.len(), c.subtitle);
                    best = best.max(p.songs.len());
                    assert!(p.songs.len() <= MAX_PLAYLIST_SONGS);
                }
                Ok(None) => {}
                // Some playlists fail in ytmapi-rs' own parsing; not what this checks.
                Err(e) => eprintln!("{} -> error: {e}", c.title),
            }
            if best > 100 {
                break;
            }
        }
        assert!(best > 100, "no playlist paged past 100 songs: {best}");
    }

    #[tokio::test]
    #[ignore = "hits YouTube Music"]
    async fn live_unknown_ids_are_none() {
        let searcher = YtmSearcher::new().await.expect("connect");
        assert!(
            searcher
                .album("MPREb_doesnotexist")
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            searcher
                .playlist("PLdoesnotexist12345")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    #[ignore = "hits YouTube Music"]
    async fn live_lyrics() {
        let searcher = YtmSearcher::new().await.expect("connect");
        let lyrics = searcher
            .youtube_lyrics("FGBhQbmPwH8")
            .await
            .expect("lyrics");
        eprintln!("{lyrics:?}");
    }
}
