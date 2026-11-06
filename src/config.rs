//! Command-line flags; every flag can also be set with a `MUSIC_*` env var.

use std::fmt;
use std::net::IpAddr;
use std::path::PathBuf;

use anyhow::Context;
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
    name = "pinson",
    version,
    about = "Pinson, the house music server: YouTube Music through nidoo-sound"
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
    /// SQLite file holding favorites, playlists, play history and settings.
    #[arg(long, env = "MUSIC_DB", default_value = "pinson.db")]
    pub db: PathBuf,
    /// Old favorites JSON file, imported into the database once, then renamed to `*.imported`.
    #[arg(long, env = "MUSIC_FAVORITES_FILE", default_value = "favorites.json")]
    pub favorites_file: PathBuf,
    /// Old playlists JSON file, imported into the database once, then renamed to `*.imported`.
    #[arg(long, env = "MUSIC_PLAYLISTS_FILE", default_value = "playlists.json")]
    pub playlists_file: PathBuf,
    /// Audio output.
    #[arg(long, env = "MUSIC_ENGINE", value_enum, default_value = "nidoo")]
    pub engine: EngineKind,
    /// nidoo-sound PCM input (host:port).
    #[arg(long, env = "MUSIC_NIDOO_SOUND", default_value = "192.168.1.91:4953")]
    pub nidoo_sound: String,
    /// nidoo-sound HTTP API (http://host:port); default: MUSIC_NIDOO_SOUND's host on port 4954.
    #[arg(long, env = "MUSIC_NIDOO_CONTROL")]
    pub nidoo_control: Option<String>,
    /// Delay from a written byte to the speakers, in ms (nidoo-sound's buffer + lead).
    #[arg(long, env = "MUSIC_NIDOO_LATENCY_MS", default_value_t = 750)]
    pub nidoo_latency_ms: u64,
    /// MQTT broker, as `host[:port]` (port 1883 by default). Unset: MQTT off.
    #[arg(long, env = "MUSIC_MQTT_BROKER")]
    pub mqtt_broker: Option<String>,
    /// MQTT user name, if the broker needs one.
    #[arg(long, env = "MUSIC_MQTT_USER")]
    pub mqtt_user: Option<String>,
    /// MQTT password, if the broker needs one. Never logged. Prefer the env var: a flag shows in `ps`.
    #[arg(long, env = "MUSIC_MQTT_PASSWORD", hide_env_values = true)]
    pub mqtt_password: Option<String>,
    /// Home Assistant MQTT discovery prefix.
    #[arg(
        long,
        env = "MUSIC_MQTT_DISCOVERY_PREFIX",
        default_value = "homeassistant"
    )]
    pub mqtt_discovery_prefix: String,
    /// Topic root for Pinson's state, commands and status.
    #[arg(long, env = "MUSIC_MQTT_ROOT", default_value = "pinson")]
    pub mqtt_root: String,
}

/// Resolved MQTT settings, present only when `--mqtt-broker` is set.
#[derive(Clone)]
pub struct MqttConfig {
    pub host: String,
    pub port: u16,
    pub user: Option<String>,
    pub password: Option<String>,
    pub discovery_prefix: String,
    pub root: String,
}

impl fmt::Debug for MqttConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MqttConfig")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("user", &self.user)
            .field("password", &self.password.as_ref().map(|_| "***"))
            .field("discovery_prefix", &self.discovery_prefix)
            .field("root", &self.root)
            .finish()
    }
}

impl Config {
    /// The MQTT settings, or `None` when no broker is configured.
    pub fn mqtt(&self) -> anyhow::Result<Option<MqttConfig>> {
        let Some(broker) = self.mqtt_broker.as_deref() else {
            return Ok(None);
        };
        let (host, port) = match broker.rsplit_once(':') {
            Some((host, port)) => {
                let port = port
                    .parse::<u16>()
                    .with_context(|| format!("invalid MQTT broker port in {broker:?}"))?;
                (host, port)
            }
            None => (broker, 1883),
        };
        anyhow::ensure!(!host.is_empty(), "MQTT broker host is empty");
        Ok(Some(MqttConfig {
            host: host.to_string(),
            port,
            user: self.mqtt_user.clone(),
            password: self.mqtt_password.clone(),
            discovery_prefix: self.mqtt_discovery_prefix.clone(),
            root: self.mqtt_root.clone(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mqtt_password_is_hidden_in_help() {
        use clap::CommandFactory;
        let cmd = Config::command();
        let arg = cmd
            .get_arguments()
            .find(|a| a.get_id() == "mqtt_password")
            .expect("mqtt_password arg");
        assert!(arg.is_hide_env_values_set());
    }

    #[test]
    fn config_defaults() {
        let cfg = Config::try_parse_from(["pinson"]).unwrap();
        assert_eq!(cfg.bind.to_string(), "0.0.0.0");
        assert_eq!(cfg.port, 8082);
        assert_eq!(cfg.yt_dlp, PathBuf::from("yt-dlp"));
        assert_eq!(cfg.engine, EngineKind::Nidoo);
        assert_eq!(cfg.nidoo_sound, "192.168.1.91:4953");
        assert!(cfg.nidoo_control.is_none());
        assert_eq!(cfg.nidoo_latency_ms, 750);
        assert!(cfg.cookies_from_browser.is_none());
        assert_eq!(cfg.db, PathBuf::from("pinson.db"));
        assert_eq!(cfg.favorites_file, PathBuf::from("favorites.json"));
        assert_eq!(cfg.playlists_file, PathBuf::from("playlists.json"));
    }

    #[test]
    fn config_flags() {
        let cfg = Config::try_parse_from([
            "pinson",
            "--port",
            "9000",
            "--cookies-from-browser",
            "firefox",
            "--engine",
            "log",
            "--nidoo-latency-ms",
            "500",
            "--db",
            "/var/lib/music/pinson.db",
            "--favorites-file",
            "/var/lib/music/favs.json",
        ])
        .unwrap();
        assert_eq!(cfg.port, 9000);
        assert_eq!(cfg.cookies_from_browser.as_deref(), Some("firefox"));
        assert_eq!(cfg.engine, EngineKind::Log);
        assert_eq!(cfg.nidoo_latency_ms, 500);
        assert_eq!(cfg.db, PathBuf::from("/var/lib/music/pinson.db"));
        assert_eq!(
            cfg.favorites_file,
            PathBuf::from("/var/lib/music/favs.json")
        );
    }

    #[test]
    fn mqtt_off_by_default() {
        let cfg = Config::try_parse_from(["pinson"]).unwrap();
        assert!(cfg.mqtt().unwrap().is_none());
        assert_eq!(cfg.mqtt_discovery_prefix, "homeassistant");
        assert_eq!(cfg.mqtt_root, "pinson");
    }

    #[test]
    fn mqtt_broker_with_port() {
        let cfg = Config::try_parse_from([
            "pinson",
            "--mqtt-broker",
            "192.168.1.10:1884",
            "--mqtt-user",
            "u",
            "--mqtt-password",
            "p",
            "--mqtt-root",
            "music",
        ])
        .unwrap();
        let mqtt = cfg.mqtt().unwrap().expect("MQTT is on");
        assert_eq!(mqtt.host, "192.168.1.10");
        assert_eq!(mqtt.port, 1884);
        assert_eq!(mqtt.user.as_deref(), Some("u"));
        assert_eq!(mqtt.password.as_deref(), Some("p"));
        assert_eq!(mqtt.root, "music");
    }

    #[test]
    fn mqtt_broker_default_port() {
        let cfg = Config::try_parse_from(["pinson", "--mqtt-broker", "broker.lan"]).unwrap();
        let mqtt = cfg.mqtt().unwrap().expect("MQTT is on");
        assert_eq!(mqtt.host, "broker.lan");
        assert_eq!(mqtt.port, 1883);
    }

    #[test]
    fn mqtt_bad_port_is_an_error() {
        let cfg = Config::try_parse_from(["pinson", "--mqtt-broker", "host:abc"]).unwrap();
        assert!(cfg.mqtt().is_err());
    }

    #[test]
    fn mqtt_debug_hides_password() {
        let cfg = Config::try_parse_from([
            "pinson",
            "--mqtt-broker",
            "broker.lan",
            "--mqtt-password",
            "p",
        ])
        .unwrap();
        let mqtt = cfg.mqtt().unwrap().expect("MQTT is on");
        let debug = format!("{mqtt:?}");
        assert!(debug.contains("***"), "{debug}");
        assert!(!debug.contains(r#"password: Some("p")"#), "{debug}");
    }
}
