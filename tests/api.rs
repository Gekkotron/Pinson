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
use pinson::cover_color::CoverColors;
use pinson::db::Db;
use pinson::engine::fake::{FakeEngine, FakeSource};
use pinson::favorites::Favorites;
use pinson::history::History;
use pinson::lyrics::{LrcLibApi, LrcRecord, LyricsQuery, LyricsService};
use pinson::mqtt::commands::{Outcome, dispatch};
use pinson::mqtt::protocol::Command;
use pinson::output::browser::DeviceHub;
use pinson::output::fake::FakeOutputs;
use pinson::output::{OutputChoice, OutputKind, Outputs};
use pinson::player::{Autoplay, Status, spawn_player};
use pinson::playlists::Playlists;
use pinson::radio::Radio;
use pinson::radio::fake::FakeRadio;
use pinson::search::{Searcher, Suggestion, SuggestionRun};
use pinson::song::Song;
use pinson::source::{PcmReader, PcmSource};
use pinson::speakers::NidooSpeakers;

#[derive(Default)]
struct FakeSearcher {
    fail: bool,
    /// What `search` answers; one song when unset.
    results: Option<Vec<Song>>,
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
        if let Some(results) = &self.results {
            return Ok(results.clone());
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
            CollectionKind::Mix => anyhow::bail!("mixes are not searchable"),
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
    let (tx, rx) = mpsc::unbounded_channel();
    let engine = FakeEngine::new(Duration::ZERO);
    let db = Db::in_memory();
    let history = History::new(db.clone());
    let radio: Arc<dyn Radio> = Arc::new(radio);
    let player = spawn_player(
        Some(Box::new(engine)),
        rx,
        Arc::new(FakeSource::default()),
        Autoplay {
            radio: radio.clone(),
            history: history.clone(),
            enabled,
        },
    );
    let artwork = Arc::new(ArtworkProxy::youtube());
    // With nothing configured or saved, starting never waits, so it is ready at once.
    let outputs = futures_util::FutureExt::now_or_never(Outputs::start(
        Arc::new(FakeOutputs::default()),
        tx,
        db.clone(),
        player.clone(),
        None,
    ))
    .expect("starting with no output does not wait")
    .unwrap();
    Services {
        devices: DeviceHub::new(player.clone(), Duration::from_secs(10)),
        player,
        db,
        searcher: searcher.clone(),
        catalog: searcher.clone(),
        favorites,
        playlists: Playlists::in_memory(),
        colors: Arc::new(CoverColors::new(artwork.clone())),
        artwork,
        lyrics: Arc::new(LyricsService::new(Arc::new(FakeLrcLib), searcher)),
        outputs,
        radio,
        history,
    }
}

/// Autoplay off, so the player never calls the radio.
fn services_with_mixes(radio: FakeRadio) -> Services {
    services_with_radio(radio, false)
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

/// An app whose artwork proxy may fetch from `127.0.0.1`, for the colour tests.
fn app_for_local_artwork() -> Router {
    let artwork = Arc::new(ArtworkProxy::new(vec!["127.0.0.1".into()]));
    let mut s = services();
    s.colors = Arc::new(CoverColors::new(artwork.clone()));
    s.artwork = artwork;
    router(s)
}

fn solid_png(rgb: [u8; 3]) -> Vec<u8> {
    let img = image::RgbImage::from_pixel(8, 8, image::Rgb(rgb));
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png).unwrap();
    out.into_inner()
}

/// Serves `/ok.png`, `/text` and `/fail` (500), counting every request.
async fn serve_covers() -> (String, Arc<std::sync::atomic::AtomicUsize>) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let hits = Arc::new(AtomicUsize::new(0));
    let count = |hits: &Arc<AtomicUsize>| {
        let hits = hits.clone();
        move || hits.fetch_add(1, Ordering::SeqCst)
    };
    let (h1, h2, h3) = (count(&hits), count(&hits), count(&hits));
    let png = solid_png([40, 90, 200]);
    let app = Router::new()
        .route(
            "/ok.png",
            axum::routing::get(move || {
                h1();
                let png = png.clone();
                async move { ([(header::CONTENT_TYPE, "image/png")], png) }
            }),
        )
        .route(
            "/text",
            axum::routing::get(move || {
                h2();
                async { ([(header::CONTENT_TYPE, "text/plain")], "hi") }
            }),
        )
        .route(
            "/fail",
            axum::routing::get(move || {
                h3();
                async { StatusCode::INTERNAL_SERVER_ERROR }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), hits)
}

fn color_uri(url: &str) -> String {
    format!("/api/artwork/color?url={url}")
}

#[tokio::test]
async fn artwork_color_of_a_png() {
    let (base, _) = serve_covers().await;
    let (status, body) = send(
        &app_for_local_artwork(),
        "GET",
        &color_uri(&format!("{base}/ok.png")),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["glow"], "#285ac8");
    assert!(body["color"].as_str().unwrap().starts_with('#'));
}

#[tokio::test]
async fn artwork_color_rejects_other_hosts() {
    let (status, body) = send(
        &app(),
        "GET",
        "/api/artwork/color?url=https://evil.example/x.jpg",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].is_string());
    let (status, _) = send(&app(), "GET", "/api/artwork/color", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn artwork_color_null_when_not_an_image() {
    let (base, _) = serve_covers().await;
    let (status, body) = send(
        &app_for_local_artwork(),
        "GET",
        &color_uri(&format!("{base}/text")),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["color"].is_null());
    assert!(body["glow"].is_null());
}

#[tokio::test]
async fn artwork_color_is_cached() {
    use std::sync::atomic::Ordering;
    let (base, hits) = serve_covers().await;
    let app = app_for_local_artwork();
    let ok = color_uri(&format!("{base}/ok.png"));
    for _ in 0..2 {
        let (status, _) = send(&app, "GET", &ok, None).await;
        assert_eq!(status, StatusCode::OK);
    }
    assert_eq!(hits.load(Ordering::SeqCst), 1);

    // A non-image is cached as "no colour" too.
    let text = color_uri(&format!("{base}/text"));
    for _ in 0..2 {
        let (status, body) = send(&app, "GET", &text, None).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body["color"].is_null());
    }
    assert_eq!(hits.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn artwork_color_does_not_cache_upstream_errors() {
    use std::sync::atomic::Ordering;
    let (base, hits) = serve_covers().await;
    let app = app_for_local_artwork();
    let fail = color_uri(&format!("{base}/fail"));
    for _ in 0..2 {
        let (status, body) = send(&app, "GET", &fail, None).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body["color"].is_null());
        assert!(body["glow"].is_null());
    }
    assert_eq!(hits.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn artwork_color_cache_control_follows_certainty() {
    let (base, _) = serve_covers().await;
    let app = app_for_local_artwork();
    let cache_control = |path: &'static str| {
        let app = app.clone();
        let uri = color_uri(&format!("{base}/{path}"));
        async move {
            let req = Request::builder().uri(uri).body(Body::empty()).unwrap();
            let res = app.oneshot(req).await.unwrap();
            res.headers()[header::CACHE_CONTROL]
                .to_str()
                .unwrap()
                .to_owned()
        }
    };
    assert_eq!(cache_control("fail").await, "no-store");
    assert_eq!(cache_control("text").await, "public, max-age=86400");
    assert_eq!(cache_control("ok.png").await, "public, max-age=86400");
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
async fn suggestions_return_runs() {
    let searcher = Arc::new(FakeSearcher::default());
    let app = app_with(searcher.clone());
    let (status, body) = send(&app, "GET", "/api/search/suggestions?q=daft%20pu", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body[0]["text"], "daft punk");
    assert_eq!(body[0]["runs"][0], json!({"text": "daft pu", "bold": true}));
    assert_eq!(
        searcher.last_query.lock().unwrap().as_deref(),
        Some("daft pu")
    );
}

#[tokio::test]
async fn suggestions_reject_blank_query() {
    for uri in [
        "/api/search/suggestions?q=",
        "/api/search/suggestions?q=%20",
        "/api/search/suggestions",
    ] {
        let (status, body) = send(&app(), "GET", uri, None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
        assert_eq!(body, json!({"error": "q must not be blank"}), "{uri}");
    }
}

#[tokio::test]
async fn suggestions_upstream_error_is_502() {
    let searcher = Arc::new(FakeSearcher {
        fail: true,
        ..FakeSearcher::default()
    });
    let (status, body) = send(
        &app_with(searcher),
        "GET",
        "/api/search/suggestions?q=daft",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .starts_with("suggestions failed:")
    );
}

#[tokio::test(start_paused = true)]
async fn suggestions_time_out_is_504() {
    let searcher = Arc::new(FakeSearcher {
        suggest_delay: Some(Duration::from_secs(10)),
        ..FakeSearcher::default()
    });
    let (status, body) = send(
        &app_with(searcher),
        "GET",
        "/api/search/suggestions?q=daft",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(body, json!({"error": "suggestions timed out"}));
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
    assert_eq!(next_json(&mut ws).await["type"], "output");

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
async fn lyrics_shortcut_redirects_to_tv_lyrics() {
    let app = app().merge(pinson::web::web_routes());
    for path in ["/lyrics", "/lyrics/"] {
        let req = Request::builder().uri(path).body(Body::empty()).unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert!(res.status().is_redirection(), "{path}: {}", res.status());
        assert_eq!(
            res.headers().get(header::LOCATION).unwrap(),
            "/#/now?tab=lyrics&tv=1",
            "{path}"
        );
    }
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
    assert_eq!(next_json(&mut ws).await["type"], "output");

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
    assert_eq!(next_json(&mut ws).await["type"], "output");

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
    for expected in ["state", "favorites", "playlists", "output"] {
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

/// Replaces the services' outputs with ones built by `fake`.
async fn restart_outputs(
    services: &mut Services,
    fake: FakeOutputs,
    configured: Option<OutputChoice>,
) {
    let (tx, _rx) = mpsc::unbounded_channel();
    services.outputs = Outputs::start(
        Arc::new(fake),
        tx,
        services.db.clone(),
        services.player.clone(),
        configured,
    )
    .await
    .unwrap();
}

fn nidoo_choice() -> OutputChoice {
    OutputChoice {
        kind: OutputKind::Nidoo,
        address: Some("127.0.0.1:4953".into()),
        name: None,
    }
}

async fn app_with_speakers(base: String, timeout: Duration) -> Router {
    let mut services = services();
    let fake = FakeOutputs {
        speakers: Some(Arc::new(NidooSpeakers::new(base, timeout))),
        ..Default::default()
    };
    restart_outputs(&mut services, fake, Some(nidoo_choice())).await;
    app_from(services)
}

#[tokio::test]
async fn browser_player_is_404_without_nidoo() {
    let app = app_from(services());
    let (status, body) = send(&app, "GET", "/api/browser-player", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "no browser player for this output");
}

#[tokio::test]
async fn browser_player_returns_the_url() {
    let mut services = services();
    let fake = FakeOutputs {
        browser_player: Some("ws://nidoo.local:8927/sendspin".into()),
        ..Default::default()
    };
    restart_outputs(&mut services, fake, Some(nidoo_choice())).await;
    let app = app_from(services);
    let (status, body) = send(&app, "GET", "/api/browser-player", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"url": "ws://nidoo.local:8927/sendspin"}));
}

#[tokio::test]
async fn speakers_list_is_relayed() {
    let (base, _) = fake_nidoo(Duration::ZERO).await;
    let app = app_with_speakers(base, Duration::from_secs(2)).await;
    let (status, body) = send(&app, "GET", "/api/speakers", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body[0]["name"], "Kitchen");
    assert_eq!(body[0]["volume"], 45);
}

#[tokio::test]
async fn speaker_change_is_relayed() {
    let (base, recorded) = fake_nidoo(Duration::ZERO).await;
    let app = app_with_speakers(base, Duration::from_secs(2)).await;
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
    let app = app_with_speakers(base, Duration::from_secs(2)).await;
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
    let app = app_with_speakers(base, Duration::from_secs(2)).await;
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
    let app = app_with_speakers("http://127.0.0.1:1".into(), Duration::from_secs(2)).await;
    let (status, body) = send(&app, "GET", "/api/speakers", None).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(body["error"], "Can't reach nidoo-sound");
}

#[tokio::test]
async fn speakers_timeout_is_502() {
    let (base, _) = fake_nidoo(Duration::from_millis(500)).await;
    let app = app_with_speakers(base, Duration::from_millis(100)).await;
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

#[tokio::test]
async fn speaker_404_without_control_names_no_engine() {
    let app = app();
    let (status, body) = send(&app, "GET", "/api/speakers", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "no speaker control for this output");
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

async fn get_json(app: &Router, uri: &str) -> (StatusCode, Value) {
    send(app, "GET", uri, None).await
}

#[tokio::test]
async fn mixes_empty_without_history() {
    let app = app_from(services_with_mixes(FakeRadio::default()));
    let (status, body) = get_json(&app, "/api/mixes").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!([]));
}

#[tokio::test]
async fn mixes_from_history_and_favorites() {
    let radio = FakeRadio::default();
    radio.set("A", ["A", "x"].map(radio_song).into());
    radio.set("F", ["F", "y"].map(radio_song).into());
    let services = services_with_mixes(radio);
    services.history.record(&song("A", "Alpha")).unwrap();
    services.favorites.add(song("F", "Fox")).unwrap();
    let app = app_from(services);
    let (status, body) = get_json(&app, "/api/mixes").await;
    assert_eq!(status, StatusCode::OK);
    let mixes = body.as_array().unwrap();
    assert_eq!(mixes.len(), 2);
    assert_eq!(mixes[0]["kind"], "mix");
    assert_eq!(mixes[0]["id"], "A");
    assert_eq!(mixes[0]["title"], "Alpha radio");
    assert!(!mixes[0]["songs"].as_array().unwrap().is_empty());
    assert_eq!(mixes[1]["id"], "F");
}

#[tokio::test]
async fn mixes_skip_failed_radios() {
    let radio = FakeRadio::default();
    radio.set("A", ["A", "x"].map(radio_song).into());
    radio.fail("B");
    let services = services_with_mixes(radio);
    services.history.record(&song("B", "Bravo")).unwrap();
    services.history.record(&song("A", "Alpha")).unwrap();
    let app = app_from(services);
    let (status, body) = get_json(&app, "/api/mixes").await;
    assert_eq!(status, StatusCode::OK);
    let ids: Vec<&str> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["A"]);
}

#[tokio::test]
async fn mix_route_200() {
    let radio = FakeRadio::default();
    radio.set("A", ["A", "x"].map(radio_song).into());
    let services = services_with_mixes(radio);
    services.history.record(&song("A", "Alpha")).unwrap();
    let app = app_from(services);
    let (status, body) = get_json(&app, "/api/mixes/A").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["kind"], "mix");
    assert_eq!(body["title"], "Alpha radio");
}

#[tokio::test]
async fn mix_route_unknown_seed_200() {
    let radio = FakeRadio::default();
    radio.set("Z", ["Z", "y"].map(radio_song).into());
    let app = app_from(services_with_mixes(radio));
    let (status, body) = get_json(&app, "/api/mixes/Z").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["title"], "Z radio");
}

#[tokio::test]
async fn mix_route_empty_radio_404() {
    let radio = FakeRadio::default();
    radio.set("Q", vec![]);
    let app = app_from(services_with_mixes(radio));
    let (status, _) = get_json(&app, "/api/mixes/Q").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn mix_route_upstream_error_502() {
    let radio = FakeRadio::default();
    radio.fail("Q");
    let app = app_from(services_with_mixes(radio));
    let (status, _) = get_json(&app, "/api/mixes/Q").await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
}

fn dp_songs() -> Vec<Song> {
    vec![song("d1", "One"), song("d2", "Two"), song("d3", "Three")]
}

fn searcher_with(results: Vec<Song>) -> Arc<FakeSearcher> {
    Arc::new(FakeSearcher {
        results: Some(results),
        ..FakeSearcher::default()
    })
}

fn current_id(app: &Services) -> Option<String> {
    app.player.state().current.map(|e| e.song.video_id)
}

#[tokio::test]
async fn mqtt_play_media_plays_only_top_song() {
    let app = services_with(searcher_with(dp_songs()), Favorites::in_memory());
    let out = dispatch(&app, Command::PlayMedia("daft punk".into())).await;
    assert_eq!(out, Outcome::Echo);
    let state = app.player.state();
    assert_eq!(current_id(&app).as_deref(), Some("d1"));
    assert!(state.upcoming.is_empty());
    assert!(!state.autoplay.enabled);
}

#[tokio::test]
async fn mqtt_play_media_keeps_autoplay_on() {
    let app = services_built(
        searcher_with(dp_songs()),
        Favorites::in_memory(),
        FakeRadio::default(),
        true,
    );
    let out = dispatch(&app, Command::PlayMedia("daft punk".into())).await;
    assert_eq!(out, Outcome::Echo);
    assert!(app.player.state().autoplay.enabled);
}

#[tokio::test]
async fn mqtt_play_media_no_results() {
    let app = services_with(searcher_with(vec![]), Favorites::in_memory());
    app.player
        .play_all(vec![song("q1", "Queued")])
        .await
        .unwrap();
    let out = dispatch(&app, Command::PlayMedia("nothing".into())).await;
    assert_eq!(out, Outcome::Nothing);
    assert_eq!(current_id(&app).as_deref(), Some("q1"));
}

#[tokio::test]
async fn mqtt_play_media_search_error() {
    let searcher = Arc::new(FakeSearcher {
        fail: true,
        ..FakeSearcher::default()
    });
    let app = services_with(searcher, Favorites::in_memory());
    app.player
        .play_all(vec![song("q1", "Queued")])
        .await
        .unwrap();
    let out = dispatch(&app, Command::PlayMedia("x".into())).await;
    assert_eq!(out, Outcome::Nothing);
    assert_eq!(current_id(&app).as_deref(), Some("q1"));
}

#[tokio::test]
async fn mqtt_play_resumes_when_paused() {
    let app = services();
    app.player
        .play_all(vec![song("q1", "Queued"), song("q2", "Next")])
        .await
        .unwrap();
    app.player.pause().await.unwrap();
    assert_eq!(app.player.state().status, Status::Paused);
    let out = dispatch(&app, Command::Play).await;
    assert_eq!(out, Outcome::Done);
    assert_eq!(app.player.state().status, Status::Playing);
    assert_eq!(current_id(&app).as_deref(), Some("q1"));
}

#[tokio::test]
async fn mqtt_pause_pauses() {
    let app = services();
    app.player
        .play_all(vec![song("q1", "Queued")])
        .await
        .unwrap();
    assert_eq!(dispatch(&app, Command::Pause).await, Outcome::Done);
    assert_eq!(app.player.state().status, Status::Paused);
}

#[tokio::test]
async fn mqtt_play_when_idle_shuffles_favorites() {
    let favorites = Favorites::in_memory();
    for (id, t) in [("f1", "A"), ("f2", "B"), ("f3", "C")] {
        favorites.add(song(id, t)).unwrap();
    }
    let app = services_with(Arc::new(FakeSearcher::default()), favorites);
    let out = dispatch(&app, Command::Play).await;
    assert_eq!(out, Outcome::Done);
    let state = app.player.state();
    assert_eq!(state.status, Status::Playing);
    assert_eq!(1 + state.upcoming.len(), 3);
}

#[tokio::test]
async fn mqtt_play_when_idle_without_favorites_plays_first_mix() {
    let radio = FakeRadio::default();
    radio.set("A", ["A", "x"].map(radio_song).into());
    let app = services_with_mixes(radio);
    app.history.record(&song("A", "Alpha")).unwrap();
    let out = dispatch(&app, Command::Play).await;
    assert_eq!(out, Outcome::Done);
    assert_eq!(app.player.state().status, Status::Playing);
    assert_eq!(current_id(&app).as_deref(), Some("A"));
}

#[tokio::test]
async fn mqtt_play_when_nothing_to_play() {
    let app = services();
    let out = dispatch(&app, Command::Play).await;
    assert_eq!(out, Outcome::Nothing);
    assert_eq!(app.player.state().status, Status::Stopped);
    assert!(app.player.state().current.is_none());
}

#[tokio::test]
async fn mqtt_next_and_previous_echo() {
    let app = services();
    app.player
        .play_all(vec![song("q1", "One"), song("q2", "Two")])
        .await
        .unwrap();
    assert_eq!(dispatch(&app, Command::Next).await, Outcome::Echo);
    assert_eq!(current_id(&app).as_deref(), Some("q2"));
    // Right after the song started, `previous` steps back to the one before.
    assert_eq!(dispatch(&app, Command::Previous).await, Outcome::Echo);
    assert_eq!(current_id(&app).as_deref(), Some("q1"));
}

#[tokio::test]
async fn mqtt_next_with_nothing_playing_is_nothing() {
    let app = services();
    assert_eq!(dispatch(&app, Command::Next).await, Outcome::Nothing);
    assert_eq!(dispatch(&app, Command::Previous).await, Outcome::Nothing);
}

#[tokio::test]
async fn mqtt_shutdown_is_bounded_when_broker_is_down() {
    // A port nothing listens on: connects are refused and retried.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let cfg = pinson::config::MqttConfig {
        host: "127.0.0.1".into(),
        port,
        user: None,
        password: None,
        discovery_prefix: "homeassistant".into(),
        root: "pinson".into(),
    };
    let app = services();
    let mqtt = pinson::mqtt::spawn(cfg, app.clone());
    // The player keeps working while the broker is unreachable.
    app.player
        .play_all(vec![song("q1", "Queued")])
        .await
        .unwrap();
    assert_eq!(current_id(&app).as_deref(), Some("q1"));
    let started = std::time::Instant::now();
    tokio::time::timeout(Duration::from_secs(3), mqtt.shutdown())
        .await
        .expect("shutdown must not hang without a broker");
    assert!(started.elapsed() < Duration::from_millis(1500));
}

fn history_ids(body: &Value) -> Vec<&str> {
    body.as_array()
        .unwrap()
        .iter()
        .map(|p| p["song"]["videoId"].as_str().unwrap())
        .collect()
}

#[tokio::test]
async fn history_empty() {
    let (status, body) = get_json(&app(), "/api/history").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!([]));
}

#[tokio::test]
async fn history_newest_first_with_paging() {
    let services = services();
    for (id, title) in [("A", "Alpha"), ("B", "Bravo"), ("C", "Charlie")] {
        services.history.record(&song(id, title)).unwrap();
    }
    let app = app_from(services);
    let (status, body) = get_json(&app, "/api/history?limit=2").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(history_ids(&body), ["C", "B"]);
    assert!(body[0]["id"].is_i64());
    assert!(body[0]["playedAt"].is_string());
    assert_eq!(body[0]["song"]["title"], "Charlie");
    let before = body[1]["id"].as_i64().unwrap();
    let (status, body) = get_json(&app, &format!("/api/history?limit=2&before={before}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(history_ids(&body), ["A"]);
}

#[tokio::test]
async fn history_search() {
    let services = services();
    services.history.record(&song("A", "Alpha")).unwrap();
    services.history.record(&song("B", "Bravo")).unwrap();
    let (status, body) = get_json(&app_from(services), "/api/history?q=alp").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(history_ids(&body), ["A"]);
}

#[tokio::test]
async fn history_bad_before_is_400() {
    let (status, body) = get_json(&app(), "/api/history?before=abc").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].is_string());
}

#[tokio::test]
async fn output_get_without_output() {
    let app = app();
    let (status, body) = send(&app, "GET", "/api/output", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["current"].is_null());
    assert!(body["startupId"].is_null());
    assert_eq!(body["available"][0]["kind"], "nidoo");
    assert_eq!(body["available"][0]["needsAddress"], true);
}

#[tokio::test]
async fn output_put_switches_and_is_reported() {
    let app = app();
    let (status, body) = send(
        &app,
        "PUT",
        "/api/output",
        Some(json!({"kind": "nidoo", "address": "127.0.0.1:4953"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["current"]["id"], "nidoo:127.0.0.1:4953");
    assert_eq!(body["current"]["speakers"], false);
    assert_eq!(body["current"]["hint"], "Approve speakers in Nidoo.");
    let (status, got) = send(&app, "GET", "/api/output", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got, body);
}

#[tokio::test]
async fn output_put_bad_address_is_400() {
    let app = app();
    let (status, body) = send(
        &app,
        "PUT",
        "/api/output",
        Some(json!({"kind": "nidoo", "address": "not an address"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap().contains("host:port"));
    let (_, got) = send(&app, "GET", "/api/output", None).await;
    assert!(got["current"].is_null());
}

#[tokio::test]
async fn output_put_unknown_kind_is_400() {
    let app = app();
    let (status, body) = send(&app, "PUT", "/api/output", Some(json!({"kind": "airplay"}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].is_string());
}

#[tokio::test]
async fn ws_sends_output_after_playlists_and_on_change() {
    let app = app();
    let mut ws = connect_ws(&app).await;
    for expected in ["state", "favorites", "playlists"] {
        assert_eq!(next_json(&mut ws).await["type"], expected);
    }
    let first = next_json(&mut ws).await;
    assert_eq!(first["type"], "output");
    assert!(first["current"].is_null());
    assert_eq!(first["available"][0]["kind"], "nidoo");

    send(
        &app,
        "PUT",
        "/api/output",
        Some(json!({"kind": "nidoo", "address": "127.0.0.1:4953"})),
    )
    .await;
    loop {
        let msg = next_json(&mut ws).await;
        if msg["type"] == "output" {
            assert_eq!(msg["current"]["id"], "nidoo:127.0.0.1:4953");
            break;
        }
    }
}

#[tokio::test]
async fn speakers_follow_the_current_output() {
    let (base, _) = fake_nidoo(Duration::ZERO).await;
    let mut services = services();
    let fake = FakeOutputs {
        speakers: Some(Arc::new(NidooSpeakers::new(base, Duration::from_secs(2)))),
        ..Default::default()
    };
    restart_outputs(&mut services, fake, None).await;
    let app = app_from(services);
    let (status, _) = send(&app, "GET", "/api/speakers", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = send(
        &app,
        "PUT",
        "/api/output",
        Some(json!({"kind": "nidoo", "address": "127.0.0.1:4953"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = send(&app, "GET", "/api/speakers", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body[0]["name"], "Kitchen");
}

// ---- "This device": WebSocket messages and stream routes ----

const DEVICE: &str = "testdevice1";
const PATTERN_LEN: usize = 4000;

/// Deterministic PCM for a song: byte `i` is `i` plus the first byte of its id.
fn pattern(video_id: &str) -> Vec<u8> {
    let base = video_id.as_bytes()[0];
    (0..PATTERN_LEN)
        .map(|i| (i as u8).wrapping_add(base))
        .collect()
}

struct PatternReader {
    bytes: Vec<u8>,
    pos: usize,
}

#[async_trait]
impl PcmReader for PatternReader {
    async fn read(&mut self, buf: &mut [u8]) -> anyhow::Result<usize> {
        let n = buf.len().min(self.bytes.len() - self.pos);
        buf[..n].copy_from_slice(&self.bytes[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

struct PatternSource;

#[async_trait]
impl PcmSource for PatternSource {
    async fn open(&self, video_id: &str, _start: Duration) -> anyhow::Result<Box<dyn PcmReader>> {
        Ok(Box::new(PatternReader {
            bytes: pattern(video_id),
            pos: 0,
        }))
    }
}

/// An app whose "This device" output is real (with a passthrough encoder).
fn services_with_device() -> (Services, DeviceHub) {
    let (tx, rx) = mpsc::unbounded_channel();
    let db = Db::in_memory();
    let history = History::new(db.clone());
    let radio: Arc<dyn Radio> = Arc::new(FakeRadio::default());
    let player = spawn_player(
        Some(Box::new(FakeEngine::new(Duration::ZERO))),
        rx,
        Arc::new(PatternSource),
        Autoplay {
            radio: radio.clone(),
            history: history.clone(),
            enabled: false,
        },
    );
    let hub = DeviceHub::new(player.clone(), Duration::from_secs(10));
    let outputs = futures_util::FutureExt::now_or_never(Outputs::start(
        Arc::new(FakeOutputs {
            devices: Some(hub.clone()),
            ..Default::default()
        }),
        tx,
        db.clone(),
        player.clone(),
        None,
    ))
    .expect("starting with no output does not wait")
    .unwrap();
    let searcher = Arc::new(FakeSearcher::default());
    let artwork = Arc::new(ArtworkProxy::youtube());
    let services = Services {
        devices: hub.clone(),
        player,
        db,
        searcher: searcher.clone(),
        catalog: searcher.clone(),
        favorites: Favorites::in_memory(),
        playlists: Playlists::in_memory(),
        colors: Arc::new(CoverColors::new(artwork.clone())),
        artwork,
        lyrics: Arc::new(LyricsService::new(Arc::new(FakeLrcLib), searcher)),
        outputs,
        radio,
        history,
    };
    (services, hub)
}

type TestWs =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Opens `/api/ws` and reads the four greeting messages.
async fn ws_greeted(app: &Router) -> TestWs {
    let mut ws = connect_ws(app).await;
    for expected in ["state", "favorites", "playlists", "output"] {
        assert_eq!(next_json(&mut ws).await["type"], expected);
    }
    ws
}

async fn ws_send(ws: &mut TestWs, value: Value) {
    use futures_util::SinkExt;
    ws.send(tokio_tungstenite::tungstenite::Message::Text(
        value.to_string().into(),
    ))
    .await
    .unwrap();
}

/// Reads messages until one has `type` equal to `kind`.
async fn next_of_type(ws: &mut TestWs, kind: &str) -> Value {
    for _ in 0..20 {
        let msg = next_json(ws).await;
        if msg["type"] == kind {
            return msg;
        }
    }
    panic!("no {kind} message");
}

fn hello() -> Value {
    json!({"type": "hello", "deviceId": DEVICE, "name": "T", "loaded": null})
}

/// Says hello, waits until the hub sees the device, and switches the output to it.
async fn device_ready(app: &Router, hub: &DeviceHub, ws: &mut TestWs) {
    ws_send(ws, hello()).await;
    let mut link = hub.link(DEVICE);
    tokio::time::timeout(Duration::from_secs(5), link.wait_for(|v| *v == Some(true)))
        .await
        .expect("device connects")
        .unwrap();
    let (status, _) = send(
        app,
        "PUT",
        "/api/output",
        Some(json!({"kind": "browser", "address": DEVICE, "name": "T"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

async fn queue_songs(app: &Router, ids: &[&str]) {
    let (status, _) = send(
        app,
        "POST",
        "/api/queue/play",
        Some(json!({ "songs": songs(ids) })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

async fn get_bytes(app: &Router, uri: &str) -> (StatusCode, Vec<u8>) {
    let req = Request::builder().uri(uri).body(Body::empty()).unwrap();
    let res = tokio::time::timeout(Duration::from_secs(5), app.clone().oneshot(req))
        .await
        .expect("response within 5 s")
        .unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, bytes.to_vec())
}

#[tokio::test]
async fn ws_hello_then_load_arrives() {
    let (s, hub) = services_with_device();
    let app = app_from(s);
    let mut ws = ws_greeted(&app).await;
    device_ready(&app, &hub, &mut ws).await;
    queue_songs(&app, &["a"]).await;
    let load = next_of_type(&mut ws, "device.load").await;
    assert!(load["generation"].is_u64());
    assert!(
        load["url"]
            .as_str()
            .unwrap()
            .starts_with("/api/device/stream/")
    );
}

#[tokio::test]
async fn ws_position_report_moves_state() {
    let (s, hub) = services_with_device();
    let app = app_from(s);
    let mut ws = ws_greeted(&app).await;
    device_ready(&app, &hub, &mut ws).await;
    queue_songs(&app, &["a"]).await;
    let load = next_of_type(&mut ws, "device.load").await;
    ws_send(
        &mut ws,
        json!({"type": "device.position", "generation": load["generation"], "positionMs": 30000}),
    )
    .await;
    for _ in 0..20 {
        let state = next_of_type(&mut ws, "state").await;
        if state["positionMs"].as_u64().unwrap() >= 30000 {
            assert_eq!(state["positionRunning"], true);
            return;
        }
    }
    panic!("position never moved");
}

#[tokio::test]
async fn stream_route_serves_bytes_from_offset() {
    let (s, hub) = services_with_device();
    let app = app_from(s);
    let mut ws = ws_greeted(&app).await;
    device_ready(&app, &hub, &mut ws).await;
    queue_songs(&app, &["a"]).await;
    let load = next_of_type(&mut ws, "device.load").await;
    let generation = load["generation"].as_u64().unwrap();

    let req = Request::builder()
        .uri(format!(
            "/api/device/stream/{generation}?device={DEVICE}&fromMs=24"
        ))
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers()[header::CONTENT_TYPE], "audio/mpeg");
    assert_eq!(res.headers()[header::CACHE_CONTROL], "no-store");
    assert!(res.headers().get(header::CONTENT_LENGTH).is_none());
    let body = res.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(body.to_vec(), pattern("a")[576..].to_vec());
}

#[tokio::test]
async fn stream_route_404_for_old_generation() {
    let (s, hub) = services_with_device();
    let app = app_from(s);
    let mut ws = ws_greeted(&app).await;
    device_ready(&app, &hub, &mut ws).await;
    queue_songs(&app, &["a"]).await;
    let load = next_of_type(&mut ws, "device.load").await;
    let old = load["generation"].as_u64().unwrap() + 100;
    let (status, body) = send(
        &app,
        "GET",
        &format!("/api/device/stream/{old}?device={DEVICE}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body, json!({"error": "no such stream"}));
    let (status, _) = send(&app, "GET", "/api/device/stream/1", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn next_route_advances_queue() {
    let (s, hub) = services_with_device();
    let app = app_from(s);
    let mut ws = ws_greeted(&app).await;
    device_ready(&app, &hub, &mut ws).await;
    queue_songs(&app, &["a", "b"]).await;
    let load = next_of_type(&mut ws, "device.load").await;
    let generation = load["generation"].as_u64().unwrap();
    let (status, body) = get_bytes(
        &app,
        &format!("/api/device/next?device={DEVICE}&after={generation}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, pattern("b"));
    let (_, state) = send(&app, "GET", "/api/state", None).await;
    assert_eq!(state["current"]["song"]["videoId"], "b");
}

#[tokio::test]
async fn next_route_204_when_queue_ends() {
    let (s, hub) = services_with_device();
    let app = app_from(s);
    let mut ws = ws_greeted(&app).await;
    device_ready(&app, &hub, &mut ws).await;
    queue_songs(&app, &["a"]).await;
    let load = next_of_type(&mut ws, "device.load").await;
    let generation = load["generation"].as_u64().unwrap();
    let (status, body) = get_bytes(
        &app,
        &format!("/api/device/next?device={DEVICE}&after={generation}"),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(body.is_empty());
}

#[tokio::test]
async fn ws_ignores_device_messages_before_hello() {
    let (s, hub) = services_with_device();
    let app = app_from(s);
    let mut ws = ws_greeted(&app).await;
    ws_send(
        &mut ws,
        json!({"type": "device.position", "generation": 1, "positionMs": 5}),
    )
    .await;
    ws_send(
        &mut ws,
        json!({"type": "device.failed", "generation": 1, "reason": "x"}),
    )
    .await;
    // The session is still alive: a hello afterwards works.
    device_ready(&app, &hub, &mut ws).await;
    queue_songs(&app, &["a"]).await;
    next_of_type(&mut ws, "device.load").await;
}
