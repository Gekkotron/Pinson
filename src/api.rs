//! HTTP API and WebSocket (see `docs/api.md`).

use std::sync::Arc;

use axum::extract::rejection::JsonRejection;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use rand::seq::SliceRandom;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::broadcast::error::RecvError;
use tokio::time::Instant;

use crate::favorites::Favorites;
use crate::player::{PlayerHandle, PlayerState, StateView};
use crate::queue::{AddAt, EntryId};
use crate::search::Searcher;
use crate::song::Song;

/// Longest song list accepted by `queue/play` and `queue/add`.
pub const MAX_QUEUE_SONGS: usize = 500;

/// Everything the routes need; built once in `main` (and in tests).
#[derive(Clone)]
pub struct Services {
    pub player: PlayerHandle,
    pub searcher: Arc<dyn Searcher>,
    pub favorites: Favorites,
}

pub fn router(services: Services) -> Router {
    Router::new()
        .route("/api/search", get(search))
        .route("/api/state", get(state))
        .route("/api/queue/next", post(queue_next))
        .route("/api/queue/play", post(queue_play))
        .route("/api/queue/add", post(queue_add))
        .route("/api/queue/{entry_id}", delete(queue_remove))
        .route("/api/player/pause", post(pause))
        .route("/api/player/resume", post(resume))
        .route("/api/player/skip", post(skip))
        .route("/api/player/previous", post(previous))
        .route("/api/favorites", get(favorites_list))
        .route("/api/favorites/shuffle", post(favorites_shuffle))
        .route(
            "/api/favorites/{video_id}",
            put(favorite_add).delete(favorite_remove),
        )
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

type ApiResult<T> = Result<T, ApiError>;

#[derive(Deserialize)]
struct SearchParams {
    q: Option<String>,
}

async fn search(
    State(app): State<Services>,
    Query(params): Query<SearchParams>,
) -> ApiResult<Json<Vec<Song>>> {
    let q = params.q.unwrap_or_default();
    let q = q.trim();
    if q.is_empty() {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "q must not be blank".into(),
        ));
    }
    match app.searcher.search(q).await {
        Ok(songs) => Ok(Json(songs)),
        Err(e) => {
            tracing::warn!("search {q:?} failed: {e:#}");
            Err(ApiError(
                StatusCode::BAD_GATEWAY,
                format!("search failed: {e:#}"),
            ))
        }
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
    let start = body.start_index.unwrap_or(0);
    if start >= songs.len() {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "startIndex out of range".into(),
        ));
    }
    if body.shuffle {
        songs.shuffle(&mut rand::rng());
    } else {
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

async fn ws(State(app): State<Services>, upgrade: WebSocketUpgrade) -> Response {
    upgrade.on_upgrade(move |socket| ws_session(socket, app.player, app.favorites))
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

async fn ws_session(mut socket: WebSocket, player: PlayerHandle, favorites: Favorites) {
    let mut states = player.subscribe();
    let mut favs = favorites.subscribe();
    let mut notices = player.notices();
    let first = state_message(&states.borrow_and_update().clone());
    if socket.send(first).await.is_err() {
        return;
    }
    let first_favorites = favorites_message(&favs.borrow_and_update());
    if socket.send(first_favorites).await.is_err() {
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
