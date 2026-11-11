use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use clap::Parser;
use tokio::sync::mpsc;
use tracing_subscriber::EnvFilter;

use pinson::artwork::ArtworkProxy;
use pinson::autoplay::autoplay_enabled;
use pinson::config::Config;
use pinson::db::Db;
use pinson::favorites::Favorites;
use pinson::history::{self, History};
use pinson::import::import_json_once;
use pinson::lyrics::{LrcLib, LyricsService};
use pinson::output::browser::DeviceHub;
use pinson::output::cast::CastStreams;
use pinson::output::discovery::Discovery;
use pinson::output::local::Gain;
use pinson::output::{DefaultOutputs, Outputs};
use pinson::player::{Autoplay, spawn_player};
use pinson::playlists::Playlists;
use pinson::radio::{RADIO_TTL, Radio, RadioCache};
use pinson::search::YtmSearcher;
use pinson::source::YtDlpFfmpegSource;

fn main() -> anyhow::Result<()> {
    // `./.env` sets variables that are not already set, so the shell's env and the flags win.
    // Loaded before the runtime starts: no other thread exists while the env changes.
    let dotenv = match dotenvy::from_path(".env") {
        Ok(()) => true,
        Err(e) if e.not_found() => false,
        Err(e) => return Err(e).context("reading .env"),
    };
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    if dotenv {
        tracing::info!("loaded settings from .env");
    }
    let cfg = Config::parse();
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("starting the async runtime")?
        .block_on(run(cfg))
}

async fn run(cfg: Config) -> anyhow::Result<()> {
    let (events_tx, events_rx) = mpsc::unbounded_channel();
    let source = Arc::new(YtDlpFfmpegSource {
        yt_dlp: cfg.yt_dlp.clone(),
        ffmpeg: cfg.ffmpeg.clone(),
        cookies_from_browser: cfg.cookies_from_browser.clone(),
        cookies_file: cfg.cookies_file.clone(),
        player_clients: Some(cfg.yt_clients.trim().to_owned()).filter(|c| !c.is_empty()),
    });
    let searcher = Arc::new(YtmSearcher::lazy(cfg.ytm_cookie_file.clone()));

    let db = Db::open(&cfg.db)?;
    tracing::info!("database: {}", cfg.db.display());
    let report = import_json_once(&db, &cfg.favorites_file, &cfg.playlists_file)?;
    if !report.already_done {
        if let Some(n) = report.favorites {
            tracing::info!(
                "imported {n} favorites from {}",
                cfg.favorites_file.display()
            );
        }
        if let Some(n) = report.playlists {
            tracing::info!(
                "imported {n} playlists from {}",
                cfg.playlists_file.display()
            );
        }
    }
    let radio: Arc<dyn Radio> = Arc::new(RadioCache::new(searcher.clone(), RADIO_TTL));
    let player = spawn_player(
        None,
        events_rx,
        source,
        Autoplay {
            radio: radio.clone(),
            history: History::new(db.clone()),
            enabled: autoplay_enabled(&db)?,
        },
    );
    let devices = DeviceHub::new(player.clone(), Duration::from_secs(10));
    let casts = CastStreams::default();
    let outputs = Outputs::start(
        Arc::new(DefaultOutputs {
            devices: devices.clone(),
            ffmpeg: cfg.ffmpeg.clone(),
            nidoo_sound: cfg.nidoo_sound.clone(),
            nidoo_control: cfg.nidoo_control.clone(),
            nidoo_player_url: cfg.nidoo_player_url.clone(),
            nidoo_latency: Duration::from_millis(cfg.nidoo_latency_ms),
            snapcast: cfg.snapcast.clone(),
            snapcast_control: cfg.snapcast_control.clone(),
            snapcast_latency: Duration::from_millis(cfg.snapcast_latency_ms),
            stream_latency: Duration::from_millis(cfg.stream_latency_ms),
            local: pinson::output::local::probe(),
            gain: Arc::new(Gain::default()),
            discovery: Discovery::spawn(),
            player: player.clone(),
            casts: casts.clone(),
            chromecast: cfg.chromecast.clone(),
            public_url: cfg.public_url.clone(),
            port: cfg.port,
            bind: cfg.bind,
        }),
        events_tx,
        db.clone(),
        player.clone(),
        cfg.startup_output(),
    )
    .await?;
    let output_id = outputs.view().current.map(|c| c.info.id);
    if output_id.is_none() {
        tracing::warn!(
            "no output configured: choose one in the web app, or set MUSIC_ENGINE (nidoo keeps the old behaviour)"
        );
    }
    let history = History::new(db.clone());
    history::spawn_recorder(history.clone(), player.plays());
    let favorites = Favorites::open(db.clone())?;
    let playlists = Playlists::open(db.clone())?;

    let lyrics = Arc::new(LyricsService::new(
        Arc::new(LrcLib::new("https://lrclib.net")),
        searcher.clone(),
    ));
    let artwork = Arc::new(ArtworkProxy::youtube());
    let (shutdown, _) = tokio::sync::watch::channel(false);
    let stop = shutdown.clone();
    let released = outputs.clone();
    let services = pinson::api::Services {
        devices,
        casts,
        player,
        db,
        searcher: searcher.clone(),
        catalog: searcher,
        lyrics,
        favorites,
        playlists,
        colors: Arc::new(pinson::cover_color::CoverColors::new(artwork.clone())),
        artwork,
        outputs,
        radio,
        history,
        shutdown,
    };
    let mqtt = cfg.mqtt()?.map(|m| {
        tracing::info!("mqtt: {}:{} root {}", m.host, m.port, m.root);
        pinson::mqtt::spawn(m, services.clone())
    });
    let app = pinson::api::router(services).merge(pinson::web::web_routes());
    let addr = SocketAddr::new(cfg.bind, cfg.port);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("listening on {addr}"))?;
    tracing::info!(
        "music server on http://{addr} (output: {})",
        output_id.as_deref().unwrap_or("none")
    );
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            // Ends the stream bodies (/stream.mp3, Cast and device songs), which would
            // otherwise hold this up.
            stop.send_replace(true);
            // Drops the output so a Chromecast stops its app (bounded to 2 s).
            released.release().await;
        })
        .await?;
    if let Some(m) = mqtt {
        m.shutdown().await;
    }
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
