use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use clap::Parser;
use tokio::sync::mpsc;
use tracing_subscriber::EnvFilter;

use pinson::artwork::ArtworkProxy;
use pinson::config::{Config, EngineKind};
use pinson::engine::AudioEngine;
use pinson::engine::log::LoggingEngine;
use pinson::engine::nidoo::{NidooSoundConfig, NidooSoundEngine};
use pinson::favorites::Favorites;
use pinson::lyrics::{LrcLib, LyricsService};
use pinson::player::spawn_player;
use pinson::playlists::Playlists;
use pinson::search::YtmSearcher;
use pinson::source::YtDlpFfmpegSource;
use pinson::speakers::{SpeakerControl, control_url};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let cfg = Config::parse();

    let (events_tx, events_rx) = mpsc::unbounded_channel();
    let engine: Box<dyn AudioEngine> = match cfg.engine {
        EngineKind::Nidoo => Box::new(NidooSoundEngine::spawn(
            NidooSoundConfig {
                addr: cfg.nidoo_sound.clone(),
                max_ahead: Duration::from_millis(200),
                latency: Duration::from_millis(cfg.nidoo_latency_ms),
            },
            events_tx,
        )),
        EngineKind::Log => {
            drop(events_tx);
            Box::new(LoggingEngine::default())
        }
    };
    let source = Arc::new(YtDlpFfmpegSource {
        yt_dlp: cfg.yt_dlp.clone(),
        ffmpeg: cfg.ffmpeg.clone(),
        cookies_from_browser: cfg.cookies_from_browser.clone(),
        cookies_file: cfg.cookies_file.clone(),
    });
    let player = spawn_player(engine, events_rx, source);
    let searcher = Arc::new(YtmSearcher::lazy(cfg.ytm_cookie_file.clone()));

    // TODO(task 4): --db flag and JSON import
    let db = pinson::db::Db::open(std::path::Path::new("pinson.db"))?;
    let favorites = Favorites::open(db.clone())?;

    let playlists = Playlists::open(db.clone())?;

    let lyrics = Arc::new(LyricsService::new(
        Arc::new(LrcLib::new("https://lrclib.net")),
        searcher.clone(),
    ));
    let speakers = (cfg.engine == EngineKind::Nidoo).then(|| {
        let base = control_url(&cfg.nidoo_sound, cfg.nidoo_control.as_deref());
        tracing::info!("speaker control: {base}");
        Arc::new(SpeakerControl::new(base, Duration::from_secs(2)))
    });
    let app = pinson::api::router(pinson::api::Services {
        player,
        searcher: searcher.clone(),
        catalog: searcher,
        lyrics,
        favorites,
        playlists,
        artwork: Arc::new(ArtworkProxy::youtube()),
        speakers,
    })
    .merge(pinson::web::web_routes());
    let addr = SocketAddr::new(cfg.bind, cfg.port);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("listening on {addr}"))?;
    tracing::info!("music server on http://{addr} (engine: {:?})", cfg.engine);
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    tracing::info!("stopped");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {}
        () = term => {}
    }
}
