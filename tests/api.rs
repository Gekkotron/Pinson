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
use pinson::catalog::{
    Artist, ArtistSummary, Catalog, Collection, CollectionKind, CollectionSummary,
};
use pinson::cover_color::CoverColors;
use pinson::db::Db;
use pinson::engine::fake::{FakeEngine, FakeSource};
use pinson::favorites::Favorites;
use pinson::follows::Follows;
use pinson::hidden::Hidden;
use pinson::history::History;
use pinson::lyrics::{LrcLibApi, LrcRecord, LyricsQuery, LyricsService};
use pinson::moods::MoodCategory;
use pinson::moods::fake::FakeMoods;
use pinson::mqtt::commands::{Outcome, dispatch};
use pinson::mqtt::protocol::{Command, PlayPlaylist};
use pinson::offline::Offline;
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
use pinson::speakers::{NidooSpeakers, Speaker, SpeakerChange, SpeakerControl, SpeakerError};

#[derive(Default)]
struct FakeSearcher {
    fail: bool,
    /// What `search` answers; one song when unset.
    results: Option<Vec<Song>>,
    suggest_delay: Option<Duration>,
    last_query: Mutex<Option<String>>,
    /// Artist pages served instead of the default "UC1", by id; tests change them
    /// between refreshes.
    artists: Mutex<std::collections::HashMap<String, Artist>>,
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
            CollectionKind::Mix | CollectionKind::Smart => {
                anyhow::bail!("mixes and smart lists are not searchable")
            }
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

    async fn artist(&self, id: &str) -> anyhow::Result<Option<Artist>> {
        if self.fail {
            anyhow::bail!("YouTube Music is down");
        }
        if let Some(artist) = self.artists.lock().unwrap().get(id) {
            return Ok(Some(artist.clone()));
        }
        Ok((id == "UC1").then(|| Artist {
            id: "UC1".into(),
            name: "Daft Punk".into(),
            artwork_url: Some("https://example.com/dp.jpg".into()),
            description: None,
            subscribers: Some("7.22M".into()),
            songs: vec![song("a", "A")],
            albums: vec![summary(CollectionKind::Album, "MPREb_1", "Discovery")],
            singles: vec![],
            related: vec![artist_summary("UC2", "Justice")],
        }))
    }

    async fn find_artist(&self, name: &str) -> anyhow::Result<Option<String>> {
        if self.fail {
            anyhow::bail!("YouTube Music is down");
        }
        Ok((name == "Daft Punk").then(|| "UC1".to_owned()))
    }

    async fn search_artists(&self, _query: &str) -> anyhow::Result<Vec<ArtistSummary>> {
        if self.fail {
            anyhow::bail!("YouTube Music is down");
        }
        Ok(vec![artist_summary("UC1", "Daft Punk")])
    }

    async fn youtube_lyrics(&self, _video_id: &str) -> anyhow::Result<Option<String>> {
        Ok(None)
    }
}

fn artist_summary(id: &str, name: &str) -> ArtistSummary {
    ArtistSummary {
        id: id.into(),
        name: name.into(),
        subtitle: Some("1M subscribers".into()),
        artwork_url: None,
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

/// An offline service that never downloads, saving into an unused temp folder.
fn idle_offline(db: &Db, favorites: &Favorites) -> Offline {
    use std::sync::atomic::{AtomicU32, Ordering};
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "pinson-api-offline-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    ));
    Offline::idle(dir, db.clone(), favorites.clone())
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
    services_parts(searcher, favorites, radio, enabled, FakeSource::default()).0
}

/// `services_built` with the player's source given, and the engine it plays on.
fn services_parts(
    searcher: Arc<FakeSearcher>,
    favorites: Favorites,
    radio: FakeRadio,
    enabled: bool,
    source: FakeSource,
) -> (Services, FakeEngine) {
    let (tx, rx) = mpsc::unbounded_channel();
    let engine = FakeEngine::new(Duration::ZERO);
    let played_on = engine.clone();
    let db = Db::in_memory();
    let history = History::new(db.clone());
    let hidden = Hidden::in_memory();
    let radio: Arc<dyn Radio> = Arc::new(radio);
    let player = spawn_player(
        Some(Box::new(engine)),
        rx,
        Arc::new(source),
        Autoplay {
            radio: radio.clone(),
            history: history.clone(),
            enabled,
            hidden: hidden.clone(),
        },
        std::time::Duration::ZERO,
        pinson::autoplay::Repeat::Off,
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
    let offline = idle_offline(&db, &favorites);
    let services = Services {
        devices: DeviceHub::new(player.clone(), Duration::from_secs(10)),
        streams: Default::default(),
        player,
        follows: Follows::new(db.clone(), searcher.clone(), Duration::ZERO),
        db,
        searcher: searcher.clone(),
        catalog: searcher.clone(),
        favorites,
        hidden,
        playlists: Playlists::in_memory(),
        colors: Arc::new(CoverColors::new(artwork.clone())),
        artwork,
        picore_api_url: None,
        lyrics: Arc::new(LyricsService::new(Arc::new(FakeLrcLib), searcher)),
        outputs,
        radio,
        moods: Arc::new(FakeMoods::default()),
        history,
        offline,
        health: pinson::health::Health::for_tests(),
        shutdown: tokio::sync::watch::channel(false).0,
    };
    (services, played_on)
}

/// Autoplay off, so the player never calls the radio.
fn services_with_mixes(radio: FakeRadio) -> Services {
    services_with_radio(radio, false)
}

fn services_with_moods(moods: FakeMoods) -> Services {
    let mut services = services();
    services.moods = Arc::new(moods);
    services
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
async fn animated_artwork_uses_configured_picore_or_returns_null() {
    let uri = "/api/artwork/animated?artist=BLACKPINK&album=JUMP";
    let (status, body) = send(&app(), "GET", uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"url": null}));

    let upstream = Router::new().route(
        "/picore/api/v1/artwork/search",
        axum::routing::get(|axum::extract::Query(query): axum::extract::Query<std::collections::HashMap<String, String>>| async move {
            assert_eq!(query.get("artist").map(String::as_str), Some("BLACKPINK"));
            assert_eq!(query.get("album").map(String::as_str), Some("JUMP"));
            axum::Json(json!({"artist": "BLACKPINK", "album": "JUMP - Single", "url": "https://mvod.itunes.apple.com/example.m3u8"}))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
    let mut services = services();
    services.picore_api_url = Some(format!("http://{addr}/picore"));
    let (status, body) = send(&app_from(services), "GET", uri, None).await;
    server.abort();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        json!({"url": "https://mvod.itunes.apple.com/example.m3u8"})
    );
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

#[tokio::test]
async fn artwork_resize_and_conditional_requests() {
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let (base, hits) = serve_covers().await;
    let app = app_for_local_artwork();
    let url = format!("{base}/ok.png");
    let uri = format!("/api/artwork?size=132&url={url}");
    let request = |path: &str| axum::http::Request::builder().uri(path);
    let response = app
        .clone()
        .oneshot(request(&uri).body(axum::body::Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CONTENT_TYPE], "image/jpeg");
    let tag = response.headers()[header::ETAG]
        .to_str()
        .unwrap()
        .to_owned();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let image = image::load_from_memory(&bytes).unwrap();
    assert_eq!((image.width(), image.height()), (132, 132));
    let response = app
        .clone()
        .oneshot(
            request(&uri)
                .header(header::IF_NONE_MATCH, &tag)
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
    assert!(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .is_empty()
    );
    let other = format!("/api/artwork?size=64&url={url}");
    let response = app
        .clone()
        .oneshot(
            request(&other)
                .header(header::IF_NONE_MATCH, &tag)
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_ne!(response.headers()[header::ETAG], tag.as_str());
    let original = format!("/api/artwork?url={url}");
    let response = app
        .clone()
        .oneshot(request(&original).body(axum::body::Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.headers()[header::CONTENT_TYPE], "image/png");
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
    let response = app
        .oneshot(
            request(&format!("/api/artwork?size=513&url={url}"))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
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
    let (status, body) = send(&app, "GET", "/api/search?q=daft&kind=bogus", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap().contains("bogus"));
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
async fn move_upcoming_reorders_the_queue() {
    let app = app();
    let mut ids = Vec::new();
    for (vid, title) in [("a", "A"), ("x", "X"), ("y", "Y"), ("z", "Z")] {
        let (_, res) = send(
            &app,
            "POST",
            "/api/queue/next",
            Some(serde_json::to_value(song(vid, title)).unwrap()),
        )
        .await;
        ids.push(res["id"].clone());
    }
    let upcoming = |state: &Value| -> Vec<String> {
        state["upcoming"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["song"]["videoId"].as_str().unwrap().to_string())
            .collect()
    };
    // Z to the top, then X after Y.
    let uri = format!("/api/queue/{}/move", ids[3]);
    let (status, _) = send(&app, "POST", &uri, Some(json!({"after": null}))).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let uri = format!("/api/queue/{}/move", ids[1]);
    let (status, _) = send(&app, "POST", &uri, Some(json!({"after": ids[2]}))).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, state) = send(&app, "GET", "/api/state", None).await;
    assert_eq!(upcoming(&state), ["z", "y", "x"]);

    // The playing song, an unknown entry or an unknown anchor: 404.
    for (id, after) in [
        (ids[0].clone(), json!(null)),
        (json!(999), json!(null)),
        (ids[1].clone(), json!(999)),
    ] {
        let uri = format!("/api/queue/{id}/move");
        let (status, res) = send(&app, "POST", &uri, Some(json!({"after": after}))).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{id} after {after}");
        assert!(res["error"].is_string());
    }
    let (status, _) = send(
        &app,
        "POST",
        &format!("/api/queue/{}/move", ids[1]),
        Some(json!({"after": "y"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
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

#[tokio::test]
async fn sleep_timer_set_shows_in_state_and_cancels() {
    let app = app();
    let (status, state) = send(&app, "GET", "/api/state", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(state["sleep"], Value::Null);

    let (status, _) = send(&app, "PUT", "/api/sleep", Some(json!({"minutes": 30}))).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, state) = send(&app, "GET", "/api/state", None).await;
    let ms = state["sleep"]["remainingMs"].as_u64().unwrap();
    assert!(ms > 29 * 60 * 1000 && ms <= 30 * 60 * 1000, "{ms}");

    let (status, _) = send(&app, "PUT", "/api/sleep", Some(json!({"endOfSong": true}))).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, state) = send(&app, "GET", "/api/state", None).await;
    assert_eq!(state["sleep"], json!({"endOfSong": true}));

    let (status, _) = send(&app, "DELETE", "/api/sleep", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, state) = send(&app, "GET", "/api/state", None).await;
    assert_eq!(state["sleep"], Value::Null);
}

#[tokio::test]
async fn sleep_timer_rejects_bad_bodies() {
    let app = app();
    for body in [
        json!({"minutes": 0}),
        json!({"minutes": 601}),
        json!({"minutes": -5}),
        json!({"minutes": "soon"}),
        json!({"endOfSong": false}),
        json!({"minutes": 30, "endOfSong": true}),
        json!({}),
    ] {
        let (status, res) = send(&app, "PUT", "/api/sleep", Some(body.clone())).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(res["error"].is_string());
    }
    let (_, state) = send(&app, "GET", "/api/state", None).await;
    assert_eq!(state["sleep"], Value::Null);
    let (status, _) = send(&app, "PUT", "/api/sleep", Some(json!({"minutes": 600}))).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = send(&app, "PUT", "/api/sleep", Some(json!({"minutes": 1}))).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
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
async fn lyrics_shortcut_keeps_the_key_debug_flag() {
    let app = app().merge(pinson::web::web_routes());
    for (path, want) in [
        ("/lyrics?keys=1", "/#/now?tab=lyrics&tv=1&keys=1"),
        ("/lyrics/?keys=1", "/#/now?tab=lyrics&tv=1&keys=1"),
        ("/lyrics?keys=0", "/#/now?tab=lyrics&tv=1"),
        ("/lyrics?other=1", "/#/now?tab=lyrics&tv=1"),
    ] {
        let req = Request::builder().uri(path).body(Body::empty()).unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert!(res.status().is_redirection(), "{path}: {}", res.status());
        assert_eq!(res.headers().get(header::LOCATION).unwrap(), want, "{path}");
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
async fn favorites_reorder() {
    let app = app();
    for (id, title) in [("a", "A"), ("b", "B"), ("c", "C")] {
        send(
            &app,
            "PUT",
            &format!("/api/favorites/{id}"),
            Some(song_json(id, title)),
        )
        .await;
    }
    let order = |ids: Value| json!({ "videoIds": ids });
    let (status, _) = send(
        &app,
        "PUT",
        "/api/favorites/order",
        Some(order(json!(["a", "c", "b"]))),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, list) = send(&app, "GET", "/api/favorites", None).await;
    let ids: Vec<&str> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["videoId"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["a", "c", "b"]);

    for (ids, want) in [
        (json!(["a", "c"]), StatusCode::CONFLICT),
        (json!(["a", "c", "b", "z"]), StatusCode::CONFLICT),
        (json!(["a", "a", "b"]), StatusCode::BAD_REQUEST),
    ] {
        let (status, body) = send(
            &app,
            "PUT",
            "/api/favorites/order",
            Some(order(ids.clone())),
        )
        .await;
        assert_eq!(status, want, "{ids}");
        assert!(body["error"].is_string(), "{ids}");
    }
    let (status, _) = send(
        &app,
        "PUT",
        "/api/favorites/order",
        Some(json!({"nope": 1})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (_, list) = send(&app, "GET", "/api/favorites", None).await;
    assert_eq!(list.as_array().unwrap().len(), 3);
    assert_eq!(list[0]["videoId"], "a");
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
/// records the PUTs (204 for `0123456789ab`, 404 otherwise). One Bedroom
/// speaker (`fedcba987654`) waits for approval: allow and forget are
/// recorded as `{"op": ...}` (204 for it, 404 otherwise).
async fn fake_nidoo(delay: Duration) -> (String, Recorded) {
    use axum::Json;
    use axum::extract::{Path, State};
    use axum::response::IntoResponse;
    use axum::routing::{get, post, put};

    let recorded: Recorded = Arc::default();
    let app =
        Router::new()
            .route(
                "/players/waiting",
                get(|| async {
                    Json(json!([{
                        "id": "fedcba987654",
                        "name": "Bedroom",
                        "connected": true
                    }]))
                }),
            )
            .route(
                "/players/{id}/allow",
                post(
                    |State(rec): State<Recorded>, Path(id): Path<String>| async move {
                        waiting_op(&rec, id, "allow")
                    },
                ),
            )
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
                )
                .delete(
                    |State(rec): State<Recorded>, Path(id): Path<String>| async move {
                        waiting_op(&rec, id, "forget")
                    },
                ),
            )
            .with_state(recorded.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), recorded)
}

/// Records a waiting-speaker `op` and answers like nidoo-sound.
fn waiting_op(rec: &Recorded, id: String, op: &str) -> axum::response::Response {
    use axum::Json;
    use axum::response::IntoResponse;
    if id == "fedcba987654" {
        rec.lock().unwrap().push((id, json!({"op": op})));
        StatusCode::NO_CONTENT.into_response()
    } else {
        let error = format!("no speaker {id}");
        (StatusCode::NOT_FOUND, Json(json!({ "error": error }))).into_response()
    }
}

/// An older nidoo-sound: only the `/players` list, no approval routes.
async fn fake_old_nidoo() -> String {
    use axum::Json;
    use axum::routing::get;

    let app = Router::new().route("/players", get(|| async { Json(json!([])) }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

/// Speaker control that has no approval (like a renderer or Snapcast output).
struct PlainSpeakers;

#[async_trait::async_trait]
impl SpeakerControl for PlainSpeakers {
    async fn list(&self) -> Result<Vec<Speaker>, SpeakerError> {
        Ok(vec![])
    }
    async fn set(&self, _id: &str, _change: &SpeakerChange) -> Result<(), SpeakerError> {
        Ok(())
    }
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
async fn waiting_lists_nidoo_speakers() {
    let (base, _) = fake_nidoo(Duration::ZERO).await;
    let app = app_with_speakers(base, Duration::from_secs(2)).await;
    let (status, body) = send(&app, "GET", "/api/speakers/waiting", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!([{"id": "fedcba987654", "name": "Bedroom"}]));
}

#[tokio::test]
async fn allow_and_forget_reach_nidoo_sound() {
    let (base, recorded) = fake_nidoo(Duration::ZERO).await;
    let app = app_with_speakers(base, Duration::from_secs(2)).await;
    let (status, _) = send(&app, "POST", "/api/speakers/fedcba987654/allow", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = send(&app, "DELETE", "/api/speakers/fedcba987654", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        *recorded.lock().unwrap(),
        vec![
            ("fedcba987654".to_owned(), json!({"op": "allow"})),
            ("fedcba987654".to_owned(), json!({"op": "forget"})),
        ]
    );
}

#[tokio::test]
async fn allow_unknown_passes_404() {
    let (base, _) = fake_nidoo(Duration::ZERO).await;
    let app = app_with_speakers(base, Duration::from_secs(2)).await;
    let (status, body) = send(&app, "POST", "/api/speakers/000000000000/allow", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "no speaker 000000000000");
}

#[tokio::test]
async fn allow_rejects_non_hex_ids() {
    let (base, recorded) = fake_nidoo(Duration::ZERO).await;
    let app = app_with_speakers(base, Duration::from_secs(2)).await;
    for path in ["/api/speakers/..%2Fx/allow", "/api/speakers/zz/allow"] {
        let (status, body) = send(&app, "POST", path, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
        assert_eq!(body["error"], "unknown speaker", "{path}");
    }
    let (status, body) = send(&app, "DELETE", "/api/speakers/zz", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "unknown speaker");
    assert!(recorded.lock().unwrap().is_empty());
}

#[tokio::test]
async fn waiting_is_empty_on_an_older_nidoo_sound() {
    let app = app_with_speakers(fake_old_nidoo().await, Duration::from_secs(2)).await;
    let (status, body) = send(&app, "GET", "/api/speakers/waiting", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!([]));
}

#[tokio::test]
async fn approval_routes_404_without_nidoo() {
    let mut services = services();
    let fake = FakeOutputs {
        speakers: Some(Arc::new(PlainSpeakers)),
        ..Default::default()
    };
    restart_outputs(&mut services, fake, Some(nidoo_choice())).await;
    let app = app_from(services);
    for (method, path) in [
        ("GET", "/api/speakers/waiting"),
        ("POST", "/api/speakers/fedcba987654/allow"),
        ("DELETE", "/api/speakers/fedcba987654"),
    ] {
        let (status, _) = send(&app, method, path, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {path}");
    }
    let (_, body) = send(&app, "GET", "/api/output", None).await;
    assert_eq!(body["current"]["speakers"], true);
    assert_eq!(body["current"]["approval"], false);
}

#[tokio::test]
async fn approval_is_reported_for_nidoo() {
    let (base, _) = fake_nidoo(Duration::ZERO).await;
    let app = app_with_speakers(base, Duration::from_secs(2)).await;
    let (_, body) = send(&app, "GET", "/api/output", None).await;
    assert_eq!(body["current"]["approval"], true);
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
async fn offline_status_is_off_and_empty_by_default() {
    let favorites = Favorites::in_memory();
    favorites.add(song("a", "A")).unwrap();
    favorites.add(song("b", "B")).unwrap();
    let app = app_from(services_with(Arc::new(FakeSearcher::default()), favorites));
    let (status, body) = send(&app, "GET", "/api/offline", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        json!({
            "enabled": false, "saved": 0, "total": 2, "bytes": 0,
            "limitBytes": null, "full": false, "downloading": null, "failed": 0
        })
    );
}

#[tokio::test]
async fn offline_switch_persists_and_returns_the_status() {
    let services = services();
    let db = services.db.clone();
    let app = app_from(services);
    let (status, body) = send(&app, "PUT", "/api/offline", Some(json!({"enabled": true}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["enabled"], true);
    assert!(pinson::offline::offline_enabled(&db).unwrap());
    let (_, body) = send(&app, "GET", "/api/offline", None).await;
    assert_eq!(body["enabled"], true);
    let (status, body) = send(&app, "PUT", "/api/offline", Some(json!({"enabled": false}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["enabled"], false);
    assert!(!pinson::offline::offline_enabled(&db).unwrap());
}

#[tokio::test]
async fn offline_put_rejects_a_bad_body() {
    let app = app_from(services());
    let (status, _) = send(&app, "PUT", "/api/offline", Some(json!({"enabled": "yes"}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
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
async fn crossfade_setting_persists_and_shows_in_state() {
    let services = services_with_radio(FakeRadio::default(), true);
    let db = services.db.clone();
    let app = app_from(services);
    let (_, state) = send(&app, "GET", "/api/state", None).await;
    assert_eq!(state["crossfadeSecs"], 0);
    // The test engine doesn't crossfade (like Chromecast, UPnP and the browser).
    assert_eq!(state["crossfadeAvailable"], false);
    let (status, _) = send(&app, "PUT", "/api/crossfade", Some(json!({"seconds": 12}))).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, state) = send(&app, "GET", "/api/state", None).await;
    assert_eq!(state["crossfadeSecs"], 12);
    assert_eq!(pinson::autoplay::crossfade_secs(&db).unwrap(), 12);
}

#[tokio::test]
async fn crossfade_put_rejects_bad_values() {
    for body in [
        json!({"seconds": 5}),
        json!({"seconds": 99}),
        json!({"s": 3}),
    ] {
        let (status, res) = send(&app(), "PUT", "/api/crossfade", Some(body)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(res["error"].is_string());
    }
}

#[tokio::test]
async fn repeat_setting_persists_and_shows_in_state() {
    let services = services_with_radio(FakeRadio::default(), true);
    let db = services.db.clone();
    let app = app_from(services);
    let (_, state) = send(&app, "GET", "/api/state", None).await;
    assert_eq!(state["repeat"], "off");
    for mode in ["all", "one", "off"] {
        let (status, _) = send(&app, "PUT", "/api/repeat", Some(json!({"mode": mode}))).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (_, state) = send(&app, "GET", "/api/state", None).await;
        assert_eq!(state["repeat"], mode);
        assert_eq!(db.setting("repeat").unwrap().as_deref(), Some(mode));
    }
}

#[tokio::test]
async fn repeat_all_shows_the_loop_in_up_next() {
    let app = app_from(services_with_radio(FakeRadio::default(), false));
    queue_next(&app, "a").await;
    let (status, _) = send(&app, "PUT", "/api/repeat", Some(json!({"mode": "all"}))).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, state) = send(&app, "GET", "/api/state", None).await;
    assert_eq!(state["upcoming"].as_array().unwrap().len(), 1);
    assert_eq!(state["upcoming"][0]["song"]["videoId"], "a");
    send(&app, "PUT", "/api/repeat", Some(json!({"mode": "off"}))).await;
    let (_, state) = send(&app, "GET", "/api/state", None).await;
    assert!(state["upcoming"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn repeat_put_rejects_bad_values() {
    for body in [
        json!({"mode": "loop"}),
        json!({"mode": 1}),
        json!({"m": "all"}),
    ] {
        let (status, res) = send(&app(), "PUT", "/api/repeat", Some(body)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(res["error"].is_string());
    }
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
async fn artist_page_has_every_section_in_camel_case() {
    let app = app_from(services());
    let (status, body) = get_json(&app, "/api/artists/UC1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["id"], "UC1");
    assert_eq!(body["name"], "Daft Punk");
    assert_eq!(body["artworkUrl"], "https://example.com/dp.jpg");
    assert_eq!(body["subscribers"], "7.22M");
    assert_eq!(body["description"], Value::Null);
    assert_eq!(body["songs"][0]["videoId"], "a");
    assert_eq!(body["albums"][0]["id"], "MPREb_1");
    assert_eq!(body["singles"], json!([]));
    assert_eq!(body["related"][0]["name"], "Justice");
    assert_eq!(body["related"][0]["artworkUrl"], Value::Null);
}

fn release(id: &str, year: &str) -> CollectionSummary {
    CollectionSummary {
        kind: CollectionKind::Album,
        id: id.into(),
        title: format!("Title {id}"),
        subtitle: Some(year.into()),
        artwork_url: Some(format!("https://example.com/{id}.jpg")),
    }
}

/// Serves artist "UC9" with the given albums and singles from now on.
fn serve_artist_uc9(
    searcher: &FakeSearcher,
    albums: Vec<CollectionSummary>,
    singles: Vec<CollectionSummary>,
) {
    searcher.artists.lock().unwrap().insert(
        "UC9".into(),
        Artist {
            id: "UC9".into(),
            name: "Air".into(),
            artwork_url: Some("https://example.com/air.jpg".into()),
            description: None,
            subscribers: None,
            songs: vec![],
            albums,
            singles,
            related: vec![],
        },
    );
}

async fn delete_status(app: &Router, uri: &str) -> StatusCode {
    send(app, "DELETE", uri, None).await.0
}

#[tokio::test]
async fn follow_unfollow_and_list() {
    let app = app_from(services());
    let (status, body) = get_json(&app, "/api/follows").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"artists": []}));

    let (status, body) = send(&app, "PUT", "/api/follows/UC1", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["id"], "UC1");
    assert_eq!(body["name"], "Daft Punk");
    assert_eq!(body["artworkUrl"], "https://example.com/dp.jpg");
    assert_eq!(body["newCount"], 0);
    assert!(body["followedAt"].as_i64().unwrap() > 1_700_000_000);

    // Idempotent.
    let (status, again) = send(&app, "PUT", "/api/follows/UC1", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(again, body);
    let (_, list) = get_json(&app, "/api/follows").await;
    assert_eq!(list["artists"], json!([body]));

    assert_eq!(
        delete_status(&app, "/api/follows/UC1").await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        delete_status(&app, "/api/follows/UC1").await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get_json(&app, "/api/follows").await.1,
        json!({"artists": []})
    );
}

#[tokio::test]
async fn following_an_unknown_artist_is_404_and_a_catalog_failure_is_502() {
    let app = app_from(services());
    assert_eq!(
        send(&app, "PUT", "/api/follows/UCnope", None).await.0,
        StatusCode::NOT_FOUND
    );
    let down = app_from(services_with(
        Arc::new(FakeSearcher {
            fail: true,
            ..Default::default()
        }),
        Favorites::in_memory(),
    ));
    assert_eq!(
        send(&down, "PUT", "/api/follows/UC1", None).await.0,
        StatusCode::BAD_GATEWAY
    );
    assert_eq!(
        get_json(&down, "/api/follows").await.1,
        json!({"artists": []})
    );
}

#[tokio::test]
async fn artist_page_says_whether_it_is_followed() {
    let app = app_from(services());
    assert_eq!(
        get_json(&app, "/api/artists/UC1").await.1["following"],
        false
    );
    send(&app, "PUT", "/api/follows/UC1", None).await;
    assert_eq!(
        get_json(&app, "/api/artists/UC1").await.1["following"],
        true
    );
    delete_status(&app, "/api/follows/UC1").await;
    assert_eq!(
        get_json(&app, "/api/artists/UC1").await.1["following"],
        false
    );
}

#[tokio::test]
async fn new_releases_list_what_a_refresh_found_since_following() {
    let searcher = Arc::new(FakeSearcher::default());
    serve_artist_uc9(&searcher, vec![release("old", "2001")], vec![]);
    let services = services_with(searcher.clone(), Favorites::in_memory());
    let follows = services.follows.clone();
    let app = app_from(services);

    let (_, body) = send(&app, "PUT", "/api/follows/UC9", None).await;
    assert_eq!(body["newCount"], 0);
    assert_eq!(
        get_json(&app, "/api/follows/releases").await,
        (StatusCode::OK, json!({"releases": []}))
    );

    serve_artist_uc9(
        &searcher,
        vec![release("old", "2001")],
        vec![release("fresh", "2026")],
    );
    assert_eq!(follows.refresh_all().await.unwrap(), 1);

    let (status, body) = get_json(&app, "/api/follows/releases").await;
    assert_eq!(status, StatusCode::OK);
    let releases = body["releases"].as_array().unwrap();
    assert_eq!(releases.len(), 1);
    let r = &releases[0];
    assert_eq!(r["id"], "fresh");
    assert_eq!(r["kind"], "single");
    assert_eq!(r["title"], "Title fresh");
    assert_eq!(r["subtitle"], "2026");
    assert_eq!(r["artworkUrl"], "https://example.com/fresh.jpg");
    assert_eq!(r["year"], "2026");
    assert_eq!(r["artistId"], "UC9");
    assert_eq!(r["artistName"], "Air");
    assert!(r["firstSeen"].as_i64().unwrap() > 1_700_000_000);

    let (_, list) = get_json(&app, "/api/follows").await;
    assert_eq!(list["artists"][0]["newCount"], 1);

    // Unfollowing drops the artist's releases too.
    delete_status(&app, "/api/follows/UC9").await;
    assert_eq!(
        get_json(&app, "/api/follows/releases").await.1,
        json!({"releases": []})
    );
}

#[tokio::test]
async fn releases_of_the_current_year_are_new_when_following() {
    let searcher = Arc::new(FakeSearcher::default());
    let year = chrono::Utc::now().format("%Y").to_string();
    serve_artist_uc9(
        &searcher,
        vec![release("this-year", &year), release("old", "2001")],
        vec![],
    );
    let app = app_with(searcher);
    let (_, body) = send(&app, "PUT", "/api/follows/UC9", None).await;
    assert_eq!(body["newCount"], 1);
    let (_, body) = get_json(&app, "/api/follows/releases").await;
    assert_eq!(body["releases"][0]["id"], "this-year");
    assert_eq!(body["releases"][0]["kind"], "album");
}

#[tokio::test]
async fn unknown_artist_is_404_and_upstream_failure_is_502() {
    let app = app_from(services());
    assert_eq!(
        get_json(&app, "/api/artists/UCnope").await.0,
        StatusCode::NOT_FOUND
    );
    let down = app_from(services_with(
        Arc::new(FakeSearcher {
            fail: true,
            ..Default::default()
        }),
        Favorites::in_memory(),
    ));
    assert_eq!(
        get_json(&down, "/api/artists/UC1").await.0,
        StatusCode::BAD_GATEWAY
    );
}

#[tokio::test]
async fn artist_lookup_is_not_swallowed_by_the_id_route() {
    let app = app_from(services());
    let (status, body) = get_json(&app, "/api/artists/lookup?name=Daft%20Punk").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"id": "UC1"}));
    assert_eq!(
        get_json(&app, "/api/artists/lookup?name=Nobody").await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get_json(&app, "/api/artists/lookup").await.0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        get_json(&app, "/api/artists/lookup?name=%20%20").await.0,
        StatusCode::BAD_REQUEST
    );
    let long = "a".repeat(301);
    assert_eq!(
        get_json(&app, &format!("/api/artists/lookup?name={long}"))
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    let down = app_from(services_with(
        Arc::new(FakeSearcher {
            fail: true,
            ..Default::default()
        }),
        Favorites::in_memory(),
    ));
    assert_eq!(
        get_json(&down, "/api/artists/lookup?name=Daft%20Punk")
            .await
            .0,
        StatusCode::BAD_GATEWAY
    );
}

#[tokio::test]
async fn search_kind_artists_lists_artist_summaries() {
    let app = app_from(services());
    let (status, body) = get_json(&app, "/api/search?q=daft&kind=artists").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        json!([{"id": "UC1", "name": "Daft Punk", "subtitle": "1M subscribers", "artworkUrl": null}])
    );
    let (status, body) = get_json(&app, "/api/search?q=daft&kind=bogus").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap().contains("artists"));
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

fn playlist_cmd(id: &str) -> PlayPlaylist {
    PlayPlaylist {
        playlist: id.into(),
        shuffle: false,
        source: None,
        fade_in: Duration::ZERO,
    }
}

async fn play_playlist(app: &Services, cmd: PlayPlaylist) -> Outcome {
    dispatch(app, Command::PlayPlaylist(cmd)).await
}

/// Ids of the current song and the upcoming ones, in order.
fn queue_ids(app: &Services) -> Vec<String> {
    let state = app.player.state();
    state
        .current
        .into_iter()
        .chain(state.upcoming)
        .map(|e| e.song.video_id)
        .collect()
}

fn favorites_of(ids: &[&str]) -> Favorites {
    let favorites = Favorites::in_memory();
    for id in ids {
        favorites.add(song(id, id)).unwrap();
    }
    favorites
}

#[tokio::test]
async fn plays_named_playlist_in_order_when_shuffle_off() {
    let app = services_with(Arc::new(FakeSearcher::default()), favorites_of(&["f1"]));
    let list = app
        .playlists
        .create(
            "Morning",
            vec![song("p1", "A"), song("p2", "B"), song("p3", "C")],
        )
        .unwrap();
    let out = play_playlist(&app, playlist_cmd(&list.id)).await;
    assert_eq!(out, Outcome::Echo);
    assert_eq!(queue_ids(&app), ["p1", "p2", "p3"]);
}

#[tokio::test]
async fn shuffle_keeps_every_song() {
    let app = services();
    let list = app
        .playlists
        .create(
            "Morning",
            (1..=8).map(|i| song(&format!("p{i}"), "x")).collect(),
        )
        .unwrap();
    let cmd = PlayPlaylist {
        shuffle: true,
        ..playlist_cmd(&list.id)
    };
    assert_eq!(play_playlist(&app, cmd).await, Outcome::Echo);
    let mut ids = queue_ids(&app);
    ids.sort();
    assert_eq!(ids, (1..=8).map(|i| format!("p{i}")).collect::<Vec<_>>());
}

#[tokio::test]
async fn favorites_id_plays_favorites() {
    let app = services_with(
        Arc::new(FakeSearcher::default()),
        favorites_of(&["f1", "f2"]),
    );
    app.playlists
        .create("Other", vec![song("p1", "A")])
        .unwrap();
    let out = play_playlist(&app, playlist_cmd("favorites")).await;
    assert_eq!(out, Outcome::Echo);
    let mut ids = queue_ids(&app);
    ids.sort();
    assert_eq!(ids, ["f1", "f2"]);
}

#[tokio::test]
async fn unknown_playlist_falls_back_to_favorites() {
    let app = services_with(Arc::new(FakeSearcher::default()), favorites_of(&["f1"]));
    let out = play_playlist(&app, playlist_cmd("deleted-id")).await;
    assert_eq!(out, Outcome::Echo);
    assert_eq!(queue_ids(&app), ["f1"]);
}

#[tokio::test]
async fn empty_everything_uses_play_something() {
    let radio = FakeRadio::default();
    radio.set("A", ["A", "x"].map(radio_song).into());
    let app = services_with_mixes(radio);
    app.history.record(&song("A", "Alpha")).unwrap();
    let out = play_playlist(&app, playlist_cmd("deleted-id")).await;
    assert_eq!(out, Outcome::Echo);
    assert_eq!(current_id(&app).as_deref(), Some("A"));
}

#[tokio::test]
async fn nothing_at_all_is_nothing() {
    let app = services();
    assert_eq!(
        play_playlist(&app, playlist_cmd("favorites")).await,
        Outcome::Nothing
    );
    assert!(app.player.state().current.is_none());
}

fn current_output_id(app: &Services) -> Option<String> {
    app.outputs.view().current.map(|c| c.info.id)
}

#[tokio::test]
async fn source_switches_the_output() {
    let mut app = services_with(Arc::new(FakeSearcher::default()), favorites_of(&["f1"]));
    restart_outputs(&mut app, FakeOutputs::default(), None).await;
    let cmd = PlayPlaylist {
        source: Some("snapcast".into()),
        ..playlist_cmd("favorites")
    };
    assert_eq!(play_playlist(&app, cmd).await, Outcome::Echo);
    assert_eq!(
        current_output_id(&app).as_deref(),
        Some("snapcast:127.0.0.1:4953")
    );
    assert_eq!(queue_ids(&app), ["f1"]);
}

#[tokio::test]
async fn unknown_source_keeps_output_and_plays() {
    let mut app = services_with(Arc::new(FakeSearcher::default()), favorites_of(&["f1"]));
    restart_outputs(&mut app, FakeOutputs::default(), Some(nidoo_choice())).await;
    let cmd = PlayPlaylist {
        source: Some("no-such-speaker".into()),
        ..playlist_cmd("favorites")
    };
    assert_eq!(play_playlist(&app, cmd).await, Outcome::Echo);
    assert_eq!(
        current_output_id(&app).as_deref(),
        Some("nidoo:127.0.0.1:4953")
    );
    assert_eq!(queue_ids(&app), ["f1"]);
}

#[tokio::test]
async fn browser_output_falls_back_to_startup() {
    let mut app = services_with(Arc::new(FakeSearcher::default()), favorites_of(&["f1"]));
    let fake = FakeOutputs::default();
    // The log output is pickable here, standing in for a browser tab (neither is a target).
    fake.extra
        .lock()
        .unwrap()
        .push(pinson::output::OutputOption {
            kind: OutputKind::Log,
            name: "Log".into(),
            needs_address: false,
            default_address: None,
            note: None,
            id: None,
            address: None,
        });
    restart_outputs(&mut app, fake, Some(nidoo_choice())).await;
    app.outputs
        .set(OutputChoice {
            kind: OutputKind::Log,
            address: None,
            name: None,
        })
        .await
        .unwrap();
    assert_eq!(current_output_id(&app).as_deref(), Some("log"));
    assert_eq!(
        play_playlist(&app, playlist_cmd("favorites")).await,
        Outcome::Echo
    );
    assert_eq!(
        current_output_id(&app).as_deref(),
        Some("nidoo:127.0.0.1:4953")
    );
}

#[tokio::test]
async fn cancels_sleep_timer() {
    let app = services_with(Arc::new(FakeSearcher::default()), favorites_of(&["f1"]));
    app.player
        .set_sleep(Some(pinson::player::SleepRequest::Minutes(30)))
        .await
        .unwrap();
    assert!(
        app.player
            .state()
            .view(tokio::time::Instant::now())
            .sleep
            .is_some()
    );
    play_playlist(&app, playlist_cmd("favorites")).await;
    assert!(
        app.player
            .state()
            .view(tokio::time::Instant::now())
            .sleep
            .is_none()
    );
}

/// The first sample the engine's reader gives, once a song has started.
async fn first_sample(engine: &FakeEngine) -> i16 {
    for _ in 0..200 {
        let taken = engine.last_reader.lock().unwrap().take();
        if let Some(mut reader) = taken {
            let mut buf = [0u8; 4];
            assert_eq!(reader.read(&mut buf).await.unwrap(), 4);
            return i16::from_le_bytes([buf[0], buf[1]]);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("no song started");
}

fn tone_services() -> (Services, FakeEngine) {
    services_parts(
        Arc::new(FakeSearcher::default()),
        favorites_of(&["f1"]),
        FakeRadio::default(),
        false,
        FakeSource {
            tone: Some(10000),
            ..FakeSource::default()
        },
    )
}

#[tokio::test]
async fn arms_fade_in() {
    let (app, engine) = tone_services();
    let cmd = PlayPlaylist {
        fade_in: Duration::from_secs(60),
        ..playlist_cmd("favorites")
    };
    assert_eq!(play_playlist(&app, cmd).await, Outcome::Echo);
    // The ramp starts silent.
    assert_eq!(first_sample(&engine).await, 0);
}

#[tokio::test]
async fn no_fade_in_plays_at_full_volume() {
    let (app, engine) = tone_services();
    assert_eq!(
        play_playlist(&app, playlist_cmd("favorites")).await,
        Outcome::Echo
    );
    assert_eq!(first_sample(&engine).await, 10000);
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
async fn stats_empty_defaults_to_month() {
    let (status, body) = get_json(&app(), "/api/stats").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["period"], "month");
    assert_eq!(body["plays"], 0);
    assert_eq!(body["topSongs"], json!([]));
    assert_eq!(body["byHour"].as_array().unwrap().len(), 24);
}

#[tokio::test]
async fn stats_counts_plays() {
    let services = services();
    for id in ["A", "B", "A"] {
        services.history.record(&song(id, "T")).unwrap();
    }
    let app = app_from(services);
    for uri in [
        "/api/stats?period=week",
        "/api/stats?period=all&tz=Europe/Paris",
        "/api/stats?tz=Not/AZone",
    ] {
        let (status, body) = get_json(&app, uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert_eq!(body["plays"], 3, "{uri}");
        assert_eq!(body["songs"], 2, "{uri}");
        assert_eq!(body["topSongs"][0]["song"]["videoId"], "A", "{uri}");
        assert_eq!(body["topSongs"][0]["plays"], 2, "{uri}");
        let hours: u64 = body["byHour"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h.as_u64().unwrap())
            .sum();
        assert_eq!(hours, 3, "{uri}");
    }
}

#[tokio::test]
async fn stats_unknown_period_is_400() {
    let (status, body) = get_json(&app(), "/api/stats?period=decade").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].is_string());
}

/// A finished stream of `bytes` that starts `start_s` seconds into the song.
fn done_stream(
    generation: u64,
    start_s: u64,
    bytes: &[u8],
) -> Arc<pinson::output::browser::Stream> {
    let buf = pinson::output::browser::StreamBuf::new();
    buf.push(bytes);
    buf.finish();
    Arc::new(pinson::output::browser::Stream::new(
        generation,
        Duration::from_secs(start_s),
        buf,
        tokio::spawn(async { Ok(()) }),
    ))
}

fn cast_bytes() -> Vec<u8> {
    (0..576 * 100).map(|i| (i % 251) as u8).collect()
}

async fn cast_get(app: &Router, uri: &str, range: Option<&str>) -> axum::http::Response<Body> {
    let mut req = Request::builder().uri(uri);
    if let Some(r) = range {
        req = req.header(header::RANGE, r);
    }
    app.clone()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

#[tokio::test]
async fn cast_stream_serves_from_offset() {
    let s = services();
    let bytes = cast_bytes();
    s.streams.set(Some(done_stream(1, 10, &bytes)));
    let app = app_from(s);
    let res = cast_get(&app, "/api/cast/stream/1?fromMs=11000", None).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers()[header::CONTENT_TYPE], "audio/mpeg");
    let body = res.into_body().collect().await.unwrap().to_bytes();
    let from = pinson::output::browser::byte_offset(1000);
    assert_eq!(body.to_vec(), bytes[from..].to_vec());
    // No fromMs: the stream's own start.
    let res = cast_get(&app, "/api/cast/stream/1", None).await;
    let body = res.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(body.to_vec(), bytes);
}

#[tokio::test]
async fn cast_stream_range_when_done() {
    let s = services();
    let bytes = cast_bytes();
    s.streams.set(Some(done_stream(1, 0, &bytes)));
    let app = app_from(s);
    let res = cast_get(&app, "/api/cast/stream/1?fromMs=0", Some("bytes=100-")).await;
    assert_eq!(res.status(), StatusCode::PARTIAL_CONTENT);
    let len = bytes.len();
    assert_eq!(
        res.headers()[header::CONTENT_RANGE],
        format!("bytes 100-{}/{len}", len - 1).as_str()
    );
    assert_eq!(
        res.headers()[header::CONTENT_LENGTH],
        (len - 100).to_string().as_str()
    );
    assert_eq!(res.headers()[header::ACCEPT_RANGES], "bytes");
    let body = res.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(body.to_vec(), bytes[100..].to_vec());
    // An open range past the end: 416, with the length.
    for n in [len, len + 576] {
        let range = format!("bytes={n}-");
        let res = cast_get(&app, "/api/cast/stream/1?fromMs=0", Some(&range)).await;
        assert_eq!(res.status(), StatusCode::RANGE_NOT_SATISFIABLE, "{range}");
        assert_eq!(
            res.headers()[header::CONTENT_RANGE],
            format!("bytes */{len}").as_str()
        );
    }
    // Not an open range, or none: the whole body, saying ranges work.
    for range in [Some("bytes=0-9"), None] {
        let res = cast_get(&app, "/api/cast/stream/1?fromMs=0", range).await;
        assert_eq!(res.status(), StatusCode::OK, "{range:?}");
        assert_eq!(res.headers()[header::ACCEPT_RANGES], "bytes");
    }
}

#[tokio::test]
async fn cast_stream_ignores_range_until_done() {
    let s = services();
    let buf = pinson::output::browser::StreamBuf::new();
    buf.push(&cast_bytes());
    s.streams
        .set(Some(Arc::new(pinson::output::browser::Stream::new(
            1,
            Duration::ZERO,
            buf.clone(),
            tokio::spawn(async { Ok(()) }),
        ))));
    let app = app_from(s);
    let res = cast_get(&app, "/api/cast/stream/1?fromMs=0", Some("bytes=100-")).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert!(res.headers().get(header::CONTENT_RANGE).is_none());
    assert!(res.headers().get(header::ACCEPT_RANGES).is_none());
}

#[tokio::test]
async fn cast_stream_ends_on_shutdown() {
    let s = services();
    let shutdown = s.shutdown.clone();
    // Still encoding: the body would wait for more bytes forever.
    let buf = pinson::output::browser::StreamBuf::new();
    buf.push(&cast_bytes());
    s.streams
        .set(Some(Arc::new(pinson::output::browser::Stream::new(
            1,
            Duration::ZERO,
            buf.clone(),
            tokio::spawn(async { Ok(()) }),
        ))));
    let app = app_from(s);
    let mut body = cast_get(&app, "/api/cast/stream/1", None).await.into_body();
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut got = 0;
        while got < cast_bytes().len() {
            let frame = body.frame().await.expect("body ended").unwrap();
            got += frame.into_data().unwrap().len();
        }
    })
    .await
    .expect("no bytes in time");
    shutdown.send_replace(true);
    tokio::time::timeout(Duration::from_secs(2), async {
        while let Some(frame) = body.frame().await {
            frame.unwrap();
        }
    })
    .await
    .expect("the Cast stream body did not end on shutdown");
}

#[tokio::test]
async fn cast_stream_404_after_switch() {
    let s = services();
    s.streams.set(Some(done_stream(1, 0, &cast_bytes())));
    let streams = s.streams.clone();
    let app = app_from(s);
    let (status, _) = send(&app, "GET", "/api/cast/stream/1", None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = send(&app, "GET", "/api/cast/stream/2", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    streams.set(None);
    let (status, body) = send(&app, "GET", "/api/cast/stream/1", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body, json!({"error": "no such stream"}));
}

#[tokio::test]
async fn upnp_stream_serves_from_offset() {
    let s = services();
    let bytes = cast_bytes();
    s.streams.set(Some(done_stream(1, 10, &bytes)));
    let app = app_from(s);
    let res = cast_get(&app, "/api/upnp/stream/1/song.mp3?fromMs=11000", None).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers()[header::CONTENT_TYPE], "audio/mpeg");
    let body = res.into_body().collect().await.unwrap().to_bytes();
    let from = pinson::output::browser::byte_offset(1000);
    assert_eq!(body.to_vec(), bytes[from..].to_vec());
    // No fromMs: the stream's own start.
    let res = cast_get(&app, "/api/upnp/stream/1/song.mp3", None).await;
    let body = res.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(body.to_vec(), bytes);
}

#[tokio::test]
async fn upnp_stream_has_dlna_headers() {
    let s = services();
    let bytes = cast_bytes();
    s.streams.set(Some(done_stream(1, 0, &bytes)));
    let app = app_from(s.clone());
    let res = cast_get(&app, "/api/upnp/stream/1/song.mp3", None).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers()["transfermode.dlna.org"], "Streaming");
    let features = res.headers()["contentfeatures.dlna.org"].to_str().unwrap();
    assert!(features.contains("DLNA.ORG_OP=01"), "{features}");
    assert_eq!(
        features,
        "DLNA.ORG_PN=MP3;DLNA.ORG_OP=01;DLNA.ORG_CI=0;DLNA.ORG_FLAGS=01700000000000000000000000000000"
    );
    let len = res.headers()[header::CONTENT_LENGTH]
        .to_str()
        .unwrap()
        .to_string();
    let body = res.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(len, body.len().to_string());
    // A range answer carries the DLNA headers too.
    let res = cast_get(&app, "/api/upnp/stream/1/song.mp3", Some("bytes=100-")).await;
    assert_eq!(res.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(res.headers()["transfermode.dlna.org"], "Streaming");
    assert!(
        res.headers()["contentfeatures.dlna.org"]
            .to_str()
            .unwrap()
            .contains("DLNA.ORG_OP=01")
    );
    // Not done: OP=00 and no length.
    let buf = pinson::output::browser::StreamBuf::new();
    buf.push(&bytes);
    s.streams
        .set(Some(Arc::new(pinson::output::browser::Stream::new(
            2,
            Duration::ZERO,
            buf,
            tokio::spawn(async { Ok(()) }),
        ))));
    let res = cast_get(&app, "/api/upnp/stream/2/song.mp3", None).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers()["transfermode.dlna.org"], "Streaming");
    let features = res.headers()["contentfeatures.dlna.org"].to_str().unwrap();
    assert!(features.contains("DLNA.ORG_OP=00"), "{features}");
    assert!(res.headers().get(header::CONTENT_LENGTH).is_none());
}

#[tokio::test]
async fn upnp_stream_head() {
    let s = services();
    let bytes = cast_bytes();
    s.streams.set(Some(done_stream(1, 0, &bytes)));
    let app = app_from(s);
    let req = Request::builder()
        .method("HEAD")
        .uri("/api/upnp/stream/1/song.mp3")
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers()["transfermode.dlna.org"], "Streaming");
    assert!(
        res.headers()["contentfeatures.dlna.org"]
            .to_str()
            .unwrap()
            .contains("DLNA.ORG_OP=01")
    );
    assert_eq!(
        res.headers()[header::CONTENT_LENGTH],
        bytes.len().to_string().as_str()
    );
}

#[tokio::test]
async fn upnp_stream_404_after_switch() {
    let s = services();
    s.streams.set(Some(done_stream(1, 0, &cast_bytes())));
    let streams = s.streams.clone();
    let app = app_from(s);
    let (status, _) = send(&app, "GET", "/api/upnp/stream/1/song.mp3", None).await;
    assert_eq!(status, StatusCode::OK);
    streams.set(None);
    let (status, body) = send(&app, "GET", "/api/upnp/stream/1/song.mp3", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body, json!({"error": "no such stream"}));
}

#[tokio::test]
async fn available_options_serialize_id_and_address() {
    let app = app();
    let (_, body) = send(&app, "GET", "/api/output", None).await;
    assert!(
        body["available"]
            .as_array()
            .unwrap()
            .iter()
            .all(|o| o.get("id").is_none() && o.get("address").is_none())
    );
    let json = serde_json::to_value(pinson::output::OutputOption {
        kind: OutputKind::Chromecast,
        name: "Kitchen".into(),
        needs_address: false,
        default_address: None,
        note: Some("Chromecast".into()),
        id: Some("chromecast:abc".into()),
        address: Some("abc".into()),
    })
    .unwrap();
    assert_eq!(json["id"], "chromecast:abc");
    assert_eq!(json["address"], "abc");
    assert_eq!(json["kind"], "chromecast");
}

#[tokio::test]
async fn output_put_switches_to_chromecast() {
    let app = app();
    let (status, body) = send(
        &app,
        "PUT",
        "/api/output",
        Some(json!({"kind": "chromecast", "address": "10.0.0.5"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["current"]["id"], "chromecast:10.0.0.5");
    assert_eq!(body["current"]["kind"], "chromecast");
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
    assert_eq!(
        body["current"]["hint"],
        "New speakers appear here to allow."
    );
    let (status, got) = send(&app, "GET", "/api/output", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got, body);
}

#[tokio::test]
async fn put_output_snapcast_switches() {
    let app = app();
    let (status, body) = send(
        &app,
        "PUT",
        "/api/output",
        Some(json!({"kind": "snapcast", "address": "127.0.0.1:4953"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["current"]["id"], "snapcast:127.0.0.1:4953");
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
            hidden: Hidden::in_memory(),
        },
        std::time::Duration::ZERO,
        pinson::autoplay::Repeat::Off,
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
    let favorites = Favorites::in_memory();
    let offline = idle_offline(&db, &favorites);
    let services = Services {
        devices: hub.clone(),
        streams: Default::default(),
        player,
        follows: Follows::new(db.clone(), searcher.clone(), Duration::ZERO),
        db,
        searcher: searcher.clone(),
        catalog: searcher.clone(),
        favorites,
        hidden: Hidden::in_memory(),
        playlists: Playlists::in_memory(),
        colors: Arc::new(CoverColors::new(artwork.clone())),
        artwork,
        picore_api_url: None,
        lyrics: Arc::new(LyricsService::new(Arc::new(FakeLrcLib), searcher)),
        outputs,
        radio,
        moods: Arc::new(FakeMoods::default()),
        history,
        offline,
        health: pinson::health::Health::for_tests(),
        shutdown: tokio::sync::watch::channel(false).0,
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
async fn next_route_without_after_follows_served() {
    let (s, hub) = services_with_device();
    let app = app_from(s);
    let mut ws = ws_greeted(&app).await;
    device_ready(&app, &hub, &mut ws).await;
    queue_songs(&app, &["a", "b", "c"]).await;
    let load = next_of_type(&mut ws, "device.load").await;
    let generation = load["generation"].as_u64().unwrap();
    let (status, body) = get_bytes(
        &app,
        &format!("/api/device/next?device={DEVICE}&after={generation}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, pattern("b"));
    let (status, body) = get_bytes(&app, &format!("/api/device/next?device={DEVICE}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, pattern("c"));
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

#[tokio::test]
async fn stream_mp3_404_when_not_current() {
    let app = app();
    let (status, _, body) = get_raw(app, "/stream.mp3").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body.contains("Stream URL isn't the current output"));
}

async fn put_output_kind(app: &Router, kind: &str) {
    let (status, _) = send(app, "PUT", "/api/output", Some(json!({"kind": kind}))).await;
    assert_eq!(status, StatusCode::OK);
}

async fn open_stream(app: &Router) -> axum::response::Response {
    let req = Request::builder()
        .uri("/stream.mp3")
        .body(Body::empty())
        .unwrap();
    app.clone().oneshot(req).await.unwrap()
}

#[tokio::test]
async fn stream_mp3_serves_audio() {
    let app = app();
    put_output_kind(&app, "stream").await;
    let res = open_stream(&app).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers()[header::CONTENT_TYPE], "audio/mpeg");
    assert_eq!(res.headers()[header::CACHE_CONTROL], "no-cache, no-store");
    let mut body = res.into_body();
    let frame = tokio::time::timeout(Duration::from_secs(5), body.frame())
        .await
        .expect("no chunk in time")
        .expect("body ended")
        .unwrap();
    assert!(!frame.into_data().unwrap().is_empty());
}

#[tokio::test]
async fn stream_ends_when_output_switches() {
    let app = app();
    put_output_kind(&app, "stream").await;
    let mut body = open_stream(&app).await.into_body();
    tokio::time::timeout(Duration::from_secs(5), body.frame())
        .await
        .expect("no chunk in time")
        .expect("body ended")
        .unwrap();
    put_output_kind(&app, "nidoo").await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(frame) = body.frame().await {
            frame.unwrap();
        }
    })
    .await
    .expect("the stream body did not end after switching output");
}

#[tokio::test]
async fn stream_ends_on_shutdown() {
    let services = services();
    let shutdown = services.shutdown.clone();
    let app = app_from(services);
    put_output_kind(&app, "stream").await;
    let mut body = open_stream(&app).await.into_body();
    tokio::time::timeout(Duration::from_secs(5), body.frame())
        .await
        .expect("no chunk in time")
        .expect("body ended")
        .unwrap();
    shutdown.send_replace(true);
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(frame) = body.frame().await {
            frame.unwrap();
        }
    })
    .await
    .expect("the stream body did not end on shutdown");
}

#[tokio::test]
async fn moments_returns_the_slot_and_picks() {
    let moods = FakeMoods::default();
    moods.set_categories(vec![MoodCategory {
        title: "Party".into(),
        params: "p-party".into(),
    }]);
    moods.set_playlists(
        "p-party",
        vec![CollectionSummary {
            kind: CollectionKind::Playlist,
            id: "RDCLAK5uy_party".into(),
            title: "Party hits".into(),
            subtitle: None,
            artwork_url: None,
        }],
    );
    let app = app_from(services_with_moods(moods));
    let (status, body) = get_json(&app, "/api/moments?hour=19&day=2026-11-14").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["slot"], "evening");
    assert_eq!(body["playlists"][0]["subtitle"], "Party");
    assert_eq!(body["playlists"][0]["kind"], "playlist");
}

#[tokio::test]
async fn moments_rejects_bad_hour_and_day() {
    let app = app_from(services());
    for uri in [
        "/api/moments?hour=24&day=2026-11-14",
        "/api/moments?hour=x&day=2026-11-14",
        "/api/moments?hour=19&day=2026-13-40",
        "/api/moments?hour=19&day=14/11/2026",
        "/api/moments?day=2026-11-14",
        "/api/moments?hour=19",
    ] {
        let (status, body) = get_json(&app, uri).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
        assert!(body["error"].is_string(), "{uri}");
    }
    for hour in ["24", "x", "-1", "300"] {
        let (status, body) =
            get_json(&app, &format!("/api/moments?hour={hour}&day=2026-11-14")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "hour={hour}");
        assert_eq!(body["error"], "hour must be 0-23", "hour={hour}");
    }
}

#[tokio::test]
async fn moments_is_empty_not_an_error_when_youtube_fails() {
    let moods = FakeMoods::default();
    moods.fail_categories();
    let app = app_from(services_with_moods(moods));
    let (status, body) = get_json(&app, "/api/moments?hour=19&day=2026-11-14").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["playlists"], json!([]));
}

fn suggestion_ids(state: &Value) -> Vec<String> {
    state["autoplay"]["songs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["videoId"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn hidden_starts_empty() {
    let (status, body) = send(&app(), "GET", "/api/hidden", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({ "songs": [], "artists": [] }));
}

#[tokio::test]
async fn hide_and_unhide_a_song() {
    let app = app();
    let (status, body) = send(
        &app,
        "POST",
        "/api/hidden/songs",
        Some(json!({ "song": song_json("a", "A") })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({ "skipped": false }));

    let (_, body) = send(&app, "GET", "/api/hidden", None).await;
    assert_eq!(body["artists"], json!([]));
    let row = &body["songs"][0];
    assert_eq!(row["videoId"], "a");
    assert_eq!(row["title"], "A");
    assert_eq!(row["artists"], "Daft Punk");
    assert_eq!(row["durationSecs"], 320);
    let at = row["hiddenAt"].as_str().unwrap();
    assert!(at.ends_with('Z') && at.contains('T'), "{at}");

    let (status, _) = send(&app, "DELETE", "/api/hidden/songs/a", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = send(&app, "DELETE", "/api/hidden/songs/a", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (_, body) = send(&app, "GET", "/api/hidden", None).await;
    assert_eq!(body["songs"], json!([]));
}

#[tokio::test]
async fn hide_song_rejects_bad_bodies() {
    let app = app();
    for body in [
        json!({}),
        json!({ "song": "a" }),
        json!({ "song": song_json("  ", "A") }),
    ] {
        let (status, _) = send(&app, "POST", "/api/hidden/songs", Some(body.clone())).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    }
    let (_, body) = send(&app, "GET", "/api/hidden", None).await;
    assert_eq!(body["songs"], json!([]));
}

#[tokio::test]
async fn hide_and_unhide_an_artist() {
    let app = app();
    let (status, body) = send(
        &app,
        "POST",
        "/api/hidden/artists",
        Some(json!({ "name": "  Daft Punk " })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({ "skipped": false }));
    let (_, body) = send(&app, "GET", "/api/hidden", None).await;
    assert_eq!(body["artists"][0]["name"], "Daft Punk");
    assert!(
        body["artists"][0]["hiddenAt"]
            .as_str()
            .unwrap()
            .ends_with('Z')
    );

    // URL-encoded and case-insensitive
    let (status, _) = send(&app, "DELETE", "/api/hidden/artists/daft%20punk", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = send(&app, "DELETE", "/api/hidden/artists/Daft%20Punk", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn hide_artist_rejects_blank_and_bad_bodies() {
    let app = app();
    for body in [json!({ "name": "" }), json!({ "name": "   " }), json!({})] {
        let (status, _) = send(&app, "POST", "/api/hidden/artists", Some(body.clone())).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    }
}

#[tokio::test]
async fn hiding_the_playing_song_skips_it() {
    let app = app();
    queue_next(&app, "a").await;
    queue_next(&app, "b").await;
    let (_, state) = send(&app, "GET", "/api/state", None).await;
    assert_eq!(state["current"]["song"]["videoId"], "a");

    // a song that is not playing: no skip
    let (_, body) = send(
        &app,
        "POST",
        "/api/hidden/songs",
        Some(json!({ "song": song_json("zzz", "Z") })),
    )
    .await;
    assert_eq!(body, json!({ "skipped": false }));
    let (_, state) = send(&app, "GET", "/api/state", None).await;
    assert_eq!(state["current"]["song"]["videoId"], "a");

    let (status, body) = send(
        &app,
        "POST",
        "/api/hidden/songs",
        Some(json!({ "song": song_json("a", "a") })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({ "skipped": true }));
    let (_, state) = send(&app, "GET", "/api/state", None).await;
    assert_eq!(state["current"]["song"]["videoId"], "b");
}

#[tokio::test]
async fn hiding_the_playing_songs_artist_skips_it() {
    let app = app();
    queue_next(&app, "a").await;
    queue_next(&app, "b").await;
    let (status, body) = send(
        &app,
        "POST",
        "/api/hidden/artists",
        Some(json!({ "name": "DAFT PUNK" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({ "skipped": true }));
    // the next song is by the same artist, but an explicit queue entry still plays
    let (_, state) = send(&app, "GET", "/api/state", None).await;
    assert_eq!(state["current"]["song"]["videoId"], "b");
}

#[tokio::test]
async fn hidden_songs_leave_the_suggestions_and_come_back() {
    let radio = FakeRadio::default();
    radio.set("a", ["a", "x", "y"].map(radio_song).into());
    let app = app_from(services_with_radio(radio, true));
    queue_next(&app, "a").await;
    let state = wait_for_suggestions(&app).await;
    assert_eq!(suggestion_ids(&state), ["x", "y"]);

    let (status, _) = send(
        &app,
        "POST",
        "/api/hidden/songs",
        Some(json!({ "song": song_json("x", "x") })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, state) = send(&app, "GET", "/api/state", None).await;
    assert_eq!(suggestion_ids(&state), ["y"]);

    let (status, _) = send(&app, "DELETE", "/api/hidden/songs/x", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, state) = send(&app, "GET", "/api/state", None).await;
    assert_eq!(suggestion_ids(&state), ["x", "y"]);
}

#[tokio::test]
async fn hidden_artists_leave_the_suggestions() {
    let radio = FakeRadio::default();
    // radio_song credits everything to "Daft Punk"; give one song another artist
    let mut other = radio_song("y");
    other.artists = "Air".into();
    radio.set("a", vec![radio_song("a"), radio_song("x"), other]);
    let services = services_with_radio(radio, true);
    let app = app_from(services);
    // `a` is by someone else, so hiding Daft Punk would skip it: play a song by Air
    let mut seed = radio_song("a");
    seed.artists = "Air".into();
    send(
        &app,
        "POST",
        "/api/queue/next",
        Some(serde_json::to_value(seed).unwrap()),
    )
    .await;
    wait_for_suggestions(&app).await;
    let (_, body) = send(
        &app,
        "POST",
        "/api/hidden/artists",
        Some(json!({ "name": "Daft Punk" })),
    )
    .await;
    assert_eq!(body, json!({ "skipped": false }));
    let (_, state) = send(&app, "GET", "/api/state", None).await;
    assert_eq!(suggestion_ids(&state), ["y"]);
}

#[tokio::test]
async fn mixes_leave_out_hidden_songs() {
    let radio = FakeRadio::default();
    radio.set("A", ["A", "x", "y"].map(radio_song).into());
    let services = services_with_mixes(radio);
    services.history.record(&song("A", "Alpha")).unwrap();
    let app = app_from(services);
    send(
        &app,
        "POST",
        "/api/hidden/songs",
        Some(json!({ "song": song_json("x", "x") })),
    )
    .await;
    let (_, body) = get_json(&app, "/api/mixes").await;
    let ids: Vec<&str> = body[0]["songs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["videoId"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["A", "y"]);
    let (_, one) = get_json(&app, "/api/mixes/A").await;
    assert_eq!(one["songs"].as_array().unwrap().len(), 2);

    // hiding the seed's artist leaves Home with no mix from it
    send(
        &app,
        "POST",
        "/api/hidden/artists",
        Some(json!({ "name": "Daft Punk" })),
    )
    .await;
    let (_, body) = get_json(&app, "/api/mixes").await;
    assert_eq!(body, json!([]));
}
#[tokio::test]
async fn health_reports_the_documented_shape() {
    let services = services();
    let health = services.health.clone();
    health
        .errors()
        .push("warn", "pinson::player", "slow".into());
    health
        .errors()
        .push("warn", "pinson::player", "slow".into());
    let app = app_from(services);
    let (status, body) = get_json(&app, "/api/health").await;
    assert_eq!(status, StatusCode::OK);
    println!("{body:#}");
    assert_eq!(body["pinson"]["version"], env!("CARGO_PKG_VERSION"));
    assert!(body["pinson"]["startedAt"].as_str().unwrap().ends_with('Z'));
    assert!(body["pinson"]["uptimeSecs"].is_u64());
    assert_eq!(body["ytDlp"]["update"], json!({"state": "idle"}));
    assert_eq!(body["ytDlp"]["managed"], false);
    assert!(body["ytDlp"]["version"].is_null());
    assert!(body["ytDlp"]["latest"].is_null());
    assert!(body["ytDlp"]["path"].is_string());
    assert_eq!(body["tools"][0]["name"], "ffmpeg");
    assert_eq!(body["tools"][1]["name"], "deno");
    assert!(body["tools"][1]["version"].is_null());
    assert!(body["output"].is_null());
    assert!(body["check"].is_null());
    let problems = body["problems"].as_array().unwrap();
    assert_eq!(problems.len(), 1);
    assert_eq!(problems[0]["level"], "warn");
    assert_eq!(problems[0]["target"], "pinson::player");
    assert_eq!(problems[0]["message"], "slow");
    assert_eq!(problems[0]["count"], 2);
    assert!(problems[0]["at"].as_str().unwrap().ends_with('Z'));
}

#[tokio::test]
async fn health_check_without_a_source_reports_the_error() {
    let app = app();
    let (status, body) = send(&app, "POST", "/api/health/check", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], false);
    assert_eq!(body["videoId"], "dQw4w9WgXcQ");
    assert!(body["error"].is_string());
    assert!(body.get("title").is_none());
    assert!(body["ms"].is_u64());
    let (_, health) = get_json(&app, "/api/health").await;
    assert_eq!(health["check"]["videoId"], "dQw4w9WgXcQ");
}

#[cfg(unix)]
mod health_scripts {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn script(path: &std::path::Path, body: &str) {
        std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn temp(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("pinson-api-health-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn health_check_uses_the_latest_play_and_keeps_the_result() {
        let dir = temp("check");
        let fake = dir.join("yt-dlp");
        script(&fake, r#"echo "Title of $(echo "$@" | sed 's/.*v=//')""#);
        let source = Arc::new(pinson::source::YtDlpFfmpegSource::new(
            fake.clone().into(),
            "ffmpeg".into(),
            None,
            None,
            None,
            None,
            None,
        ));
        let mut services = services();
        services.health = pinson::health::Health::for_tests_with(Some(source));
        services
            .history
            .record(&Song {
                video_id: "abc123".into(),
                title: "Played".into(),
                artists: "A".into(),
                album: None,
                duration_secs: None,
                artwork_url: None,
            })
            .unwrap();
        let app = app_from(services);
        let (status, body) = send(&app, "POST", "/api/health/check", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["ok"], true);
        assert_eq!(body["videoId"], "abc123");
        assert_eq!(body["title"], "Title of abc123");
        assert!(body.get("error").is_none());
        let (_, health) = get_json(&app, "/api/health").await;
        assert_eq!(health["check"]["title"], "Title of abc123");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn health_check_failure_gives_the_last_stderr_line() {
        let dir = temp("checkfail");
        let fake = dir.join("yt-dlp");
        script(
            &fake,
            "echo 'first' >&2; echo 'ERROR: Sign in to confirm' >&2; exit 1",
        );
        let source = Arc::new(pinson::source::YtDlpFfmpegSource::new(
            fake.into(),
            "ffmpeg".into(),
            None,
            None,
            None,
            None,
            None,
        ));
        let mut services = services();
        services.health = pinson::health::Health::for_tests_with(Some(source));
        let (_, body) = send(&app_from(services), "POST", "/api/health/check", None).await;
        assert_eq!(body["ok"], false);
        assert_eq!(body["error"], "ERROR: Sign in to confirm");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn health_update_runs_one_at_a_time() {
        use pinson::errlog::ErrorLog;
        use pinson::health::Health;
        use pinson::ytdlp::YtDlp;
        let dir = temp("update");
        let python = dir.join("python3");
        // Slow enough to still be running for the second request, then a failure.
        script(&python, "sleep 1; echo 'no venv here' >&2; exit 3");
        let ytdlp = YtDlp::new(
            dir.join("configured").into(),
            dir.join("configured"),
            dir.join("managed"),
            python,
            None,
        );
        let mut services = services();
        services.health = Health::new(
            ErrorLog::default(),
            ytdlp,
            None,
            "ffmpeg".into(),
            "deno".into(),
        );
        let app = app_from(services);
        let (status, body) = send(&app, "POST", "/api/health/yt-dlp/update", None).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        assert_eq!(body["state"], "running");
        assert!(body["startedAt"].is_string());
        let (status, body) = send(&app, "POST", "/api/health/yt-dlp/update", None).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(body["error"].is_string());
        let (_, health) = get_json(&app, "/api/health").await;
        assert_eq!(health["ytDlp"]["update"]["state"], "running");
        for _ in 0..200 {
            let (_, health) = get_json(&app, "/api/health").await;
            let update = &health["ytDlp"]["update"];
            if update["state"] == "failed" {
                assert!(
                    update["error"]
                        .as_str()
                        .unwrap()
                        .contains("creating the venv failed")
                );
                assert_eq!(update["log"], json!(["no venv here"]));
                assert!(update["at"].is_string());
                let _ = std::fs::remove_dir_all(&dir);
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("update never failed");
    }
}

/// Inserts a play of `id` that started `days` days ago.
fn play_days_ago(services: &Services, id: &str, days: i64) {
    let at = (chrono::Utc::now() - chrono::Duration::days(days)).to_rfc3339();
    services
        .db
        .with(|c| {
            let tx = c.transaction()?;
            tx.execute(
                "INSERT INTO songs (video_id, title, artists) VALUES (?1, ?2, 'Daft Punk')
                 ON CONFLICT(video_id) DO NOTHING",
                (id, format!("Title {id}")),
            )?;
            tx.execute(
                "INSERT INTO history (played_at, song_id) SELECT ?1, id FROM songs WHERE video_id = ?2",
                (at, id),
            )?;
            tx.commit()
        })
        .unwrap();
}

#[tokio::test]
async fn smart_empty_without_history() {
    let app = app();
    let (status, body) = get_json(&app, "/api/smart").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({ "lists": [] }));
    let (status, _) = get_json(&app, "/api/smart/on-repeat").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn smart_lists_and_one_list() {
    let services = services();
    for i in 0..5 {
        let id = format!("r{i}");
        play_days_ago(&services, &id, 1);
        play_days_ago(&services, &id, 2);
    }
    for i in 0..5 {
        play_days_ago(&services, &format!("o{i}"), 20);
    }
    let app = app_from(services);
    let (status, body) = get_json(&app, "/api/smart").await;
    assert_eq!(status, StatusCode::OK);
    let lists = body["lists"].as_array().unwrap();
    assert_eq!(lists.len(), 2);
    assert_eq!(lists[0]["kind"], "smart");
    assert_eq!(lists[0]["id"], "on-repeat");
    assert_eq!(lists[0]["title"], "On repeat");
    assert_eq!(
        lists[0]["subtitle"],
        "5 songs · Your most played this month"
    );
    assert_eq!(lists[1]["id"], "heard-once");
    assert!(lists[0].get("songs").is_none());

    let (status, body) = get_json(&app, "/api/smart/heard-once").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["kind"], "smart");
    assert_eq!(body["songs"].as_array().unwrap().len(), 5);

    let (status, body) = get_json(&app, "/api/smart/forgotten-favorites").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("forgotten-favorites")
    );
    let (status, _) = get_json(&app, "/api/smart/nope").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn smart_forgotten_favorites_use_favorites_order() {
    let services = services();
    for id in ["a", "b", "c", "d", "e", "f"] {
        services.favorites.add(song(id, id)).unwrap();
    }
    play_days_ago(&services, "c", 1);
    let app = app_from(services);
    let (_, body) = get_json(&app, "/api/smart/forgotten-favorites").await;
    // Newest-added favorite first; the recently played one is left out.
    let ids: Vec<&str> = body["songs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["videoId"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["f", "e", "d", "b", "a"]);
}
