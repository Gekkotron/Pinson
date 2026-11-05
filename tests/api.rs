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

use music::api::router;
use music::engine::fake::{FakeEngine, FakeSource};
use music::favorites::Favorites;
use music::player::spawn_player;
use music::search::Searcher;
use music::song::Song;

#[derive(Default)]
struct FakeSearcher {
    fail: bool,
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

fn app_with(searcher: Arc<FakeSearcher>) -> Router {
    app_with_favorites(searcher, Favorites::in_memory())
}

fn app_with_favorites(searcher: Arc<FakeSearcher>, favorites: Favorites) -> Router {
    let (_tx, rx) = mpsc::unbounded_channel();
    let engine = FakeEngine::new(Duration::ZERO);
    let player = spawn_player(Box::new(engine), rx, Arc::new(FakeSource::default()));
    router(player, searcher, favorites)
}

fn app() -> Router {
    app_with(Arc::new(FakeSearcher::default()))
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
    let app = app().merge(music::web::web_routes());
    let (status, ctype, body) = get_raw(app.clone(), "/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(ctype.starts_with("text/html"), "{ctype}");
    assert!(body.contains(r#"id="search""#));

    let (status, ctype, _) = get_raw(app.clone(), "/app.js").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        ctype.starts_with("application/javascript") || ctype.starts_with("text/javascript"),
        "{ctype}"
    );

    let (status, ctype, _) = get_raw(app.clone(), "/style.css").await;
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

    send(&app, "PUT", "/api/favorites/b", Some(song_json("b", "B"))).await;
    let favs = next_json(&mut ws).await;
    assert_eq!(favs["type"], "favorites");
    assert_eq!(favs["songs"][0]["videoId"], "b");
    assert_eq!(favs["songs"][1]["videoId"], "a");

    send(&app, "DELETE", "/api/favorites/a", None).await;
    let favs = next_json(&mut ws).await;
    assert_eq!(favs["songs"].as_array().unwrap().len(), 1);
}
