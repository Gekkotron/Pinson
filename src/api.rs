//! HTTP API and WebSocket (see `docs/api.md`).

use std::sync::Arc;

use axum::extract::rejection::JsonRejection;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use rand::seq::SliceRandom;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::broadcast::error::RecvError;
use tokio::time::Instant;

use crate::artwork::{ArtworkError, ArtworkProxy};
use crate::catalog::{Catalog, Collection, CollectionKind, CollectionSummary};
use crate::favorites::Favorites;
use crate::lyrics::{Lyrics, LyricsQuery, LyricsService};
use crate::player::{PlayerHandle, PlayerState, StateView};
use crate::playlists::{AddSongsOutcome, Playlist, PlaylistError, PlaylistSummary, Playlists};
use crate::queue::{AddAt, EntryId};
use crate::search::Searcher;
use crate::song::Song;
use crate::speakers::{Speaker, SpeakerChange, SpeakerControl, SpeakerError};

/// Longest song list accepted by `queue/play` and `queue/add`.
pub const MAX_QUEUE_SONGS: usize = 500;

/// Everything the routes need; built once in `main` (and in tests).
#[derive(Clone)]
pub struct Services {
    pub player: PlayerHandle,
    pub searcher: Arc<dyn Searcher>,
    pub catalog: Arc<dyn Catalog>,
    pub favorites: Favorites,
    pub playlists: Playlists,
    pub artwork: Arc<ArtworkProxy>,
    pub lyrics: Arc<LyricsService>,
    /// Per-speaker volume; `None` unless the engine is nidoo.
    pub speakers: Option<Arc<SpeakerControl>>,
}

pub fn router(services: Services) -> Router {
    Router::new()
        .route("/api/search", get(search))
        .route("/api/albums/{id}", get(album_get))
        .route("/api/ytplaylists/{id}", get(ytplaylist_get))
        .route("/api/artwork", get(artwork))
        .route("/api/lyrics", get(lyrics))
        .route("/api/state", get(state))
        .route("/api/queue/next", post(queue_next))
        .route("/api/queue/play", post(queue_play))
        .route("/api/queue/add", post(queue_add))
        .route("/api/queue/{entry_id}", delete(queue_remove))
        .route("/api/player/pause", post(pause))
        .route("/api/player/resume", post(resume))
        .route("/api/player/skip", post(skip))
        .route("/api/player/previous", post(previous))
        .route("/api/player/seek", post(seek))
        .route("/api/favorites", get(favorites_list))
        .route("/api/favorites/shuffle", post(favorites_shuffle))
        .route(
            "/api/favorites/{video_id}",
            put(favorite_add).delete(favorite_remove),
        )
        .route("/api/playlists", get(playlists_list).post(playlist_create))
        .route(
            "/api/playlists/{id}",
            get(playlist_get)
                .patch(playlist_rename)
                .delete(playlist_delete),
        )
        .route("/api/playlists/{id}/songs", post(playlist_add_songs))
        .route(
            "/api/playlists/{id}/songs/{video_id}",
            delete(playlist_remove_song),
        )
        .route("/api/playlists/{id}/order", put(playlist_reorder))
        .route("/api/speakers", get(speakers_list))
        .route("/api/speakers/{id}", put(speaker_change))
        .route("/api/ws", get(ws))
        .with_state(services)
}

/// An error answered as `{"error": "..."}`.
struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
    }
}

impl From<PlaylistError> for ApiError {
    fn from(e: PlaylistError) -> Self {
        let status = match &e {
            PlaylistError::NotFound(_) | PlaylistError::SongNotFound(_) => StatusCode::NOT_FOUND,
            PlaylistError::BadName(_) | PlaylistError::BadOrder(_) => StatusCode::BAD_REQUEST,
            PlaylistError::OrderMismatch => StatusCode::CONFLICT,
            PlaylistError::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        ApiError(status, e.to_string())
    }
}

type ApiResult<T> = Result<T, ApiError>;

#[derive(Deserialize)]
struct SearchParams {
    q: Option<String>,
    kind: Option<String>,
}

/// `GET /api/search`: songs by default, or albums / playlists with `kind`.
async fn search(
    State(app): State<Services>,
    Query(params): Query<SearchParams>,
) -> ApiResult<Response> {
    let kind = match params.kind.as_deref().unwrap_or("songs") {
        "songs" => None,
        "albums" => Some(CollectionKind::Album),
        "playlists" => Some(CollectionKind::Playlist),
        other => {
            return Err(ApiError(
                StatusCode::BAD_REQUEST,
                format!("unknown kind {other:?}: use songs, albums or playlists"),
            ));
        }
    };
    let q = params.q.unwrap_or_default();
    let q = q.trim();
    if q.is_empty() {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "q must not be blank".into(),
        ));
    }
    let failed = |e: anyhow::Error| {
        tracing::warn!("search {q:?} failed: {e:#}");
        ApiError(StatusCode::BAD_GATEWAY, format!("search failed: {e:#}"))
    };
    match kind {
        None => {
            let songs: Vec<Song> = app.searcher.search(q).await.map_err(failed)?;
            Ok(Json(songs).into_response())
        }
        Some(kind) => {
            let found: Vec<CollectionSummary> = app
                .catalog
                .search_collections(q, kind)
                .await
                .map_err(failed)?;
            Ok(Json(found).into_response())
        }
    }
}

/// Answers a collection lookup: 404 when unknown, 502 on an upstream error.
fn collection_response(
    what: &str,
    id: &str,
    found: anyhow::Result<Option<Collection>>,
) -> ApiResult<Json<Collection>> {
    match found {
        Ok(Some(c)) => Ok(Json(c)),
        Ok(None) => Err(ApiError(StatusCode::NOT_FOUND, format!("no {what} {id}"))),
        Err(e) => {
            tracing::warn!("{what} {id:?} failed: {e:#}");
            Err(ApiError(
                StatusCode::BAD_GATEWAY,
                format!("{what} failed: {e:#}"),
            ))
        }
    }
}

async fn album_get(
    State(app): State<Services>,
    Path(id): Path<String>,
) -> ApiResult<Json<Collection>> {
    collection_response("album", &id, app.catalog.album(&id).await)
}

async fn ytplaylist_get(
    State(app): State<Services>,
    Path(id): Path<String>,
) -> ApiResult<Json<Collection>> {
    collection_response("playlist", &id, app.catalog.playlist(&id).await)
}

#[derive(Deserialize)]
struct ArtworkParams {
    url: Option<String>,
}

async fn artwork(
    State(app): State<Services>,
    Query(params): Query<ArtworkParams>,
) -> Result<Response, ApiError> {
    let url = params.url.unwrap_or_default();
    match app.artwork.fetch(&url).await {
        Ok(art) => Ok((
            [
                (header::CONTENT_TYPE, art.content_type),
                (header::CACHE_CONTROL, "public, max-age=86400".to_owned()),
            ],
            art.bytes,
        )
            .into_response()),
        Err(e) => {
            let status = match e {
                ArtworkError::NotAllowed => StatusCode::BAD_REQUEST,
                _ => StatusCode::BAD_GATEWAY,
            };
            Err(ApiError(status, e.to_string()))
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LyricsParams {
    video_id: Option<String>,
    title: Option<String>,
    artists: Option<String>,
    album: Option<String>,
    /// Text, so a bad value answers a JSON error instead of axum's plain text.
    duration_secs: Option<String>,
}

/// `GET /api/lyrics`: synced or plain lyrics of a song.
async fn lyrics(
    State(app): State<Services>,
    Query(p): Query<LyricsParams>,
) -> ApiResult<Json<Lyrics>> {
    let required = |v: Option<String>, name: &str| {
        v.filter(|s| !s.trim().is_empty())
            .ok_or_else(|| ApiError(StatusCode::BAD_REQUEST, format!("{name} is required")))
    };
    let duration_secs = match p.duration_secs.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(v) => Some(v.parse::<u32>().map_err(|_| {
            ApiError(
                StatusCode::BAD_REQUEST,
                "durationSecs must be a whole number of seconds".into(),
            )
        })?),
    };
    let query = LyricsQuery {
        video_id: required(p.video_id, "videoId")?,
        title: required(p.title, "title")?,
        artists: required(p.artists, "artists")?,
        album: p.album.filter(|s| !s.trim().is_empty()),
        duration_secs,
    };
    match app.lyrics.lyrics(&query).await {
        Ok(Some(found)) => Ok(Json(found)),
        Ok(None) => Err(ApiError(StatusCode::NOT_FOUND, "no lyrics".into())),
        Err(e) => Err(ApiError(
            StatusCode::BAD_GATEWAY,
            format!("lyrics failed: {e:#}"),
        )),
    }
}

async fn state(State(app): State<Services>) -> Json<StateView> {
    Json(app.player.state().view(Instant::now()))
}

#[derive(Serialize)]
struct PlayNextResponse {
    id: EntryId,
    started: bool,
}

async fn queue_next(
    State(app): State<Services>,
    body: Result<Json<Song>, JsonRejection>,
) -> ApiResult<Json<PlayNextResponse>> {
    let Json(song) = body.map_err(|e| ApiError(StatusCode::BAD_REQUEST, e.body_text()))?;
    if song.video_id.trim().is_empty() {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "videoId must not be blank".into(),
        ));
    }
    let outcome = app.player.play_next(song).await?;
    Ok(Json(PlayNextResponse {
        id: outcome.id,
        started: outcome.started,
    }))
}

/// Checks a song list from a request body: 1..=500 songs, no blank videoId.
fn check_songs(songs: &[Song]) -> ApiResult<()> {
    if songs.is_empty() || songs.len() > MAX_QUEUE_SONGS {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            format!("songs must hold 1 to {MAX_QUEUE_SONGS} songs"),
        ));
    }
    if songs.iter().any(|s| s.video_id.trim().is_empty()) {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "videoId must not be blank".into(),
        ));
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct QueuePlayBody {
    songs: Vec<Song>,
    start_index: Option<usize>,
    #[serde(default)]
    shuffle: bool,
}

async fn queue_play(
    State(app): State<Services>,
    body: Result<Json<QueuePlayBody>, JsonRejection>,
) -> ApiResult<Json<Value>> {
    let Json(body) = body.map_err(|e| ApiError(StatusCode::BAD_REQUEST, e.body_text()))?;
    let mut songs = body.songs;
    check_songs(&songs)?;
    if body.shuffle {
        songs.shuffle(&mut rand::rng());
    } else {
        let start = body.start_index.unwrap_or(0);
        if start >= songs.len() {
            return Err(ApiError(
                StatusCode::BAD_REQUEST,
                "startIndex out of range".into(),
            ));
        }
        songs.drain(..start);
    }
    let count = app.player.play_all(songs).await?;
    Ok(Json(json!({ "count": count })))
}

#[derive(Deserialize)]
struct QueueAddBody {
    songs: Vec<Song>,
    at: AddAt,
}

async fn queue_add(
    State(app): State<Services>,
    body: Result<Json<QueueAddBody>, JsonRejection>,
) -> ApiResult<Json<Value>> {
    let Json(body) = body.map_err(|e| ApiError(StatusCode::BAD_REQUEST, e.body_text()))?;
    check_songs(&body.songs)?;
    let outcome = app.player.add(body.songs, body.at).await?;
    Ok(Json(
        json!({ "count": outcome.count, "started": outcome.started }),
    ))
}

async fn queue_remove(
    State(app): State<Services>,
    Path(entry_id): Path<EntryId>,
) -> ApiResult<StatusCode> {
    if app.player.remove(entry_id).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError(
            StatusCode::NOT_FOUND,
            format!("no upcoming entry {entry_id} (the playing song can't be removed)"),
        ))
    }
}

async fn pause(State(app): State<Services>) -> ApiResult<StatusCode> {
    app.player.pause().await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn resume(State(app): State<Services>) -> ApiResult<StatusCode> {
    app.player.resume().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `?entryId=`: act only if that entry is still current (see `docs/api.md`).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Expected {
    entry_id: Option<EntryId>,
}

/// 204 when it acted or nothing was named; 409 when the named entry has
/// already stopped being current.
fn acted(acted: bool, expected: Option<EntryId>) -> ApiResult<StatusCode> {
    match expected {
        Some(id) if !acted => Err(ApiError(
            StatusCode::CONFLICT,
            format!("entry {id} is no longer playing"),
        )),
        _ => Ok(StatusCode::NO_CONTENT),
    }
}

async fn skip(State(app): State<Services>, Query(q): Query<Expected>) -> ApiResult<StatusCode> {
    acted(app.player.skip(q.entry_id).await?, q.entry_id)
}

async fn previous(State(app): State<Services>, Query(q): Query<Expected>) -> ApiResult<StatusCode> {
    acted(app.player.previous(q.entry_id).await?, q.entry_id)
}

/// `?positionMs=&entryId=`: where to jump in the current song.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SeekTo {
    position_ms: u64,
    entry_id: Option<EntryId>,
}

async fn seek(State(app): State<Services>, Query(q): Query<SeekTo>) -> ApiResult<StatusCode> {
    let position = std::time::Duration::from_millis(q.position_ms);
    acted(app.player.seek(position, q.entry_id).await?, q.entry_id)
}

async fn favorites_list(State(app): State<Services>) -> Json<Vec<Song>> {
    Json(app.favorites.list())
}

async fn favorite_add(
    State(app): State<Services>,
    Path(video_id): Path<String>,
    body: Result<Json<Song>, JsonRejection>,
) -> ApiResult<StatusCode> {
    let Json(song) = body.map_err(|e| ApiError(StatusCode::BAD_REQUEST, e.body_text()))?;
    if song.video_id.trim().is_empty() || video_id.trim().is_empty() {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "videoId must not be blank".into(),
        ));
    }
    if song.video_id != video_id {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "videoId in the body must match the one in the path".into(),
        ));
    }
    app.favorites.add(song)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn favorite_remove(
    State(app): State<Services>,
    Path(video_id): Path<String>,
) -> ApiResult<StatusCode> {
    if app.favorites.remove(&video_id)? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError(
            StatusCode::NOT_FOUND,
            format!("{video_id} is not a favorite"),
        ))
    }
}

async fn favorites_shuffle(State(app): State<Services>) -> ApiResult<Json<Value>> {
    let mut songs = app.favorites.list();
    if songs.is_empty() {
        return Err(ApiError(StatusCode::CONFLICT, "no favorites".into()));
    }
    songs.shuffle(&mut rand::rng());
    let count = app.player.play_all(songs).await?;
    Ok(Json(json!({ "count": count })))
}

#[derive(Deserialize)]
struct CreatePlaylistBody {
    name: String,
    #[serde(default)]
    songs: Vec<Song>,
}

#[derive(Deserialize)]
struct NameBody {
    name: String,
}

#[derive(Deserialize)]
struct SongsBody {
    songs: Vec<Song>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OrderBody {
    video_ids: Vec<String>,
}

fn json_body<T>(body: Result<Json<T>, JsonRejection>) -> ApiResult<T> {
    body.map(|Json(v)| v)
        .map_err(|e| ApiError(StatusCode::BAD_REQUEST, e.body_text()))
}

async fn playlists_list(State(app): State<Services>) -> Json<Vec<PlaylistSummary>> {
    Json(app.playlists.list())
}

async fn playlist_create(
    State(app): State<Services>,
    body: Result<Json<CreatePlaylistBody>, JsonRejection>,
) -> ApiResult<(StatusCode, Json<Playlist>)> {
    let body = json_body(body)?;
    if !body.songs.is_empty() {
        check_songs(&body.songs)?;
    }
    let playlist = app.playlists.create(&body.name, body.songs)?;
    Ok((StatusCode::CREATED, Json(playlist)))
}

async fn playlist_get(
    State(app): State<Services>,
    Path(id): Path<String>,
) -> ApiResult<Json<Playlist>> {
    app.playlists
        .get(&id)
        .map(Json)
        .ok_or_else(|| PlaylistError::NotFound(id).into())
}

async fn playlist_rename(
    State(app): State<Services>,
    Path(id): Path<String>,
    body: Result<Json<NameBody>, JsonRejection>,
) -> ApiResult<Json<Playlist>> {
    let body = json_body(body)?;
    Ok(Json(app.playlists.rename(&id, &body.name)?))
}

async fn playlist_delete(
    State(app): State<Services>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    app.playlists.delete(&id)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn playlist_add_songs(
    State(app): State<Services>,
    Path(id): Path<String>,
    body: Result<Json<SongsBody>, JsonRejection>,
) -> ApiResult<Json<AddSongsOutcome>> {
    let body = json_body(body)?;
    check_songs(&body.songs)?;
    Ok(Json(app.playlists.add_songs(&id, body.songs)?))
}

async fn playlist_remove_song(
    State(app): State<Services>,
    Path((id, video_id)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    app.playlists.remove_song(&id, &video_id)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn playlist_reorder(
    State(app): State<Services>,
    Path(id): Path<String>,
    body: Result<Json<OrderBody>, JsonRejection>,
) -> ApiResult<Json<Playlist>> {
    let body = json_body(body)?;
    Ok(Json(app.playlists.reorder(&id, body.video_ids)?))
}

impl From<SpeakerError> for ApiError {
    fn from(e: SpeakerError) -> Self {
        match e {
            SpeakerError::Rejected(status, message) => ApiError(status, message),
            SpeakerError::Unreachable => {
                ApiError(StatusCode::BAD_GATEWAY, "Can't reach nidoo-sound".into())
            }
        }
    }
}

fn speaker_control(app: &Services) -> ApiResult<&SpeakerControl> {
    app.speakers.as_deref().ok_or_else(|| {
        ApiError(
            StatusCode::NOT_FOUND,
            "no speaker control (engine is not nidoo)".into(),
        )
    })
}

/// `GET /api/speakers`: the approved Nidoo speakers with volume and mute.
async fn speakers_list(State(app): State<Services>) -> ApiResult<Json<Vec<Speaker>>> {
    Ok(Json(speaker_control(&app)?.list().await?))
}

/// `PUT /api/speakers/{id}`: set a speaker's volume and/or mute.
async fn speaker_change(
    State(app): State<Services>,
    Path(id): Path<String>,
    body: Result<Json<SpeakerChange>, JsonRejection>,
) -> ApiResult<StatusCode> {
    let control = speaker_control(&app)?;
    let change = json_body(body)?;
    control.set(&id, &change).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn ws(State(app): State<Services>, upgrade: WebSocketUpgrade) -> Response {
    upgrade.on_upgrade(move |socket| ws_session(socket, app.player, app.favorites, app.playlists))
}

/// `{"type": "playlists", "playlists": [...]}`.
fn playlists_message(playlists: &[PlaylistSummary]) -> Message {
    let value = json!({ "type": "playlists", "playlists": playlists });
    Message::Text(value.to_string().into())
}

/// `{"type": "favorites", "songs": [...]}`.
fn favorites_message(songs: &[Song]) -> Message {
    let value = json!({ "type": "favorites", "songs": songs });
    Message::Text(value.to_string().into())
}

/// `{"type": "state", ...StateView}`, computed at send time.
fn state_message(state: &PlayerState) -> Message {
    let mut value = serde_json::to_value(state.view(Instant::now())).expect("state serializes");
    if let Value::Object(map) = &mut value {
        map.insert("type".into(), "state".into());
    }
    Message::Text(value.to_string().into())
}

async fn ws_session(
    mut socket: WebSocket,
    player: PlayerHandle,
    favorites: Favorites,
    playlists: Playlists,
) {
    let mut states = player.subscribe();
    let mut favs = favorites.subscribe();
    let mut lists = playlists.subscribe();
    let mut notices = player.notices();
    let first = state_message(&states.borrow_and_update().clone());
    if socket.send(first).await.is_err() {
        return;
    }
    let first_favorites = favorites_message(&favs.borrow_and_update());
    if socket.send(first_favorites).await.is_err() {
        return;
    }
    let first_playlists = playlists_message(&lists.borrow_and_update());
    if socket.send(first_playlists).await.is_err() {
        return;
    }
    loop {
        let msg = tokio::select! {
            changed = states.changed() => {
                if changed.is_err() {
                    break;
                }
                let state = states.borrow_and_update().clone();
                state_message(&state)
            }
            changed = favs.changed() => {
                if changed.is_err() {
                    break;
                }
                favorites_message(&favs.borrow_and_update())
            }
            changed = lists.changed() => {
                if changed.is_err() {
                    break;
                }
                playlists_message(&lists.borrow_and_update())
            }
            notice = notices.recv() => match notice {
                Ok(n) => Message::Text(serde_json::to_string(&n).expect("notice serializes").into()),
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => break,
            },
            incoming = socket.recv() => match incoming {
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                Some(Ok(_)) => continue, // clients don't send anything; ignore pings and text
            },
        };
        if socket.send(msg).await.is_err() {
            break;
        }
    }
}
