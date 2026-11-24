use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use clap::Parser;
use tokio::sync::mpsc;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, fmt};

use pinson::artwork::ArtworkProxy;
use pinson::autoplay::{autoplay_enabled, crossfade_secs, repeat_mode};
use pinson::config::Config;
use pinson::db::Db;
use pinson::errlog::ErrorLog;
use pinson::favorites::Favorites;
use pinson::follows::{Follows, REFRESH_INTERVAL, REFRESH_PAUSE, spawn_refresher};
use pinson::health::Health;
use pinson::history::{self, History};
use pinson::import::import_json_once;
use pinson::loudness::{LevelCache, Loudness};
use pinson::lyrics::{LrcLib, LyricsService};
use pinson::moods::{MOODS_TTL, Moods, MoodsCache};
use pinson::offline::{Mp3Downloader, Offline, OfflineIndex};
use pinson::output::browser::DeviceHub;
use pinson::output::discovery::Discovery;
use pinson::output::local::Gain;
use pinson::output::renderer::SongStreams;
use pinson::output::{DefaultOutputs, Outputs};
use pinson::player::{Autoplay, spawn_player};
use pinson::playlists::Playlists;
use pinson::radio::{RADIO_TTL, Radio, RadioCache};
use pinson::search::YtmSearcher;
use pinson::source::YtDlpFfmpegSource;
use pinson::ytdlp::{PYPI_URL, YtDlp, YtDlpBin, managed_dir};

fn main() -> anyhow::Result<()> {
    // `./.env` sets variables that are not already set, so the shell's env and the flags win.
    // Loaded before the runtime starts: no other thread exists while the env changes.
    let dotenv = match dotenvy::from_path(".env") {
        Ok(()) => true,
        Err(e) if e.not_found() => false,
        Err(e) => return Err(e).context("reading .env"),
    };
    let errors = ErrorLog::default();
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with(fmt::layer())
        .with(errors.clone())
        .init();
    if dotenv {
        tracing::info!("loaded settings from .env");
    }
    let cfg = Config::parse();
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("starting the async runtime")?
        .block_on(run(cfg, errors))
}

/// An empty `<temp dir>/pinson-info` for saved song info; `None` if it can't
/// be made (songs then open the slow way).
fn fresh_info_dir() -> Option<std::path::PathBuf> {
    let dir = std::env::temp_dir().join("pinson-info");
    let made = match std::fs::remove_dir_all(&dir) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => std::fs::create_dir_all(&dir),
    };
    match made {
        Ok(()) => Some(dir),
        Err(e) => {
            tracing::warn!("saved song info is off: {}: {e}", dir.display());
            None
        }
    }
}

/// Where saved favorites go: `offline/` next to the database.
fn offline_dir(db: &std::path::Path) -> std::path::PathBuf {
    let parent = db.parent().filter(|p| !p.as_os_str().is_empty());
    parent.unwrap_or(std::path::Path::new(".")).join("offline")
}

async fn run(cfg: Config, errors: ErrorLog) -> anyhow::Result<()> {
    let (events_tx, events_rx) = mpsc::unbounded_channel();
    let db = Db::open(&cfg.db)?;
    let hidden = pinson::hidden::Hidden::open(db.clone())?;
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
    let favorites = Favorites::open(db.clone())?;
    let offline_dir = offline_dir(&cfg.db);
    let offline_index = OfflineIndex::open(&offline_dir, db.clone());
    let yt_dlp_bin = YtDlpBin::from(cfg.yt_dlp.clone());
    let source = Arc::new(
        YtDlpFfmpegSource::new(
            yt_dlp_bin.clone(),
            cfg.ffmpeg.clone(),
            cfg.cookies_from_browser.clone(),
            cfg.cookies_file.clone(),
            Some(cfg.yt_clients.trim().to_owned()).filter(|c| !c.is_empty()),
            cfg.loudness.map(|target_lufs| Loudness {
                target_lufs,
                cache: LevelCache::default(),
            }),
            fresh_info_dir(),
        )
        .with_offline(offline_index.clone()),
    );
    let ytdlp = YtDlp::new(
        yt_dlp_bin,
        cfg.yt_dlp.clone(),
        managed_dir(&cfg.db),
        "python3".into(),
        Some(PYPI_URL.to_owned()),
    );
    ytdlp.start();
    let health = Health::new(
        errors,
        ytdlp,
        Some(source.clone()),
        cfg.ffmpeg.clone(),
        "deno".into(),
    );
    match cfg.loudness {
        Some(target) => tracing::info!("loudness: {target} LUFS"),
        None => tracing::info!("loudness: off"),
    }
    let searcher = Arc::new(YtmSearcher::lazy(cfg.ytm_cookie_file.clone()));

    let lyrics = Arc::new(LyricsService::new(
        Arc::new(LrcLib::new("https://lrclib.net")),
        searcher.clone(),
    ));
    let artwork = Arc::new(ArtworkProxy::youtube());
    let offline = Offline::new(
        offline_dir,
        offline_index,
        db.clone(),
        favorites.clone(),
        Some(cfg.offline_max_gb)
            .filter(|gb| *gb > 0)
            .map(|gb| gb.saturating_mul(1 << 30)),
        Arc::new(Mp3Downloader::new(
            source.clone(),
            artwork.clone(),
            lyrics.clone(),
        )),
    )?;
    offline.start();
    let radio: Arc<dyn Radio> = Arc::new(RadioCache::new(searcher.clone(), RADIO_TTL));
    let moods: Arc<dyn Moods> = Arc::new(MoodsCache::new(searcher.clone(), MOODS_TTL));
    let player = spawn_player(
        None,
        events_rx,
        source,
        Autoplay {
            radio: radio.clone(),
            history: History::new(db.clone()),
            enabled: autoplay_enabled(&db)?,
            hidden: hidden.clone(),
        },
        Duration::from_secs(crossfade_secs(&db)?.into()),
        repeat_mode(&db)?,
    );
    let devices = DeviceHub::new(player.clone(), Duration::from_secs(10));
    let streams = SongStreams::default();
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
            streams: streams.clone(),
            chromecast: cfg.chromecast.clone(),
            upnp: cfg.upnp.clone(),
            airplay: cfg.airplay.clone(),
            airplay_latency: Duration::from_millis(cfg.airplay_latency_ms),
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
    let playlists = Playlists::open(db.clone())?;
    let follows = Follows::new(db.clone(), searcher.clone(), REFRESH_PAUSE);
    let refresher = spawn_refresher(follows.clone(), REFRESH_INTERVAL);

    let (shutdown, _) = tokio::sync::watch::channel(false);
    let stop = shutdown.clone();
    let released = outputs.clone();
    let services = pinson::api::Services {
        devices,
        streams,
        player,
        db,
        searcher: searcher.clone(),
        catalog: searcher,
        lyrics,
        favorites,
        hidden,
        playlists,
        follows,
        colors: Arc::new(pinson::cover_color::CoverColors::new(artwork.clone())),
        artwork,
        picore_api_url: cfg.picore_api_url.clone(),
        outputs,
        radio,
        moods,
        history,
        offline,
        health,
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
    refresher.abort();
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
