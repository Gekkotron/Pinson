use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use clap::Parser;
use tokio::sync::mpsc;
use tracing_subscriber::EnvFilter;

use music::config::{Config, EngineKind};
use music::engine::AudioEngine;
use music::engine::log::LoggingEngine;
use music::engine::nidoo::{NidooSoundConfig, NidooSoundEngine};
use music::favorites::Favorites;
use music::player::spawn_player;
use music::search::YtmSearcher;
use music::source::YtDlpFfmpegSource;

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

    let favorites = Favorites::load(cfg.favorites_file.clone())?;
    tracing::info!("favorites file: {}", cfg.favorites_file.display());

    let app = music::api::router(player, searcher, favorites).merge(music::web::web_routes());
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
