//! Command-line flags; every flag can also be set with a `MUSIC_*` env var.

use std::net::IpAddr;
use std::path::PathBuf;

use clap::{Parser, ValueEnum};

#[derive(Clone, Copy, Debug, PartialEq, ValueEnum)]
pub enum EngineKind {
    /// Paced PCM over TCP to nidoo-sound.
    Nidoo,
    /// Log engine calls only (no sound).
    Log,
}

#[derive(Clone, Debug, Parser)]
#[command(
    name = "music",
    version,
    about = "House music server: YouTube Music through nidoo-sound"
)]
pub struct Config {
    /// Address to listen on.
    #[arg(long, env = "MUSIC_BIND", default_value = "0.0.0.0")]
    pub bind: IpAddr,
    /// Port for the web page and the API.
    #[arg(long, env = "MUSIC_PORT", default_value_t = 8082)]
    pub port: u16,
    /// yt-dlp executable.
    #[arg(long, env = "MUSIC_YT_DLP", default_value = "yt-dlp")]
    pub yt_dlp: PathBuf,
    /// ffmpeg executable.
    #[arg(long, env = "MUSIC_FFMPEG", default_value = "ffmpeg")]
    pub ffmpeg: PathBuf,
    /// Passed to yt-dlp as --cookies-from-browser (e.g. "firefox").
    #[arg(long, env = "MUSIC_COOKIES_FROM_BROWSER")]
    pub cookies_from_browser: Option<String>,
    /// Passed to yt-dlp as --cookies (Netscape cookie file).
    #[arg(long, env = "MUSIC_COOKIES_FILE")]
    pub cookies_file: Option<PathBuf>,
    /// Browser cookie file for YouTube Music search, if anonymous search fails.
    #[arg(long, env = "MUSIC_YTM_COOKIE_FILE")]
    pub ytm_cookie_file: Option<PathBuf>,
    /// File that stores the favorites (created on the first change).
    #[arg(long, env = "MUSIC_FAVORITES_FILE", default_value = "favorites.json")]
    pub favorites_file: PathBuf,
    /// Audio output.
    #[arg(long, env = "MUSIC_ENGINE", value_enum, default_value = "nidoo")]
    pub engine: EngineKind,
    /// nidoo-sound PCM input (host:port).
    #[arg(long, env = "MUSIC_NIDOO_SOUND", default_value = "192.168.1.91:4953")]
    pub nidoo_sound: String,
    /// Delay from a written byte to the speakers, in ms (nidoo-sound's buffer + lead).
    #[arg(long, env = "MUSIC_NIDOO_LATENCY_MS", default_value_t = 750)]
    pub nidoo_latency_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults() {
        let cfg = Config::try_parse_from(["music"]).unwrap();
        assert_eq!(cfg.bind.to_string(), "0.0.0.0");
        assert_eq!(cfg.port, 8082);
        assert_eq!(cfg.yt_dlp, PathBuf::from("yt-dlp"));
        assert_eq!(cfg.engine, EngineKind::Nidoo);
        assert_eq!(cfg.nidoo_sound, "192.168.1.91:4953");
        assert_eq!(cfg.nidoo_latency_ms, 750);
        assert!(cfg.cookies_from_browser.is_none());
        assert_eq!(cfg.favorites_file, PathBuf::from("favorites.json"));
    }

    #[test]
    fn config_flags() {
        let cfg = Config::try_parse_from([
            "music",
            "--port",
            "9000",
            "--cookies-from-browser",
            "firefox",
            "--engine",
            "log",
            "--nidoo-latency-ms",
            "500",
            "--favorites-file",
            "/var/lib/music/favs.json",
        ])
        .unwrap();
        assert_eq!(cfg.port, 9000);
        assert_eq!(cfg.cookies_from_browser.as_deref(), Some("firefox"));
        assert_eq!(cfg.engine, EngineKind::Log);
        assert_eq!(cfg.nidoo_latency_ms, 500);
        assert_eq!(
            cfg.favorites_file,
            PathBuf::from("/var/lib/music/favs.json")
        );
    }
}
