//! Command-line flags; every flag can also be set with a `MUSIC_*` env var.

use std::fmt;
use std::net::IpAddr;
use std::path::PathBuf;

use anyhow::Context;
use clap::Parser;

use crate::output::{OutputChoice, OutputKind};

#[derive(Clone, Debug, Parser)]
#[command(
    name = "pinson",
    version,
    about = "Pinson, the house music server: YouTube Music on your speakers"
)]
pub struct Config {
    /// Address to listen on.
    #[arg(long, env = "MUSIC_BIND", default_value = "0.0.0.0")]
    pub bind: IpAddr,
    /// Port for the web page and the API.
    #[arg(long, env = "MUSIC_PORT", default_value_t = 8082)]
    pub port: u16,
    /// Picore base URL for animated Apple Music covers. Unset: animated covers are off.
    #[arg(long, env = "MUSIC_PICORE_API_URL", value_parser = parse_picore_url)]
    pub picore_api_url: Option<String>,
    /// yt-dlp executable.
    #[arg(long, env = "MUSIC_YT_DLP", default_value = "yt-dlp")]
    pub yt_dlp: PathBuf,
    /// ffmpeg executable.
    #[arg(long, env = "MUSIC_FFMPEG", default_value = "ffmpeg")]
    pub ffmpeg: PathBuf,
    /// YouTube clients yt-dlp asks for the stream, comma-separated (its `player_client`).
    /// `visionos` skips the default clients' wait for a pre-roll ad. Empty: yt-dlp's default.
    /// A song that fails with these is opened again with yt-dlp's default.
    #[arg(long, env = "MUSIC_YT_CLIENTS", default_value = "visionos")]
    pub yt_clients: String,
    /// Target loudness in LUFS (-30 to -5), or `off`. Each song is measured as it loads and
    /// played at about this loudness.
    #[arg(long, env = "MUSIC_LOUDNESS", value_parser = parse_loudness, default_value = "-14", allow_negative_numbers = true)]
    pub loudness: LoudnessTarget,
    /// Directory for offline favorites. Defaults to `offline/` next to the database.
    #[arg(long, env = "MUSIC_OFFLINE_DIR")]
    pub offline_dir: Option<PathBuf>,
    /// Most gigabytes of favorites kept on the server's disk when "Keep favorites on the
    /// server" is on; `0` means no limit.
    #[arg(long, env = "MUSIC_OFFLINE_MAX_GB", default_value_t = 20)]
    pub offline_max_gb: u64,
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
    /// Startup output: nidoo, snapcast, stream, local, chromecast, upnp, airplay or log. Unset: choose one in the web app.
    #[arg(long, env = "MUSIC_ENGINE", value_enum)]
    pub engine: Option<OutputKind>,
    /// nidoo-sound PCM input (host:port).
    #[arg(long, env = "MUSIC_NIDOO_SOUND", default_value = "192.168.1.91:4953")]
    pub nidoo_sound: String,
    /// nidoo-sound HTTP API (http://host:port); default: MUSIC_NIDOO_SOUND's host on port 4954.
    #[arg(long, env = "MUSIC_NIDOO_CONTROL")]
    pub nidoo_control: Option<String>,
    /// nidoo-sound's Sendspin player WebSocket (ws://host:port/path) for browsers that play sound; default: MUSIC_NIDOO_SOUND's host on port 8927, path /sendspin.
    #[arg(long, env = "MUSIC_NIDOO_PLAYER_URL")]
    pub nidoo_player_url: Option<String>,
    /// Delay from a written byte to the speakers, in ms (nidoo-sound's buffer + lead).
    #[arg(long, env = "MUSIC_NIDOO_LATENCY_MS", default_value_t = 750)]
    pub nidoo_latency_ms: u64,
    /// snapserver tcp source Pinson writes PCM to (host:port); default: a snapserver found by mDNS, else 127.0.0.1:4953.
    #[arg(long, env = "MUSIC_SNAPCAST")]
    pub snapcast: Option<String>,
    /// snapserver JSON-RPC control port (host:port); default: MUSIC_SNAPCAST's host on port 1705.
    #[arg(long, env = "MUSIC_SNAPCAST_CONTROL")]
    pub snapcast_control: Option<String>,
    /// Delay from a written byte to the speakers, in ms (snapserver's 1000 ms buffer + lead).
    #[arg(long, env = "MUSIC_SNAPCAST_LATENCY_MS", default_value_t = 1200)]
    pub snapcast_latency_ms: u64,
    /// Delay from a written byte to the listener's speakers on Stream URL, in ms (player buffers).
    #[arg(long, env = "MUSIC_STREAM_LATENCY_MS", default_value_t = 1000)]
    pub stream_latency_ms: u64,
    /// Chromecast to play on: its name, its Cast id or `host:port`. Unset: pick one in the web app.
    #[arg(long, env = "MUSIC_CHROMECAST")]
    pub chromecast: Option<String>,
    /// UPnP/DLNA renderer to play on: its name, its UDN or its description URL. Unset: pick one in the web app.
    #[arg(long, env = "MUSIC_UPNP")]
    pub upnp: Option<String>,
    /// AirPlay 1 (RAOP) speaker to play on: its name, its id or `host[:port]` (port 5000 by default). Unset: pick one in the web app.
    #[arg(long, env = "MUSIC_AIRPLAY")]
    pub airplay: Option<String>,
    /// Delay from a written byte to an AirPlay speaker's output, in ms (500 to 5000).
    #[arg(
        long,
        env = "MUSIC_AIRPLAY_LATENCY_MS",
        default_value_t = 2000,
        value_parser = clap::value_parser!(u64).range(500..=5000)
    )]
    pub airplay_latency_ms: u64,
    /// Where Chromecasts and UPnP renderers reach Pinson (http:// or https://), e.g. behind a proxy. Default: the address facing the device.
    #[arg(long, env = "MUSIC_PUBLIC_URL", value_parser = parse_public_url)]
    pub public_url: Option<String>,
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

/// The loudness target in LUFS; `None` is off. An alias so that clap sees one
/// parsed value rather than an optional flag.
pub type LoudnessTarget = Option<f32>;

fn parse_loudness(value: &str) -> Result<LoudnessTarget, String> {
    let value = value.trim();
    if value.eq_ignore_ascii_case("off") {
        return Ok(None);
    }
    match value.parse::<f32>() {
        Ok(lufs) if (-30.0..=-5.0).contains(&lufs) => Ok(Some(lufs)),
        _ => Err("expected `off` or a loudness from -30 to -5 LUFS".into()),
    }
}

fn parse_picore_url(value: &str) -> Result<String, String> {
    let url = reqwest::Url::parse(value.trim())
        .map_err(|_| "expected an HTTP(S) Picore base URL".to_owned())?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(
            "expected an HTTP(S) Picore base URL without credentials, query or fragment".into(),
        );
    }
    Ok(url.as_str().trim_end_matches('/').to_owned())
}

fn parse_public_url(s: &str) -> Result<String, String> {
    let s = s.trim();
    let ok = ["http://", "https://"]
        .iter()
        .any(|p| s.strip_prefix(p).is_some_and(|rest| !rest.is_empty()));
    if ok {
        Ok(s.to_string())
    } else {
        Err(format!("expected an http:// or https:// URL, got {s:?}"))
    }
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
    /// The cache directory, resolved from the database location when unset.
    pub fn offline_dir(&self) -> PathBuf {
        self.offline_dir
            .clone()
            .unwrap_or_else(|| self.db.with_file_name("offline"))
    }

    /// The output chosen with `--engine`, if any.
    pub fn startup_output(&self) -> Option<OutputChoice> {
        self.engine.map(|kind| OutputChoice {
            kind,
            address: match kind {
                OutputKind::Chromecast => self.chromecast.clone(),
                OutputKind::Upnp => self.upnp.clone(),
                OutputKind::Airplay => self.airplay.clone(),
                _ => None,
            },
            name: None,
        })
    }

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
        assert!(cfg.picore_api_url.is_none());
        assert_eq!(cfg.yt_dlp, PathBuf::from("yt-dlp"));
        assert_eq!(cfg.engine, None);
        assert_eq!(cfg.startup_output(), None);
        assert_eq!(cfg.nidoo_sound, "192.168.1.91:4953");
        assert!(cfg.nidoo_control.is_none());
        assert!(cfg.nidoo_player_url.is_none());
        assert_eq!(cfg.nidoo_latency_ms, 750);
        assert_eq!(cfg.snapcast, None);
        assert!(cfg.snapcast_control.is_none());
        assert_eq!(cfg.snapcast_latency_ms, 1200);
        assert_eq!(cfg.stream_latency_ms, 1000);
        assert!(cfg.cookies_from_browser.is_none());
        assert_eq!(cfg.yt_clients, "visionos");
        assert_eq!(cfg.loudness, Some(-14.0));
        assert_eq!(cfg.offline_max_gb, 20);
        assert_eq!(cfg.db, PathBuf::from("pinson.db"));
        assert_eq!(cfg.offline_dir(), PathBuf::from("offline"));
        assert_eq!(cfg.favorites_file, PathBuf::from("favorites.json"));
        assert_eq!(cfg.playlists_file, PathBuf::from("playlists.json"));
    }

    #[test]
    fn offline_dir_follows_the_database_unless_set_explicitly() {
        let cfg = Config::try_parse_from(["pinson", "--db", "data/pinson.db"]).unwrap();
        assert_eq!(cfg.offline_dir(), PathBuf::from("data/offline"));

        let cfg = Config::try_parse_from([
            "pinson",
            "--db",
            "data/pinson.db",
            "--offline-dir",
            "external music/cache",
        ])
        .unwrap();
        assert_eq!(cfg.offline_dir(), PathBuf::from("external music/cache"));
    }

    #[test]
    fn loudness_is_off_or_a_target_in_range() {
        let parse = |v: &str| Config::try_parse_from(["pinson", "--loudness", v]);
        assert_eq!(parse("off").unwrap().loudness, None);
        assert_eq!(parse("OFF").unwrap().loudness, None);
        assert_eq!(parse("-18").unwrap().loudness, Some(-18.0));
        assert_eq!(parse("-23.5").unwrap().loudness, Some(-23.5));
        for value in ["-31", "-4", "0", "loud", ""] {
            assert!(parse(value).is_err(), "{value}");
        }
    }

    #[test]
    fn picore_url_is_optional_and_validated() {
        let cfg =
            Config::try_parse_from(["pinson", "--picore-api-url=http://localhost:1234/picore/"])
                .unwrap();
        assert_eq!(
            cfg.picore_api_url.as_deref(),
            Some("http://localhost:1234/picore")
        );
        for value in [
            "",
            "ftp://example.com",
            "http://",
            "https://example.com/?x=1",
            "https://user:secret@example.com",
        ] {
            assert!(
                Config::try_parse_from(["pinson", "--picore-api-url", value]).is_err(),
                "{value}"
            );
        }
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
            "--nidoo-player-url",
            "ws://nidoo.local:8927/sendspin",
            "--db",
            "/var/lib/music/pinson.db",
            "--favorites-file",
            "/var/lib/music/favs.json",
        ])
        .unwrap();
        assert_eq!(cfg.port, 9000);
        assert_eq!(cfg.cookies_from_browser.as_deref(), Some("firefox"));
        assert_eq!(cfg.engine, Some(OutputKind::Log));
        assert_eq!(cfg.nidoo_latency_ms, 500);
        assert_eq!(
            cfg.nidoo_player_url.as_deref(),
            Some("ws://nidoo.local:8927/sendspin")
        );
        assert_eq!(cfg.db, PathBuf::from("/var/lib/music/pinson.db"));
        assert_eq!(
            cfg.favorites_file,
            PathBuf::from("/var/lib/music/favs.json")
        );
    }

    #[test]
    fn engine_flag_sets_startup_output() {
        let cfg = Config::try_parse_from(["pinson", "--engine", "nidoo"]).unwrap();
        assert_eq!(
            cfg.startup_output(),
            Some(OutputChoice {
                kind: OutputKind::Nidoo,
                address: None,
                name: None
            })
        );
    }

    #[test]
    fn chromecast_and_public_url_flags() {
        let cfg = Config::try_parse_from(["pinson"]).unwrap();
        assert!(cfg.chromecast.is_none() && cfg.public_url.is_none());
        let cfg = Config::try_parse_from([
            "pinson",
            "--engine",
            "chromecast",
            "--chromecast",
            "Living Room",
            "--public-url",
            "https://pinson.example/",
        ])
        .unwrap();
        assert_eq!(cfg.chromecast.as_deref(), Some("Living Room"));
        assert_eq!(cfg.public_url.as_deref(), Some("https://pinson.example/"));
        assert_eq!(
            cfg.startup_output(),
            Some(OutputChoice {
                kind: OutputKind::Chromecast,
                address: Some("Living Room".into()),
                name: None
            })
        );
        // Other kinds ignore --chromecast.
        let cfg =
            Config::try_parse_from(["pinson", "--engine", "log", "--chromecast", "x"]).unwrap();
        assert_eq!(cfg.startup_output().unwrap().address, None);
    }

    #[test]
    fn upnp_flag() {
        let cfg = Config::try_parse_from(["pinson"]).unwrap();
        assert!(cfg.upnp.is_none());
        let cfg = Config::try_parse_from(["pinson", "--engine", "upnp", "--upnp", "Living Room"])
            .unwrap();
        assert_eq!(
            cfg.startup_output(),
            Some(OutputChoice {
                kind: OutputKind::Upnp,
                address: Some("Living Room".into()),
                name: None
            })
        );
        let cfg = Config::try_parse_from(["pinson", "--engine", "log", "--upnp", "x"]).unwrap();
        assert_eq!(cfg.startup_output().unwrap().address, None);
    }

    #[test]
    fn airplay_flags() {
        let cfg = Config::try_parse_from(["pinson"]).unwrap();
        assert!(cfg.airplay.is_none());
        assert_eq!(cfg.airplay_latency_ms, 2000);
        let cfg =
            Config::try_parse_from(["pinson", "--engine", "airplay", "--airplay", "Living Room"])
                .unwrap();
        assert_eq!(
            cfg.startup_output(),
            Some(OutputChoice {
                kind: OutputKind::Airplay,
                address: Some("Living Room".into()),
                name: None
            })
        );
        let cfg = Config::try_parse_from(["pinson", "--engine", "log", "--airplay", "x"]).unwrap();
        assert_eq!(cfg.startup_output().unwrap().address, None);
        for bad in ["100", "5001"] {
            assert!(Config::try_parse_from(["pinson", "--airplay-latency-ms", bad]).is_err());
        }
        let cfg = Config::try_parse_from(["pinson", "--airplay-latency-ms", "500"]).unwrap();
        assert_eq!(cfg.airplay_latency_ms, 500);
    }

    #[test]
    fn bad_public_url_fails() {
        for bad in ["pinson.lan:8082", "ftp://x", "http://", ""] {
            assert!(
                Config::try_parse_from(["pinson", "--public-url", bad]).is_err(),
                "{bad}"
            );
        }
        assert!(Config::try_parse_from(["pinson", "--public-url", "http://10.0.0.2:8082"]).is_ok());
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
