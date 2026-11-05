//! HTTP API and WebSocket tests against a player with a fake engine.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use futures_util::StreamExt;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tower::ServiceExt;

use pinson::api::{Services, router};
use pinson::artwork::ArtworkProxy;
use pinson::autoplay::autoplay_enabled;
use pinson::catalog::{Catalog, Collection, CollectionKind, CollectionSummary};
use pinson::db::Db;
use pinson::engine::fake::{FakeEngine, FakeSource};
use pinson::favorites::Favorites;
use pinson::history::History;
use pinson::lyrics::{LrcLibApi, LrcRecord, LyricsQuery, LyricsService};
use pinson::player::{Autoplay, spawn_player};
use pinson::playlists::Playlists;
use pinson::radio::fake::FakeRadio;
use pinson::search::{Searcher, Suggestion, SuggestionRun};
use pinson::song::Song;
use pinson::speakers::SpeakerControl;

#[derive(Default)]
struct FakeSearcher {
    fail: bool,
    suggest_delay: Option<Duration>,
    last_query: Mutex<Option<String>>,
}

#[async_trait]
impl Searcher for FakeSearcher {
    async fn search(&self, query: &str) -> anyhow::Result<Vec<Song>> {
        *self.last_query.lock().unwrap() = Some(query.to_owned());
        if self.fail {
            anyhow::bail!("YouTube Music is down");
        }
        Ok(vec![song("abc", "One More Time")])
    }

    async fn suggestions(&self, query: &str) -> anyhow::Result<Vec<Suggestion>> {
        *self.last_query.lock().unwrap() = Some(query.to_owned());
        if self.fail {
            anyhow::bail!("YouTube Music is down");
        }
        if let Some(delay) = self.suggest_delay {
            tokio::time::sleep(delay).await;
        }
        let run = |text: &str, bold| SuggestionRun {
            text: text.into(),
            bold,
        };
        Ok(vec![Suggestion {
            text: "daft punk".into(),
            runs: vec![run("daft pu", true), run("nk", false)],
        }])
    }
}

fn summary(kind: CollectionKind, id: &str, title: &str) -> CollectionSummary {
    CollectionSummary {
        kind,
        id: id.into(),
        title: title.into(),
        subtitle: Some("Daft Punk".into()),
        artwork_url: None,
    }
}

#[async_trait]
impl Catalog for FakeSearcher {
    async fn search_collections(
        &self,
        _query: &str,
        kind: CollectionKind,
    ) -> anyhow::Result<Vec<CollectionSummary>> {
        if self.fail {
            anyhow::bail!("YouTube Music is down");
        }
        Ok(vec![match kind {
            CollectionKind::Album => summary(kind, "MPREb_1", "Discovery"),
            CollectionKind::Playlist => summary(kind, "PL1", "House"),
        }])
    }

    async fn album(&self, id: &str) -> anyhow::Result<Option<Collection>> {
        if self.fail {
            anyhow::bail!("YouTube Music is down");
        }
        Ok((id == "MPREb_1").then(|| Collection {
            summary: summary(CollectionKind::Album, "MPREb_1", "Discovery"),
            songs: vec![song("a", "A"), song("b", "B")],
        }))
    }

    async fn playlist(&self, id: &str) -> anyhow::Result<Option<Collection>> {
        if self.fail {
            anyhow::bail!("YouTube Music is down");
        }
        // Like the real catalog: accepts the `VL` browse form, answers bare.
        let id = id.strip_prefix("VL").unwrap_or(id);
        Ok((id == "PL1").then(|| Collection {
            summary: summary(CollectionKind::Playlist, "PL1", "House"),
            songs: vec![song("a", "A")],
        }))
    }

    async fn youtube_lyrics(&self, _video_id: &str) -> anyhow::Result<Option<String>> {
        Ok(None)
    }
}

/// LRCLIB stand-in keyed on the title: "Boom" fails, "One More Time" has
/// synced lyrics, anything else has none.
struct FakeLrcLib;

#[async_trait]
impl LrcLibApi for FakeLrcLib {
    async fn get(&self, q: &LyricsQuery) -> anyhow::Result<Option<LrcRecord>> {
        match q.title.as_str() {
            "Boom" => anyhow::bail!("lrclib is down"),
            "One More Time" => Ok(Some(LrcRecord {
                duration: Some(320.0),
                instrumental: false,
                plain_lyrics: Some("One more time".into()),
                synced_lyrics: Some("[00:02.50]One more time\n[00:01.00]Intro".into()),
            })),
            _ => Ok(None),
        }
    }

    async fn search(&self, _q: &LyricsQuery) -> anyhow::Result<Vec<LrcRecord>> {
        Ok(Vec::new())
    }
}

fn song(id: &str, title: &str) -> Song {
    Song {
        video_id: id.into(),
        title: title.into(),
        artists: "Daft Punk".into(),
        album: Some("Discovery".into()),
        duration_secs: Some(320),
        artwork_url: None,
    }
}

fn services_with(searcher: Arc<FakeSearcher>, favorites: Favorites) -> Services {
    services_built(searcher, favorites, FakeRadio::default(), false)
}

fn services_with_radio(radio: FakeRadio, enabled: bool) -> Services {
    services_built(
        Arc::new(FakeSearcher::default()),
        Favorites::in_memory(),
        radio,
        enabled,
    )
}

fn services_built(
    searcher: Arc<FakeSearcher>,
    favorites: Favorites,
    radio: FakeRadio,
    enabled: bool,
) -> Services {
    let (_tx, rx) = mpsc::unbounded_channel();
    let engine = FakeEngine::new(Duration::ZERO);
    let db = Db::in_memory();
    let player = spawn_player(
        Box::new(engine),
        rx,
        Arc::new(FakeSource::default()),
        Autoplay {
            radio: Arc::new(radio),
            history: History::new(db.clone()),
            enabled,
        },
    );
    Services {
        player,
        db,
        searcher: searcher.clone(),
        catalog: searcher.clone(),
        favorites,
        playlists: Playlists::in_memory(),
        artwork: Arc::new(ArtworkProxy::youtube()),
        lyrics: Arc::new(LyricsService::new(Arc::new(FakeLrcLib), searcher)),
        speakers: None,
    }
}

fn services() -> Services {
    services_with(Arc::new(FakeSearcher::default()), Favorites::in_memory())
}

fn app_from(services: Services) -> Router {
    router(services)
}

fn app_with(searcher: Arc<FakeSearcher>) -> Router {
    app_with_favorites(searcher, Favorites::in_memory())
}

fn app_with_favorites(searcher: Arc<FakeSearcher>, favorites: Favorites) -> Router {
    app_from(services_with(searcher, favorites))
}

fn app() -> Router {
    app_from(services())
}

async fn send(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(v) => {
            req = req.header(header::CONTENT_TYPE, "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    let res = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

#[tokio::test]
async fn artwork_rejects_other_hosts() {
    let (status, body) = send(
        &app(),
        "GET",
        "/api/artwork?url=https://evil.example/a.png",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].is_string());
}

#[tokio::test]
async fn search_returns_songs() {
    let (status, body) = send(&app(), "GET", "/api/search?q=daft", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body[0]["videoId"], "abc");
    assert_eq!(body[0]["title"], "One More Time");
}

#[tokio::test]
async fn search_rejects_blank_query() {
    for uri in ["/api/search?q=", "/api/search?q=%20%20", "/api/search"] {
        let (status, body) = send(&app(), "GET", uri, None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
        assert!(body["error"].is_string(), "{uri}");
    }
}

#[tokio::test]
async fn search_kind_albums_and_playlists() {
    let app = app();
    let (status, body) = send(&app, "GET", "/api/search?q=daft&kind=albums", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body[0]["kind"], "album");
    assert_eq!(body[0]["id"], "MPREb_1");
    assert_eq!(body[0]["subtitle"], "Daft Punk");
    let (status, body) = send(&app, "GET", "/api/search?q=daft&kind=playlists", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body[0]["kind"], "playlist");
    assert_eq!(body[0]["id"], "PL1");
}

#[tokio::test]
async fn search_kind_default_is_songs_and_bad_kind_is_400() {
    let app = app();
    let (status, body) = send(&app, "GET", "/api/search?q=daft", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body[0]["videoId"], "abc");
    let (status, body) = send(&app, "GET", "/api/search?q=daft&kind=songs", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body[0]["videoId"], "abc");
    let (status, body) = send(&app, "GET", "/api/search?q=daft&kind=artists", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap().contains("artists"));
}

#[tokio::test]
async fn open_album_and_playlist() {
    let app = app();
    let (status, body) = send(&app, "GET", "/api/albums/MPREb_1", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["kind"], "album");
    assert_eq!(body["title"], "Discovery");
    assert_eq!(body["songs"][0]["videoId"], "a");
    assert_eq!(body["songs"][1]["videoId"], "b");
    let (status, body) = send(&app, "GET", "/api/ytplaylists/PL1", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["kind"], "playlist");
    assert_eq!(body["id"], "PL1");
    assert_eq!(body["songs"].as_array().unwrap().len(), 1);
    let (status, vl) = send(&app, "GET", "/api/ytplaylists/VLPL1", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(vl, body);
}

#[tokio::test]
async fn open_unknown_collection_is_404_and_upstream_error_502() {
    let app = app();
    let (status, body) = send(&app, "GET", "/api/albums/nope", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "no album nope");
    let (status, body) = send(&app, "GET", "/api/ytplaylists/nope", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "no playlist nope");

    let failing = app_with(Arc::new(FakeSearcher {
        fail: true,
        ..FakeSearcher::default()
    }));
    let (status, body) = send(&failing, "GET", "/api/albums/MPREb_1", None).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .starts_with("album failed: ")
    );
    let (status, body) = send(&failing, "GET", "/api/ytplaylists/PL1", None).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .starts_with("playlist failed: ")
    );
    let (status, _) = send(&failing, "GET", "/api/search?q=x&kind=albums", None).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn search_upstream_error_is_502() {
    let searcher = Arc::new(FakeSearcher {
        fail: true,
        ..FakeSearcher::default()
    });
    let (status, body) = send(&app_with(searcher), "GET", "/api/search?q=daft", None).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("YouTube Music is down")
    );
}

#[tokio::test]
async fn search_unicode_query_passes_through() {
    let searcher = Arc::new(FakeSearcher::default());
    let app = app_with(searcher.clone());
    let (status, _) = send(&app, "GET", "/api/search?q=T%C3%A9l%C3%A9popmusik", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        searcher.last_query.lock().unwrap().as_deref(),
        Some("Télépopmusik")
    );
}

#[tokio::test]
async fn queue_next_then_state_shows_current() {
    let app = app();
    let song = serde_json::to_value(song("abc", "One More Time")).unwrap();
    let (status, body) = send(&app, "POST", "/api/queue/next", Some(song)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["started"], true);
    let id = body["id"].as_u64().unwrap();

    let (status, state) = send(&app, "GET", "/api/state", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(state["current"]["id"], id);
    assert_eq!(state["current"]["song"]["videoId"], "abc");
    assert_eq!(state["status"], "playing");
    assert!(state["positionMs"].is_u64());
    assert!(state["positionRunning"].is_boolean());
    assert_eq!(state["upcoming"], json!([]));
}

#[tokio::test]
async fn queue_next_rejects_bad_body() {
    let app = app();
    for body in [
        json!({"title": "no id"}),
        json!({"videoId": "", "title": "t", "artists": "a"}),
    ] {
        let (status, res) = send(&app, "POST", "/api/queue/next", Some(body)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(res["error"].is_string());
    }
}

#[tokio::test]
async fn delete_unknown_is_404() {
    let (status, _) = send(&app(), "DELETE", "/api/queue/999", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn delete_upcoming_is_204() {
    let app = app();
    send(
        &app,
        "POST",
        "/api/queue/next",
        Some(serde_json::to_value(song("a", "A")).unwrap()),
    )
    .await;
    let (_, x) = send(
        &app,
        "POST",
        "/api/queue/next",
        Some(serde_json::to_value(song("x", "X")).unwrap()),
    )
    .await;
    let uri = format!("/api/queue/{}", x["id"]);
    let (status, _) = send(&app, "DELETE", &uri, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, state) = send(&app, "GET", "/api/state", None).await;
    assert_eq!(state["upcoming"], json!([]));
}

#[tokio::test]
async fn player_controls_return_204() {
    let app = app();
    send(
        &app,
        "POST",
        "/api/queue/next",
        Some(serde_json::to_value(song("a", "A")).unwrap()),
    )
    .await;
    for action in ["pause", "resume", "skip", "previous"] {
        let (status, _) = send(&app, "POST", &format!("/api/player/{action}"), None).await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{action}");
    }
}

async fn next_json<S>(ws: &mut S) -> Value
where
    S: StreamExt<
            Item = Result<
                tokio_tungstenite::tungstenite::Message,
                tokio_tungstenite::tungstenite::Error,
            >,
        > + Unpin,
{
    let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
        .await
        .expect("message within 5 s")
        .expect("stream open")
        .expect("valid message");
    serde_json::from_str(msg.to_text().unwrap()).unwrap()
}

#[tokio::test]
async fn ws_sends_state_on_connect_and_on_change() {
    let app = app();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(axum::serve(listener, app.clone()).into_future());

    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/api/ws"))
        .await
        .unwrap();
    let first = next_json(&mut ws).await;
    assert_eq!(first["type"], "state");
    assert_eq!(first["status"], "stopped");
    let v0 = first["version"].as_u64().unwrap();
    assert_eq!(next_json(&mut ws).await["type"], "favorites");
    assert_eq!(next_json(&mut ws).await["type"], "playlists");

    send(
        &app,
        "POST",
        "/api/queue/next",
        Some(serde_json::to_value(song("a", "A")).unwrap()),
    )
    .await;
    let second = next_json(&mut ws).await;
    assert_eq!(second["type"], "state");
    assert!(second["version"].as_u64().unwrap() > v0);
    assert_eq!(second["current"]["song"]["videoId"], "a");
}

async fn get_raw(app: Router, uri: &str) -> (StatusCode, String, String) {
    let req = Request::builder().uri(uri).body(Body::empty()).unwrap();
    let res = app.oneshot(req).await.unwrap();
    let status = res.status();
    let ctype = res
        .headers()
        .get(header::CONTENT_TYPE)
        .map(|v| v.to_str().unwrap().to_owned())
        .unwrap_or_default();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, ctype, String::from_utf8_lossy(&bytes).into_owned())
}

#[tokio::test]
async fn index_is_served() {
    let app = app().merge(pinson::web::web_routes());
    let (status, ctype, body) = get_raw(app.clone(), "/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(ctype.starts_with("text/html"), "{ctype}");
    assert!(body.contains(r#"id="view""#));
    assert!(body.contains("/js/main.js"));

    let (status, ctype, _) = get_raw(app.clone(), "/js/main.js").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        ctype.starts_with("application/javascript") || ctype.starts_with("text/javascript"),
        "{ctype}"
    );

    for path in [
        "/js/views/home.js",
        "/js/views/search.js",
        "/js/views/collection.js",
        "/js/views/playlist.js",
        "/js/views/lyrics.js",
    ] {
        let (status, ctype, _) = get_raw(app.clone(), path).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            ctype.starts_with("application/javascript") || ctype.starts_with("text/javascript"),
            "{ctype}"
        );
    }

    let (status, ctype, _) = get_raw(app.clone(), "/css/app.css").await;
    assert_eq!(status, StatusCode::OK);
    assert!(ctype.starts_with("text/css"), "{ctype}");

    let (status, _, _) = get_raw(app, "/nope.txt").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn skip_with_stale_entry_id_is_409() {
    let app = app();
    let (_, a) = send(
        &app,
        "POST",
        "/api/queue/next",
        Some(serde_json::to_value(song("a", "A")).unwrap()),
    )
    .await;
    send(
        &app,
        "POST",
        "/api/queue/next",
        Some(serde_json::to_value(song("x", "X")).unwrap()),
    )
    .await;
    let a_id = a["id"].as_u64().unwrap();
    let (status, _) = send(
        &app,
        "POST",
        &format!("/api/player/skip?entryId={a_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "A is current: skip acts");
    for action in ["skip", "previous"] {
        let uri = format!("/api/player/{action}?entryId={a_id}");
        let (status, body) = send(&app, "POST", &uri, None).await;
        assert_eq!(status, StatusCode::CONFLICT, "{action}");
        assert!(body["error"].is_string(), "{action}");
    }
    let (_, state) = send(&app, "GET", "/api/state", None).await;
    assert_eq!(state["current"]["song"]["videoId"], "x");
}

#[tokio::test]
async fn seek_jumps_in_the_current_song() {
    let app = app();
    let (_, a) = send(&app, "POST", "/api/queue/next", Some(song_json("a", "A"))).await;
    let a_id = a["id"].as_u64().unwrap();
    let uri = format!("/api/player/seek?positionMs=30000&entryId={a_id}");
    let (status, _) = send(&app, "POST", &uri, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, state) = send(&app, "GET", "/api/state", None).await;
    assert!(state["positionMs"].as_u64().unwrap() >= 30_000, "{state}");

    let (status, _) = send(&app, "POST", "/api/player/seek", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "positionMs is required");
    let stale = format!("/api/player/seek?positionMs=1000&entryId={}", a_id + 99);
    let (status, body) = send(&app, "POST", &stale, None).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(body["error"].is_string());
}

fn song_json(id: &str, title: &str) -> Value {
    serde_json::to_value(song(id, title)).unwrap()
}

#[tokio::test]
async fn favorites_start_empty() {
    let (status, body) = send(&app(), "GET", "/api/favorites", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!([]));
}

#[tokio::test]
async fn favorites_put_list_newest_first_and_idempotent() {
    let app = app();
    for (id, title) in [("a", "A"), ("b", "B")] {
        let (status, _) = send(
            &app,
            "PUT",
            &format!("/api/favorites/{id}"),
            Some(song_json(id, title)),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
    }
    // Again, with refreshed metadata: position is kept.
    let (status, _) = send(
        &app,
        "PUT",
        "/api/favorites/a",
        Some(song_json("a", "A renamed")),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, body) = send(&app, "GET", "/api/favorites", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().unwrap().len(), 2);
    assert_eq!(body[0]["videoId"], "b");
    assert_eq!(body[1]["videoId"], "a");
    assert_eq!(body[1]["title"], "A renamed");
    assert_eq!(body[0]["durationSecs"], 320);
}

#[tokio::test]
async fn favorites_put_rejects_bad_requests() {
    let app = app();
    let cases = [
        ("/api/favorites/a", json!({"title": "no id"})),
        ("/api/favorites/a", song_json("b", "B")),
        ("/api/favorites/%20", song_json(" ", "blank")),
        ("/api/favorites/a", song_json("", "empty")),
    ];
    for (uri, body) in cases {
        let (status, res) = send(&app, "PUT", uri, Some(body.clone())).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri} {body}");
        assert!(res["error"].is_string(), "{uri} {body}");
    }
    let (_, list) = send(&app, "GET", "/api/favorites", None).await;
    assert_eq!(list, json!([]));
}

#[tokio::test]
async fn favorites_delete() {
    let app = app();
    send(&app, "PUT", "/api/favorites/a", Some(song_json("a", "A"))).await;
    let (status, _) = send(&app, "DELETE", "/api/favorites/a", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, body) = send(&app, "DELETE", "/api/favorites/a", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body["error"].is_string());
    let (_, list) = send(&app, "GET", "/api/favorites", None).await;
    assert_eq!(list, json!([]));
}

#[tokio::test]
async fn favorites_shuffle_with_none_is_409() {
    let app = app();
    let (status, body) = send(&app, "POST", "/api/favorites/shuffle", None).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"], "no favorites");
    let (_, state) = send(&app, "GET", "/api/state", None).await;
    assert_eq!(state["status"], "stopped");
}

#[tokio::test]
async fn favorites_shuffle_replaces_the_queue_and_plays() {
    let app = app();
    for id in ["f1", "f2", "f3"] {
        send(
            &app,
            "PUT",
            &format!("/api/favorites/{id}"),
            Some(song_json(id, id)),
        )
        .await;
    }
    send(
        &app,
        "POST",
        "/api/queue/next",
        Some(song_json("old", "Old")),
    )
    .await;
    send(
        &app,
        "POST",
        "/api/queue/next",
        Some(song_json("older", "Older")),
    )
    .await;
    send(&app, "POST", "/api/player/pause", None).await;

    let (status, body) = send(&app, "POST", "/api/favorites/shuffle", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"count": 3}));

    let (_, state) = send(&app, "GET", "/api/state", None).await;
    assert_eq!(state["status"], "playing");
    let mut ids = vec![
        state["current"]["song"]["videoId"]
            .as_str()
            .unwrap()
            .to_owned(),
    ];
    for entry in state["upcoming"].as_array().unwrap() {
        ids.push(entry["song"]["videoId"].as_str().unwrap().to_owned());
    }
    ids.sort();
    assert_eq!(ids, ["f1", "f2", "f3"]);

    // Shuffling doesn't touch the favorites themselves.
    let (_, list) = send(&app, "GET", "/api/favorites", None).await;
    assert_eq!(list.as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn ws_sends_favorites_after_state_and_on_change() {
    let app = app();
    send(&app, "PUT", "/api/favorites/a", Some(song_json("a", "A"))).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(axum::serve(listener, app.clone()).into_future());

    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/api/ws"))
        .await
        .unwrap();
    assert_eq!(next_json(&mut ws).await["type"], "state");
    let favs = next_json(&mut ws).await;
    assert_eq!(favs["type"], "favorites");
    assert_eq!(favs["songs"].as_array().unwrap().len(), 1);
    assert_eq!(favs["songs"][0]["videoId"], "a");
    assert_eq!(next_json(&mut ws).await["type"], "playlists");

    send(&app, "PUT", "/api/favorites/b", Some(song_json("b", "B"))).await;
    let favs = next_json(&mut ws).await;
    assert_eq!(favs["type"], "favorites");
    assert_eq!(favs["songs"][0]["videoId"], "b");
    assert_eq!(favs["songs"][1]["videoId"], "a");

    send(&app, "DELETE", "/api/favorites/a", None).await;
    let favs = next_json(&mut ws).await;
    assert_eq!(favs["songs"].as_array().unwrap().len(), 1);
}

async fn connect_ws(
    app: &Router,
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(axum::serve(listener, app.clone()).into_future());
    let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/api/ws"))
        .await
        .unwrap();
    ws
}

#[tokio::test]
async fn playlists_crud_flow() {
    let app = app();
    let (status, created) = send(
        &app,
        "POST",
        "/api/playlists",
        Some(json!({"name": "Party"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let id = created["id"].as_str().unwrap().to_owned();
    assert_eq!(created["name"], "Party");
    assert_eq!(created["songs"], json!([]));

    let (status, list) = send(&app, "GET", "/api/playlists", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["id"], id);
    assert_eq!(list[0]["name"], "Party");
    assert_eq!(list[0]["songCount"], 0);
    assert_eq!(list[0]["artworkUrls"], json!([]));
    assert!(list[0]["updatedAt"].is_string());

    let base = format!("/api/playlists/{id}");
    let (status, body) = send(
        &app,
        "POST",
        &format!("{base}/songs"),
        Some(json!({"songs": songs(&["a", "b"])})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"added": 2, "skipped": 0}));

    let (status, body) = send(&app, "PATCH", &base, Some(json!({"name": "Kitchen"}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"], "Kitchen");

    let (status, body) = send(
        &app,
        "PUT",
        &format!("{base}/order"),
        Some(json!({"videoIds": ["b", "a"]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["songs"][0]["videoId"], "b");
    assert_eq!(body["songs"][1]["videoId"], "a");

    let (status, body) = send(&app, "GET", &base, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["songs"].as_array().unwrap().len(), 2);

    let (status, _) = send(&app, "DELETE", &format!("{base}/songs/a"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, body) = send(&app, "GET", &base, None).await;
    assert_eq!(body["songs"].as_array().unwrap().len(), 1);

    let (status, _) = send(&app, "DELETE", &base, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = send(&app, "GET", &base, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn playlists_create_with_songs_is_a_copy() {
    let app = app();
    let (status, body) = send(
        &app,
        "POST",
        "/api/playlists",
        Some(json!({"name": "Discovery", "songs": songs(&["a", "b"])})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["songs"][0]["videoId"], "a");
    assert_eq!(body["songs"][1]["videoId"], "b");
    let (_, list) = send(&app, "GET", "/api/playlists", None).await;
    assert_eq!(list[0]["songCount"], 2);
}

#[tokio::test]
async fn playlists_errors() {
    let app = app();
    let (status, body) = send(&app, "POST", "/api/playlists", Some(json!({"name": "  "}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].is_string());
    let (status, _) = send(&app, "POST", "/api/playlists", Some(json!({"nom": "x"}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = send(
        &app,
        "POST",
        "/api/playlists",
        Some(json!({"name": "x", "songs": songs(&[" "])})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    for method in ["GET", "DELETE"] {
        let (status, body) = send(&app, method, "/api/playlists/nope", None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method}");
        assert!(body["error"].is_string());
    }
    let (status, _) = send(
        &app,
        "PATCH",
        "/api/playlists/nope",
        Some(json!({"name": "x"})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (_, created) = send(
        &app,
        "POST",
        "/api/playlists",
        Some(json!({"name": "P", "songs": songs(&["a", "b"])})),
    )
    .await;
    let base = format!("/api/playlists/{}", created["id"].as_str().unwrap());

    let (status, _) = send(&app, "DELETE", &format!("{base}/songs/zzz"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = send(&app, "PATCH", &base, Some(json!({"name": ""}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = send(
        &app,
        "POST",
        &format!("{base}/songs"),
        Some(json!({"songs": []})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let order = format!("{base}/order");
    let (status, _) = send(&app, "PUT", &order, Some(json!({"videoIds": ["a"]}))).await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) = send(&app, "PUT", &order, Some(json!({"videoIds": ["a", "a"]}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = send(&app, "PUT", &order, Some(json!({"ids": []}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // Nothing above changed the order.
    let (_, body) = send(&app, "GET", &base, None).await;
    assert_eq!(body["songs"][0]["videoId"], "a");
}

#[tokio::test]
async fn ws_sends_playlists_after_favorites_and_on_change() {
    let app = app();
    let mut ws = connect_ws(&app).await;
    assert_eq!(next_json(&mut ws).await["type"], "state");
    assert_eq!(next_json(&mut ws).await["type"], "favorites");
    let first = next_json(&mut ws).await;
    assert_eq!(first["type"], "playlists");
    assert_eq!(first["playlists"], json!([]));

    send(
        &app,
        "POST",
        "/api/playlists",
        Some(json!({"name": "Party"})),
    )
    .await;
    let msg = next_json(&mut ws).await;
    assert_eq!(msg["type"], "playlists");
    assert_eq!(msg["playlists"].as_array().unwrap().len(), 1);
    assert_eq!(msg["playlists"][0]["name"], "Party");
    assert_eq!(msg["playlists"][0]["songCount"], 0);
}

#[tokio::test]
async fn ws_sends_playlists_on_delete() {
    let app = app();
    let (_, created) = send(
        &app,
        "POST",
        "/api/playlists",
        Some(json!({"name": "Party"})),
    )
    .await;
    let id = created["id"].as_str().unwrap();
    let mut ws = connect_ws(&app).await;
    for expected in ["state", "favorites", "playlists"] {
        assert_eq!(next_json(&mut ws).await["type"], expected);
    }
    send(&app, "DELETE", &format!("/api/playlists/{id}"), None).await;
    let msg = next_json(&mut ws).await;
    assert_eq!(msg["type"], "playlists");
    assert_eq!(msg["playlists"], json!([]));
}

fn songs(ids: &[&str]) -> Value {
    Value::Array(
        ids.iter()
            .map(|id| serde_json::to_value(song(id, id)).unwrap())
            .collect(),
    )
}

fn video_ids(state: &Value) -> (String, Vec<String>) {
    let current = state["current"]["song"]["videoId"]
        .as_str()
        .unwrap()
        .to_owned();
    let upcoming = state["upcoming"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["song"]["videoId"].as_str().unwrap().to_owned())
        .collect();
    (current, upcoming)
}

#[tokio::test]
async fn queue_play_replaces_queue_from_start_index() {
    let app = app();
    let (status, body) = send(
        &app,
        "POST",
        "/api/queue/play",
        Some(json!({ "songs": songs(&["a", "b", "c"]), "startIndex": 1 })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({ "count": 2 }));
    let (_, state) = send(&app, "GET", "/api/state", None).await;
    assert_eq!(video_ids(&state), ("b".to_owned(), vec!["c".to_owned()]));
    assert_eq!(state["status"], "playing");
}

#[tokio::test]
async fn queue_play_shuffle_keeps_all_songs() {
    let app = app();
    let (status, body) = send(
        &app,
        "POST",
        "/api/queue/play",
        Some(json!({ "songs": songs(&["a", "b", "c"]), "startIndex": 2, "shuffle": true })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({ "count": 3 }));
    let (_, state) = send(&app, "GET", "/api/state", None).await;
    let (current, upcoming) = video_ids(&state);
    let mut all = upcoming;
    all.push(current);
    all.sort();
    assert_eq!(all, ["a", "b", "c"]);
}

#[tokio::test]
async fn queue_play_rejects_bad_lists() {
    let app = app();
    let (status, _) = send(
        &app,
        "POST",
        "/api/queue/play",
        Some(json!({ "songs": [] })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let many: Vec<String> = (0..501).map(|i| format!("s{i}")).collect();
    let many: Vec<&str> = many.iter().map(String::as_str).collect();
    let (status, _) = send(
        &app,
        "POST",
        "/api/queue/play",
        Some(json!({ "songs": songs(&many) })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, body) = send(
        &app,
        "POST",
        "/api/queue/play",
        Some(json!({ "songs": songs(&["a", "b", "c"]), "startIndex": 3 })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body, json!({ "error": "startIndex out of range" }));

    let (status, body) = send(
        &app,
        "POST",
        "/api/queue/play",
        Some(json!({ "songs": songs(&["a", " "]) })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body, json!({ "error": "videoId must not be blank" }));
}

#[tokio::test]
async fn queue_add_next_and_end() {
    let app = app();
    send(
        &app,
        "POST",
        "/api/queue/next",
        Some(serde_json::to_value(song("a", "a")).unwrap()),
    )
    .await;
    let (status, body) = send(
        &app,
        "POST",
        "/api/queue/add",
        Some(json!({ "songs": songs(&["x", "y"]), "at": "next" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({ "count": 2, "started": false }));
    send(
        &app,
        "POST",
        "/api/queue/add",
        Some(json!({ "songs": songs(&["z"]), "at": "end" })),
    )
    .await;
    let (_, state) = send(&app, "GET", "/api/state", None).await;
    let (current, upcoming) = video_ids(&state);
    assert_eq!(current, "a");
    assert_eq!(upcoming, ["x", "y", "z"]);
}

#[tokio::test]
async fn queue_add_when_idle_starts() {
    let app = app();
    let (status, body) = send(
        &app,
        "POST",
        "/api/queue/add",
        Some(json!({ "songs": songs(&["x"]), "at": "end" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({ "count": 1, "started": true }));
    let (_, state) = send(&app, "GET", "/api/state", None).await;
    assert_eq!(video_ids(&state).0, "x");
}

#[tokio::test]
async fn queue_add_rejects_bad_at() {
    let (status, body) = send(
        &app(),
        "POST",
        "/api/queue/add",
        Some(json!({ "songs": songs(&["x"]), "at": "middle" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].is_string());
}

#[tokio::test]
async fn stale_entry_id_after_queue_play_is_409() {
    let app = app();
    send(
        &app,
        "POST",
        "/api/queue/play",
        Some(json!({ "songs": songs(&["a"]) })),
    )
    .await;
    let (_, state) = send(&app, "GET", "/api/state", None).await;
    let old = state["current"]["id"].as_u64().unwrap();
    send(
        &app,
        "POST",
        "/api/queue/play",
        Some(json!({ "songs": songs(&["b"]) })),
    )
    .await;
    let (status, _) = send(
        &app,
        "POST",
        &format!("/api/player/skip?entryId={old}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn queue_play_shuffle_ignores_out_of_range_start_index() {
    let (status, body) = send(
        &app(),
        "POST",
        "/api/queue/play",
        Some(json!({ "songs": songs(&["a", "b", "c"]), "startIndex": 5, "shuffle": true })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({ "count": 3 }));
}

#[tokio::test]
async fn lyrics_route_ok_404_400() {
    let app = app();
    let (status, body) = send(
        &app,
        "GET",
        "/api/lyrics?videoId=abc&title=One%20More%20Time&artists=Daft%20Punk&durationSecs=320",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["videoId"], "abc");
    assert_eq!(body["source"], "lrclib");
    assert_eq!(body["synced"], true);
    assert_eq!(body["instrumental"], false);
    assert_eq!(
        body["lines"],
        json!([
            {"timeMs": 1000, "text": "Intro"},
            {"timeMs": 2500, "text": "One more time"}
        ])
    );

    let (status, body) = send(
        &app,
        "GET",
        "/api/lyrics?videoId=x&title=Nothing&artists=Nobody",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body, json!({"error": "no lyrics"}));

    for uri in [
        "/api/lyrics",
        "/api/lyrics?title=t&artists=a",
        "/api/lyrics?videoId=x&artists=a",
        "/api/lyrics?videoId=x&title=t",
        "/api/lyrics?videoId=%20&title=t&artists=a",
        "/api/lyrics?videoId=x&title=t&artists=a&durationSecs=abc",
        "/api/lyrics?videoId=x&title=t&artists=a&durationSecs=-3",
    ] {
        let (status, body) = send(&app, "GET", uri, None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
        assert!(body["error"].is_string());
    }

    // An empty durationSecs counts as absent.
    let (status, _) = send(
        &app,
        "GET",
        "/api/lyrics?videoId=abc&title=One%20More%20Time&artists=Daft%20Punk&durationSecs=",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = send(
        &app,
        "GET",
        "/api/lyrics?videoId=b&title=Boom&artists=Nobody",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .starts_with("lyrics failed: ")
    );
}

/// Hits the real LRCLIB: `cargo test -- --ignored live_lrclib`.
#[tokio::test]
#[ignore = "hits lrclib.net"]
async fn live_lrclib_one_more_time_is_synced() {
    let lrclib = pinson::lyrics::LrcLib::new("https://lrclib.net");
    let svc = LyricsService::new(Arc::new(lrclib), Arc::new(FakeSearcher::default()));
    let found = svc
        .lyrics(&LyricsQuery {
            video_id: "FGBhQbmPwH8".into(),
            title: "One More Time".into(),
            artists: "Daft Punk".into(),
            album: None,
            duration_secs: Some(320),
        })
        .await
        .unwrap()
        .expect("lyrics exist");
    assert!(found.synced, "{found:?}");
    assert!(!found.lines.is_empty());
}

type Recorded = Arc<Mutex<Vec<(String, Value)>>>;

/// nidoo-sound stand-in: lists one Kitchen speaker after `delay`, and
/// records the PUTs (204 for `0123456789ab`, 404 otherwise).
async fn fake_nidoo(delay: Duration) -> (String, Recorded) {
    use axum::Json;
    use axum::extract::{Path, State};
    use axum::response::IntoResponse;
    use axum::routing::{get, put};

    let recorded: Recorded = Arc::default();
    let app =
        Router::new()
            .route(
                "/players",
                get(move || async move {
                    tokio::time::sleep(delay).await;
                    Json(json!([{
                        "id": "0123456789ab",
                        "name": "Kitchen",
                        "volume": 45,
                        "muted": false,
                        "connected": true
                    }]))
                }),
            )
            .route(
                "/players/{id}",
                put(
                    |State(rec): State<Recorded>,
                     Path(id): Path<String>,
                     Json(body): Json<Value>| async move {
                        if id == "0123456789ab" {
                            rec.lock().unwrap().push((id, body));
                            StatusCode::NO_CONTENT.into_response()
                        } else {
                            (
                                StatusCode::NOT_FOUND,
                                Json(json!({"error": "no approved speaker"})),
                            )
                                .into_response()
                        }
                    },
                ),
            )
            .with_state(recorded.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), recorded)
}

fn app_with_speakers(base: String, timeout: Duration) -> Router {
    let mut services = services();
    services.speakers = Some(Arc::new(SpeakerControl::new(base, timeout)));
    app_from(services)
}

#[tokio::test]
async fn speakers_list_is_relayed() {
    let (base, _) = fake_nidoo(Duration::ZERO).await;
    let app = app_with_speakers(base, Duration::from_secs(2));
    let (status, body) = send(&app, "GET", "/api/speakers", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body[0]["name"], "Kitchen");
    assert_eq!(body[0]["volume"], 45);
}

#[tokio::test]
async fn speaker_change_is_relayed() {
    let (base, recorded) = fake_nidoo(Duration::ZERO).await;
    let app = app_with_speakers(base, Duration::from_secs(2));
    let (status, _) = send(
        &app,
        "PUT",
        "/api/speakers/0123456789ab",
        Some(json!({"volume": 30, "muted": true})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        *recorded.lock().unwrap(),
        vec![(
            "0123456789ab".to_owned(),
            json!({"volume": 30, "muted": true})
        )]
    );
}

#[tokio::test]
async fn speaker_404_passes_through() {
    let (base, _) = fake_nidoo(Duration::ZERO).await;
    let app = app_with_speakers(base, Duration::from_secs(2));
    let (status, body) = send(
        &app,
        "PUT",
        "/api/speakers/ffffffffffff",
        Some(json!({"volume": 30})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "no approved speaker");
}

#[tokio::test]
async fn speaker_bad_id_is_404_without_relaying() {
    let (base, seen) = fake_nidoo(Duration::ZERO).await;
    let app = app_with_speakers(base, Duration::from_secs(2));
    for path in [
        "/api/speakers/..",
        "/api/speakers/a%2Fb",
        "/api/speakers/x%3Fy",
    ] {
        let (status, body) = send(&app, "PUT", path, Some(json!({"volume": 30}))).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
        assert_eq!(body["error"], "unknown speaker", "{path}");
    }
    assert!(seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn speakers_unreachable_is_502() {
    let app = app_with_speakers("http://127.0.0.1:1".into(), Duration::from_secs(2));
    let (status, body) = send(&app, "GET", "/api/speakers", None).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(body["error"], "Can't reach nidoo-sound");
}

#[tokio::test]
async fn speakers_timeout_is_502() {
    let (base, _) = fake_nidoo(Duration::from_millis(500)).await;
    let app = app_with_speakers(base, Duration::from_millis(100));
    let (status, body) = send(&app, "GET", "/api/speakers", None).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(body["error"], "Can't reach nidoo-sound");
}

#[tokio::test]
async fn speakers_off_without_nidoo_engine() {
    let app = app();
    let (status, _) = send(&app, "GET", "/api/speakers", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = send(
        &app,
        "PUT",
        "/api/speakers/0123456789ab",
        Some(json!({"volume": 30})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

fn radio_song(id: &str) -> Song {
    song(id, id)
}

/// Waits (real time) until `/api/state` lists suggestions.
async fn wait_for_suggestions(app: &Router) -> Value {
    for _ in 0..200 {
        let (_, state) = send(app, "GET", "/api/state", None).await;
        if state["autoplay"]["songs"]
            .as_array()
            .is_some_and(|s| !s.is_empty())
        {
            return state;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("no suggestions within 2 s");
}

async fn queue_next(app: &Router, id: &str) {
    send(
        app,
        "POST",
        "/api/queue/next",
        Some(serde_json::to_value(song(id, id)).unwrap()),
    )
    .await;
}

#[tokio::test]
async fn autoplay_switch_persists_and_shows_in_state() {
    let services = services_with_radio(FakeRadio::default(), true);
    let db = services.db.clone();
    let app = app_from(services);
    let (_, state) = send(&app, "GET", "/api/state", None).await;
    assert_eq!(state["autoplay"]["enabled"], true);
    let (status, _) = send(
        &app,
        "PUT",
        "/api/autoplay",
        Some(json!({"enabled": false})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, state) = send(&app, "GET", "/api/state", None).await;
    assert_eq!(state["autoplay"]["enabled"], false);
    assert!(!autoplay_enabled(&db).unwrap());
    let (status, _) = send(&app, "PUT", "/api/autoplay", Some(json!({"enabled": true}))).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(autoplay_enabled(&db).unwrap());
}

#[tokio::test]
async fn autoplay_put_rejects_a_bad_body() {
    let (status, res) = send(&app(), "PUT", "/api/autoplay", Some(json!({"on": 1}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(res["error"].is_string());
}

#[tokio::test]
async fn play_suggestion_plays_it() {
    let radio = FakeRadio::default();
    radio.set("a", ["a", "x"].map(radio_song).into());
    let app = app_from(services_with_radio(radio, true));
    queue_next(&app, "a").await;
    wait_for_suggestions(&app).await;
    let (status, _) = send(
        &app,
        "POST",
        "/api/autoplay/play",
        Some(json!({"videoId": "x"})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, state) = send(&app, "GET", "/api/state", None).await;
    assert_eq!(state["current"]["song"]["videoId"], "x");
}

#[tokio::test]
async fn play_suggestion_that_is_gone_is_409() {
    let radio = FakeRadio::default();
    radio.set("a", ["a", "x"].map(radio_song).into());
    let app = app_from(services_with_radio(radio, true));
    queue_next(&app, "a").await;
    wait_for_suggestions(&app).await;
    let (status, res) = send(
        &app,
        "POST",
        "/api/autoplay/play",
        Some(json!({"videoId": "zzz"})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(res["error"], "that song is no longer suggested");
}

#[tokio::test]
async fn play_suggestion_rejects_blank_id() {
    for body in [json!({"videoId": ""}), json!({"videoId": "  "}), json!({})] {
        let (status, _) = send(&app(), "POST", "/api/autoplay/play", Some(body)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
}

#[tokio::test]
async fn state_carries_autoplay() {
    let radio = FakeRadio::default();
    radio.set("a", ["a", "x", "y"].map(radio_song).into());
    let app = app_from(services_with_radio(radio, true));
    let (_, state) = send(&app, "GET", "/api/state", None).await;
    assert_eq!(
        state["autoplay"],
        json!({"enabled": true, "basedOn": null, "songs": []})
    );
    queue_next(&app, "a").await;
    let state = wait_for_suggestions(&app).await;
    assert_eq!(state["autoplay"]["enabled"], true);
    assert_eq!(state["autoplay"]["basedOn"]["videoId"], "a");
    let ids: Vec<_> = state["autoplay"]["songs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["videoId"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["x", "y"]);
}

#[tokio::test]
async fn ws_sends_autoplay_failed() {
    let radio = FakeRadio::default();
    radio.fail("a");
    let app = app_from(services_with_radio(radio, true));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(axum::serve(listener, app.clone()).into_future());
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/api/ws"))
        .await
        .unwrap();
    for _ in 0..3 {
        next_json(&mut ws).await;
    }
    queue_next(&app, "a").await;
    for _ in 0..10 {
        if next_json(&mut ws).await["type"] == "autoplayFailed" {
            return;
        }
    }
    panic!("no autoplayFailed message");
}
