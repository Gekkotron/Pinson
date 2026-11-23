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
use serde_json::Value;
use ytmapi_rs::YtMusic;
use ytmapi_rs::auth::AuthToken;
use ytmapi_rs::common::ArtistChannelID;
use ytmapi_rs::common::{AlbumID, PlaylistID, Thumbnail, YoutubeID};
use ytmapi_rs::parse::{
    AlbumResult, ArtistSong, GetAlbum, GetArtist, PlaylistItem, SearchResultAlbum,
    SearchResultArtist, SearchResultPlaylist,
};
use ytmapi_rs::query::SearchQuery;
use ytmapi_rs::query::search::AlbumsFilter;
use ytmapi_rs::query::{GetArtistQuery, GetPlaylistTracksQuery};

use crate::artwork::upscale;
use crate::fuzzy::fold;
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
    Smart,
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

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtistSummary {
    pub id: String,
    pub name: String,
    /// The subscriber count text, e.g. "12.3M subscribers".
    pub subtitle: Option<String>,
    pub artwork_url: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Artist {
    pub id: String,
    pub name: String,
    pub artwork_url: Option<String>,
    pub description: Option<String>,
    pub subscribers: Option<String>,
    pub songs: Vec<Song>,
    pub albums: Vec<CollectionSummary>,
    pub singles: Vec<CollectionSummary>,
    pub related: Vec<ArtistSummary>,
}

/// Most top songs kept on an artist page.
pub const MAX_ARTIST_SONGS: usize = 50;

#[async_trait]
pub trait Catalog: Send + Sync + 'static {
    async fn search_collections(
        &self,
        query: &str,
        kind: CollectionKind,
    ) -> anyhow::Result<Vec<CollectionSummary>>;
    async fn album(&self, id: &str) -> anyhow::Result<Option<Collection>>;
    async fn playlist(&self, id: &str) -> anyhow::Result<Option<Collection>>;
    /// An artist page; `None` for an unknown channel id.
    async fn artist(&self, id: &str) -> anyhow::Result<Option<Artist>>;
    /// The channel id of the artist a credit such as "Daft Punk & Pharrell"
    /// names; `None` when YouTube Music finds no artist.
    async fn find_artist(&self, name: &str) -> anyhow::Result<Option<String>>;
    async fn search_artists(&self, query: &str) -> anyhow::Result<Vec<ArtistSummary>>;
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

pub(crate) fn best_artwork(thumbnails: &[Thumbnail]) -> Option<String> {
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

/// Remove unsupported album types before handing the page to ytmapi-rs.
/// Keep malformed items so the library still reports unexpected page layouts.
fn skip_unknown_album_types(page: &mut Value) {
    let Some(sections) = page.pointer_mut("/contents/tabbedSearchResultsRenderer/tabs/0/tabRenderer/content/sectionListRenderer/contents").and_then(Value::as_array_mut) else {
        return;
    };
    for section in sections {
        if let Some(items) = section
            .pointer_mut("/musicShelfRenderer/contents")
            .and_then(Value::as_array_mut)
        {
            items.retain(|item| {
                item.pointer("/musicResponsiveListItemRenderer/flexColumns/1/musicResponsiveListItemFlexColumnRenderer/text/runs/0/text")
                    .and_then(Value::as_str)
                    .is_none_or(|kind| matches!(kind, "Single" | "Album" | "EP"))
            });
        }
    }
}

async fn search_albums<A: AuthToken>(
    yt: &YtMusic<A>,
    text: &str,
) -> anyhow::Result<Vec<SearchResultAlbum>> {
    let query = SearchQuery::new_filtered(text, AlbumsFilter);
    let json = yt.raw_json_query(query.clone()).await?;
    let mut page: Value = serde_json::from_str(&json)?;
    skip_unknown_album_types(&mut page);
    Ok(ytmapi_rs::process_json::<_, A>(page.to_string(), query)?)
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

/// Credit separators, folded: a credit such as "A feat. B & C" names several artists.
const CREDIT_SEPARATORS: [&str; 6] = [", ", " & ", " feat. ", " ft. ", " x ", " and "];

/// The folded credit and each of its credited parts.
fn credit_names(name: &str) -> Vec<String> {
    let whole = fold(name);
    let mut marked = whole.clone();
    for sep in CREDIT_SEPARATORS {
        marked = marked.replace(sep, "\u{0}");
    }
    let mut names = vec![whole.split_whitespace().collect::<Vec<_>>().join(" ")];
    names.extend(
        marked
            .split('\u{0}')
            .map(|p| p.split_whitespace().collect::<Vec<_>>().join(" "))
            .filter(|p| !p.is_empty()),
    );
    names
}

/// Index of the candidate named like the whole credit, else like its first
/// credited artist that is among the candidates (credit order, not result
/// order: "Daft Punk & Pharrell" opens Daft Punk), else the first candidate;
/// `None` without candidates.
fn best_artist_match(name: &str, candidates: &[&str]) -> Option<usize> {
    let folded: Vec<String> = candidates
        .iter()
        .map(|c| fold(c).split_whitespace().collect::<Vec<_>>().join(" "))
        .collect();
    credit_names(name)
        .iter()
        .find_map(|w| folded.iter().position(|c| c == w))
        .or((!candidates.is_empty()).then_some(0))
}

fn artist_summary(r: &SearchResultArtist) -> ArtistSummary {
    ArtistSummary {
        id: r.browse_id.get_raw().to_owned(),
        name: r.artist.clone(),
        subtitle: r.subscribers.clone().filter(|s| !s.trim().is_empty()),
        artwork_url: best_artwork(&r.thumbnails),
    }
}

fn artist_album(r: &AlbumResult) -> CollectionSummary {
    CollectionSummary {
        kind: CollectionKind::Album,
        id: r.album_id.get_raw().to_owned(),
        title: r.title.clone(),
        subtitle: join_nonblank(&[&r.year]),
        artwork_url: best_artwork(&r.thumbnails),
    }
}

fn artist_song(s: &ArtistSong) -> Option<Song> {
    let artists = s
        .artists
        .iter()
        .map(|a| a.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let album = Some(s.album.name.as_str()).filter(|a| !a.trim().is_empty());
    track_song(
        &s.title,
        &artists,
        album,
        "",
        s.video_id.get_raw(),
        &[],
        None,
    )
}

/// The artist page without its top songs, which the caller loads.
fn artist_page(id: &str, a: &GetArtist) -> Artist {
    let releases = &a.top_releases;
    let section = |s: &Option<ytmapi_rs::parse::GetArtistAlbums>| {
        s.as_ref()
            .map(|s| s.results.iter().map(artist_album).collect())
            .unwrap_or_default()
    };
    Artist {
        id: id.to_owned(),
        name: a.name.clone(),
        artwork_url: best_artwork(&a.thumbnails),
        description: a.description.clone().filter(|d| !d.trim().is_empty()),
        subscribers: a.subscribers.clone().filter(|s| !s.trim().is_empty()),
        songs: releases
            .songs
            .as_ref()
            .map(|s| s.results.iter().filter_map(artist_song).collect())
            .unwrap_or_default(),
        albums: section(&releases.albums),
        singles: section(&releases.singles),
        related: releases
            .related
            .as_ref()
            .map(|r| {
                r.results
                    .iter()
                    .map(|r| ArtistSummary {
                        id: r.browse_id.get_raw().to_owned(),
                        name: r.title.clone(),
                        subtitle: Some(r.subscribers.clone()).filter(|s| !s.trim().is_empty()),
                        artwork_url: None,
                    })
                    .collect()
            })
            .unwrap_or_default(),
    }
}

/// Text of the first run of a runs array at `path`.
fn first_run<'a>(item: &'a Value, path: &str) -> Option<&'a str> {
    item.pointer(path)?
        .as_array()?
        .first()?
        .get("text")?
        .as_str()
}

/// The thumbnails of a `musicTwoRowItemRenderer`.
fn two_row_art(item: &Value) -> Option<String> {
    let thumbs = item
        .pointer("/thumbnailRenderer/musicThumbnailRenderer/thumbnail/thumbnails")?
        .as_array()?;
    let thumbs: Vec<Thumbnail> = thumbs
        .iter()
        .filter_map(|t| {
            Some(Thumbnail {
                url: t.get("url")?.as_str()?.to_owned(),
                width: t.get("width").and_then(Value::as_u64).unwrap_or(0),
                height: t.get("height").and_then(Value::as_u64).unwrap_or(0),
            })
        })
        .collect();
    best_artwork(&thumbs)
}

/// The "Singles & EPs" and "Fans might also like" shelves of an artist page,
/// which `ytmapi-rs` 0.3.3 skips. Shelves are told apart by what their items
/// are (singles and EPs, artists), not by their localised titles.
fn artist_extra_shelves(page: &Value) -> (Vec<CollectionSummary>, Vec<ArtistSummary>) {
    let mut singles = Vec::new();
    let mut related = Vec::new();
    let Some(sections) = page
        .pointer("/contents/singleColumnBrowseResultsRenderer/tabs/0/tabRenderer/content/sectionListRenderer/contents")
        .and_then(Value::as_array)
    else {
        return (singles, related);
    };
    for section in sections {
        let Some(items) = section
            .pointer("/musicCarouselShelfRenderer/contents")
            .and_then(Value::as_array)
        else {
            continue;
        };
        for item in items {
            let Some(item) = item.get("musicTwoRowItemRenderer") else {
                continue;
            };
            let Some(title) = first_run(item, "/title/runs") else {
                continue;
            };
            let Some(id) = item
                .pointer("/title/runs/0/navigationEndpoint/browseEndpoint/browseId")
                .and_then(Value::as_str)
            else {
                continue;
            };
            let page_type = item
                .pointer("/title/runs/0/navigationEndpoint/browseEndpoint/browseEndpointContextSupportedConfigs/browseEndpointContextMusicConfig/pageType")
                .and_then(Value::as_str);
            match page_type {
                Some("MUSIC_PAGE_TYPE_ARTIST") => related.push(ArtistSummary {
                    id: id.to_owned(),
                    name: title.to_owned(),
                    subtitle: first_run(item, "/subtitle/runs")
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned),
                    artwork_url: two_row_art(item),
                }),
                Some("MUSIC_PAGE_TYPE_ALBUM")
                    if matches!(first_run(item, "/subtitle/runs"), Some("Single" | "EP")) =>
                {
                    let year = item
                        .pointer("/subtitle/runs")
                        .and_then(Value::as_array)
                        .and_then(|r| r.last())
                        .and_then(|r| r.get("text"))
                        .and_then(Value::as_str)
                        .filter(|y| y.len() == 4 && y.bytes().all(|b| b.is_ascii_digit()));
                    singles.push(CollectionSummary {
                        kind: CollectionKind::Album,
                        id: id.to_owned(),
                        title: title.to_owned(),
                        subtitle: year.map(str::to_owned),
                        artwork_url: two_row_art(item),
                    });
                }
                _ => {}
            }
        }
    }
    (singles, related)
}

/// One artist-page request read twice: typed by `ytmapi-rs`, and raw for the
/// shelves it skips. `None` for an unknown channel.
async fn fetch_artist<A: AuthToken>(
    yt: &YtMusic<A>,
    id: &str,
) -> anyhow::Result<Option<(GetArtist, Value)>> {
    let query = GetArtistQuery::new(ArtistChannelID::from_raw(id));
    let json = match yt.raw_json_query(query.clone()).await {
        Ok(json) => json,
        Err(e) if is_not_found(&e) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let raw: Value = serde_json::from_str(&json)?;
    match ytmapi_rs::process_json::<_, A>(json, query) {
        Ok(page) => Ok(Some((page, raw))),
        Err(e) if is_not_found(&e) => Ok(None),
        Err(e) => Err(e.into()),
    }
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
    not_found_message(&e.to_string())
}

fn not_found_message(msg: &str) -> bool {
    msg.contains("/contents/twoColumnBrowseResultsRenderer")
        && msg.contains("not found")
        // A key missing inside one track is a layout quirk, not a missing playlist.
        && !msg.contains("musicPlaylistShelfRenderer/contents/")
}

/// The songs of a playlist's first tracks page, read leniently: `ytmapi-rs`
/// fails the whole page when one item doesn't match its layout (an artist
/// run without a browse link), so skip only the unreadable items.
pub fn songs_from_playlist_page(page: &serde_json::Value) -> Vec<Song> {
    use serde_json::Value;
    let sections = page
        .pointer("/contents/twoColumnBrowseResultsRenderer/secondaryContents/sectionListRenderer/contents")
        .and_then(Value::as_array);
    sections
        .into_iter()
        .flatten()
        .filter_map(|section| {
            section
                .pointer("/musicPlaylistShelfRenderer/contents")?
                .as_array()
        })
        .flatten()
        .filter_map(|item| playlist_song_from_item(item.get("musicResponsiveListItemRenderer")?))
        .take(MAX_PLAYLIST_SONGS)
        .collect()
}

fn playlist_song_from_item(item: &serde_json::Value) -> Option<Song> {
    use serde_json::Value;
    // Unavailable (greyed-out) tracks would only fail at playback.
    if item
        .get("musicItemRendererDisplayPolicy")
        .and_then(Value::as_str)
        == Some("MUSIC_ITEM_RENDERER_DISPLAY_POLICY_GREY_OUT")
    {
        return None;
    }
    let column = |i: usize| {
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
    let title = column(0).filter(|t| !t.is_empty())?;
    let video_id = item.pointer("/playlistItemData/videoId")?.as_str()?;
    let artists = column(1).unwrap_or_default();
    let album = column(2).filter(|a| !a.is_empty());
    let duration = item
        .pointer("/fixedColumns/0/musicResponsiveListItemFixedColumnRenderer/text/runs/0/text")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let thumbnails: Vec<Thumbnail> = item
        .pointer("/thumbnail/musicThumbnailRenderer/thumbnail/thumbnails")
        .and_then(|t| serde_json::from_value(t.clone()).ok())
        .unwrap_or_default();
    Some(map_fields(
        &title,
        &artists,
        album.as_deref(),
        duration,
        video_id,
        &thumbnails,
    ))
}

/// All the songs of a playlist; when `ytmapi-rs` rejects the first page,
/// falls back to reading it leniently (without continuation pages).
async fn playlist_tracks<A: ytmapi_rs::auth::AuthToken>(
    yt: &ytmapi_rs::YtMusic<A>,
    tracks: &GetPlaylistTracksQuery<'_>,
) -> Result<Vec<Song>, ytmapi_rs::Error> {
    let e = match collect_playlist_songs(yt.stream(tracks)).await {
        Ok(songs) => return Ok(songs),
        Err(e) => e,
    };
    let Ok(json) = yt
        .raw_json_query::<GetPlaylistTracksQuery<'_>>(tracks)
        .await
    else {
        return Err(e);
    };
    let songs = serde_json::from_str(&json)
        .map(|page| songs_from_playlist_page(&page))
        .unwrap_or_default();
    if songs.is_empty() {
        return Err(e);
    }
    tracing::debug!("ytmapi-rs rejected a playlist page, read it leniently: {e}");
    Ok(songs)
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
                let found = with_client!(&client, yt => search_albums(yt, query).await?);
                found.iter().map(album_summary).collect::<Vec<_>>()
            }
            CollectionKind::Playlist => {
                let found = with_client!(&client, yt => yt.search_playlists(query).await?);
                found.iter().filter_map(playlist_summary).collect()
            }
            CollectionKind::Mix | CollectionKind::Smart => {
                anyhow::bail!("mixes and smart lists are not searchable")
            }
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
            playlist_tracks(yt, &tracks),
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

    async fn artist(&self, id: &str) -> anyhow::Result<Option<Artist>> {
        let client = self.client().await?;
        let fetched = with_client!(&client, yt => fetch_artist(yt, id).await?);
        let Some((page, raw)) = fetched else {
            return Ok(None);
        };
        let mut artist = artist_page(id, &page);
        (artist.singles, artist.related) = artist_extra_shelves(&raw);
        if let Some(top) = page.top_releases.songs.as_ref() {
            match self.playlist(top.browse_id.get_raw()).await {
                Ok(Some(list)) if !list.songs.is_empty() => {
                    artist.songs = list.songs;
                    artist.songs.truncate(MAX_ARTIST_SONGS);
                }
                Ok(_) => {}
                Err(e) => tracing::debug!("artist {id} top songs playlist failed: {e:#}"),
            }
        }
        Ok(Some(artist))
    }

    async fn find_artist(&self, name: &str) -> anyhow::Result<Option<String>> {
        let found = self.search_artists(name).await?;
        let names: Vec<&str> = found.iter().map(|a| a.name.as_str()).collect();
        Ok(best_artist_match(name, &names).map(|i| found[i].id.clone()))
    }

    async fn search_artists(&self, query: &str) -> anyhow::Result<Vec<ArtistSummary>> {
        let client = self.client().await?;
        let found = with_client!(&client, yt => yt.search_artists(query).await?);
        let mut out: Vec<ArtistSummary> = found.iter().map(artist_summary).collect();
        out.truncate(MAX_RESULTS);
        Ok(out)
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

    #[test]
    fn album_search_skips_unknown_types_and_keeps_supported_results() {
        fn item(kind: &str) -> Value {
            json!({"musicResponsiveListItemRenderer": {
                "flexColumns": [
                    {"musicResponsiveListItemFlexColumnRenderer": {"text": {"runs": [{"text": kind}]}}},
                    {"musicResponsiveListItemFlexColumnRenderer": {"text": {"runs": [
                        {"text": kind}, {"text": " • "}, {"text": "Artist"}, {"text": " • "}, {"text": "2024"}
                    ]}}}
                ],
                "navigationEndpoint": {"browseEndpoint": {"browseId": "MPREb_test"}},
                "thumbnail": {"musicThumbnailRenderer": {"thumbnail": {"thumbnails": []}}}
            }})
        }
        let mut page = json!({"contents": {"tabbedSearchResultsRenderer": {"tabs": [{"tabRenderer": {
            "content": {"sectionListRenderer": {"contents": [
                {"musicShelfRenderer": {"contents": [item("Album"), item("Audiobook"), item("Show"), item("EP"), item("Single")]}}
            ]}}
        }}]}}});
        skip_unknown_album_types(&mut page);
        let albums = ytmapi_rs::process_json::<_, ytmapi_rs::auth::noauth::NoAuthToken>(
            page.to_string(),
            SearchQuery::new_filtered("test", AlbumsFilter),
        )
        .unwrap();
        assert_eq!(
            albums
                .iter()
                .map(|album| album.title.as_str())
                .collect::<Vec<_>>(),
            ["Album", "EP", "Single"]
        );
    }

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
            CollectionKind::Mix | CollectionKind::Smart => {
                unreachable!("mixes and smart lists are not searchable")
            }
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

    fn playlist_item(
        title: &str,
        video_id: Option<&str>,
        artist_run: serde_json::Value,
    ) -> serde_json::Value {
        let mut item = json!({"musicResponsiveListItemRenderer": {
            "flexColumns": [
                {"musicResponsiveListItemFlexColumnRenderer": {"text": {"runs": [{"text": title}]}}},
                {"musicResponsiveListItemFlexColumnRenderer": {"text": {"runs": [artist_run]}}},
                {"musicResponsiveListItemFlexColumnRenderer": {"text": {"runs": [{"text": "Some Album"}]}}}
            ],
            "fixedColumns": [{"musicResponsiveListItemFixedColumnRenderer": {"text": {"runs": [{"text": "3:25"}]}}}],
            "thumbnail": {"musicThumbnailRenderer": {"thumbnail": {"thumbnails": [
                {"url": "https://example.com/a.jpg", "width": 60, "height": 60}]}}}
        }});
        if let Some(id) = video_id {
            item["musicResponsiveListItemRenderer"]["playlistItemData"] = json!({"videoId": id});
        }
        item
    }

    #[test]
    fn lenient_playlist_page_keeps_unlinked_artists_and_skips_unplayable() {
        let linked = json!({"text": "Linked", "navigationEndpoint": {"browseEndpoint": {"browseId": "UC1"}}});
        let mut grey = playlist_item("Four", Some("v4"), json!({"text": "Blocked"}));
        grey["musicResponsiveListItemRenderer"]["musicItemRendererDisplayPolicy"] =
            json!("MUSIC_ITEM_RENDERER_DISPLAY_POLICY_GREY_OUT");
        let page = json!({"contents": {"twoColumnBrowseResultsRenderer": {"secondaryContents": {
        "sectionListRenderer": {"contents": [{"musicPlaylistShelfRenderer": {"contents": [
            playlist_item("One", Some("v1"), linked),
            playlist_item("Two", Some("v2"), json!({"text": "Unlinked"})),
            playlist_item("Three", None, json!({"text": "Nobody"})),
            grey,
        ]}}]}}}}});
        let songs = songs_from_playlist_page(&page);
        assert_eq!(songs.len(), 2);
        assert_eq!(songs[0].video_id, "v1");
        assert_eq!(songs[1].title, "Two");
        assert_eq!(songs[1].artists, "Unlinked");
        assert_eq!(songs[1].album.as_deref(), Some("Some Album"));
        assert_eq!(songs[1].duration_secs, Some(205));
    }

    #[test]
    fn a_broken_track_is_not_a_missing_playlist() {
        let track = "Key /contents/twoColumnBrowseResultsRenderer/secondaryContents/sectionListRenderer/contents/0/musicPlaylistShelfRenderer/contents/22/musicResponsiveListItemRenderer/flexColumns/1/musicResponsiveListItemFlexColumnRenderer/text/runs/0/navigationEndpoint/browseEndpoint/browseId not found";
        assert!(!not_found_message(track));
        assert!(not_found_message(
            "Key /contents/twoColumnBrowseResultsRenderer/tabs not found"
        ));
    }

    #[tokio::test]
    #[ignore = "hits YouTube Music"]
    async fn live_playlist_with_an_unlinked_artist_opens() {
        let searcher = YtmSearcher::new().await.expect("connect");
        let found = searcher
            .playlist("RDCLAK5uy_lccrSOoaBxTPBANsA5byQyYLbyDK49q9I")
            .await
            .expect("playlist")
            .expect("found");
        assert!(!found.songs.is_empty());
    }

    #[tokio::test]
    #[ignore = "hits YouTube Music"]
    async fn live_artist_round_trip() {
        let searcher = YtmSearcher::new().await.expect("connect to YouTube Music");
        let id = searcher
            .find_artist("Daft Punk")
            .await
            .expect("find")
            .expect("an artist");
        eprintln!("channel id {id}");
        let artist = searcher.artist(&id).await.expect("open").expect("found");
        eprintln!(
            "{} | subs {:?} | art {:?} | desc {:?}\nsongs {} albums {} singles {} related {}",
            artist.name,
            artist.subscribers,
            artist.artwork_url,
            artist.description.as_deref().map(|d| d.len()),
            artist.songs.len(),
            artist.albums.len(),
            artist.singles.len(),
            artist.related.len()
        );
        eprintln!("first song {:#?}", artist.songs.first());
        eprintln!("first album {:#?}", artist.albums.first());
        eprintln!("first related {:#?}", artist.related.first());
        assert_eq!(artist.name, "Daft Punk");
        assert!(!artist.songs.is_empty());
        assert!(!artist.albums.is_empty());
        assert!(!artist.singles.is_empty());
        assert!(!artist.related.is_empty());
        let found = searcher.search_artists("daft punk").await.expect("search");
        assert!(!found.is_empty());
        eprintln!("{:#?}", found[0]);
    }

    #[test]
    fn extra_shelves_read_singles_and_related_artists() {
        let item = |id: &str, kind: &str, title: &str, sub: Value| {
            json!({"musicTwoRowItemRenderer": {
                "thumbnailRenderer": {"musicThumbnailRenderer": {"thumbnail": {"thumbnails": [
                    {"url": "https://x/s=w226-h226", "width": 226, "height": 226},
                    {"url": "https://x/s=w544-h544", "width": 544, "height": 544}]}}},
                "title": {"runs": [{"text": title, "navigationEndpoint": {"browseEndpoint": {
                    "browseId": id,
                    "browseEndpointContextSupportedConfigs": {"browseEndpointContextMusicConfig": {"pageType": kind}}}}}]},
                "subtitle": {"runs": sub}}})
        };
        let runs = |t: &[&str]| t.iter().map(|t| json!({"text": t})).collect::<Vec<_>>();
        let shelf = |items: Vec<Value>| json!({"musicCarouselShelfRenderer": {"contents": items}});
        let page = json!({"contents": {"singleColumnBrowseResultsRenderer": {"tabs": [{"tabRenderer": {"content": {"sectionListRenderer": {"contents": [
            shelf(vec![item("MPREb_a", "MUSIC_PAGE_TYPE_ALBUM", "Album", json!(runs(&["Album", " \u{2022} ", "2001"])))]),
            shelf(vec![
                item("MPREb_s", "MUSIC_PAGE_TYPE_ALBUM", "Single", json!(runs(&["Single", " \u{2022} ", "2023"]))),
                item("MPREb_e", "MUSIC_PAGE_TYPE_ALBUM", "Ep", json!(runs(&["EP"]))),
            ]),
            shelf(vec![item("UC1", "MUSIC_PAGE_TYPE_ARTIST", "Capital Cities", json!(runs(&["24.3M monthly audience"])))]),
        ]}}}}]}}});
        let (singles, related) = artist_extra_shelves(&page);
        assert_eq!(singles.len(), 2);
        assert_eq!(singles[0].id, "MPREb_s");
        assert_eq!(singles[0].subtitle.as_deref(), Some("2023"));
        assert_eq!(singles[1].subtitle, None);
        assert_eq!(
            singles[0].artwork_url.as_deref().map(|u| u.contains("544")),
            Some(true)
        );
        assert_eq!(related.len(), 1);
        assert_eq!(related[0].id, "UC1");
        assert_eq!(
            related[0].subtitle.as_deref(),
            Some("24.3M monthly audience")
        );
        assert!(related[0].artwork_url.is_some());
        assert_eq!(artist_extra_shelves(&json!({})), (vec![], vec![]));
    }

    #[test]
    fn credit_names_split_on_every_separator() {
        assert_eq!(
            credit_names("Daft Punk & Pharrell"),
            ["daft punk & pharrell", "daft punk", "pharrell"]
        );
        assert_eq!(
            credit_names("A, B feat. C ft. D x E and F"),
            ["a, b feat. c ft. d x e and f", "a", "b", "c", "d", "e", "f"]
        );
        assert_eq!(
            credit_names("Simon and Garfunkel")[0],
            "simon and garfunkel"
        );
    }

    #[test]
    fn artist_match_prefers_whole_name_then_credited_part_then_first() {
        let found = ["Pharrell Daft", "Beyoncé", "Daft Punk"];
        assert_eq!(best_artist_match("daft punk", &found), Some(2));
        assert_eq!(best_artist_match("BEYONCE", &found), Some(1));
        assert_eq!(best_artist_match("Pharrell & Beyoncé", &found), Some(1));
        assert_eq!(best_artist_match("Nobody", &found), Some(0));
        assert_eq!(best_artist_match("Nobody", &[]), None);
        let credited = ["Pharrell", "Daft Punk"];
        assert_eq!(
            best_artist_match("Daft Punk & Pharrell", &credited),
            Some(1)
        );
    }

    #[test]
    fn artist_serializes_camel_case() {
        let a = Artist {
            id: "UC1".into(),
            name: "N".into(),
            artwork_url: None,
            description: None,
            subscribers: None,
            songs: vec![],
            albums: vec![],
            singles: vec![],
            related: vec![ArtistSummary {
                id: "UC2".into(),
                name: "M".into(),
                subtitle: None,
                artwork_url: None,
            }],
        };
        let v = serde_json::to_value(&a).unwrap();
        assert!(v.get("artworkUrl").is_some());
        assert!(v["related"][0].get("artworkUrl").is_some());
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
