//! MQTT topics, the Home Assistant discovery payload and command parsing. Pure: no I/O.

use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::config::MqttConfig;

pub const ONLINE: &str = "online";
pub const OFFLINE: &str = "offline";
/// The `playlist` id that stands for the Favorites.
pub const FAVORITES_ID: &str = "favorites";
/// The longest `fade_in_s` honoured; longer asks are clamped.
pub const MAX_FADE_IN_S: u64 = 600;

/// The four topics Pinson uses. `node_id` is the topic root.
#[derive(Clone, Debug, PartialEq)]
pub struct Topics {
    pub discovery: String,
    pub state: String,
    pub command: String,
    pub availability: String,
}

impl Topics {
    pub fn new(cfg: &MqttConfig) -> Self {
        let root = &cfg.root;
        Self {
            discovery: format!("{}/media_player/{root}/player/config", cfg.discovery_prefix),
            state: format!("{root}/player/state"),
            command: format!("{root}/player/set"),
            availability: format!("{root}/status"),
        }
    }
}

/// The retained discovery payload (contract §1).
pub fn discovery_payload(cfg: &MqttConfig, topics: &Topics) -> Value {
    let node_id = &cfg.root;
    json!({
        "name": "Music",
        "unique_id": format!("{node_id}_player"),
        "state_topic": topics.state,
        "command_topic": topics.command,
        "availability_topic": topics.availability,
        "supported_features": ["play", "pause", "next", "previous", "play_media", "play_playlist"],
        "device": {
            "identifiers": [node_id],
            "name": "Pinson",
            "model": "Music server",
            "manufacturer": "Gekkotron"
        }
    })
}

#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    Play,
    Pause,
    Next,
    Previous,
    PlayMedia(String),
    PlayPlaylist(PlayPlaylist),
}

/// The `play_playlist` command (wake-up alarms).
#[derive(Clone, Debug, PartialEq)]
pub struct PlayPlaylist {
    /// `favorites` or a Pinson playlist id.
    pub playlist: String,
    pub shuffle: bool,
    /// An id from the state's `source_list`; `None` keeps the current output.
    pub source: Option<String>,
    pub fade_in: Duration,
}

#[derive(Deserialize)]
struct Raw {
    command: String,
    query: Option<String>,
    playlist: Option<String>,
    shuffle: Option<bool>,
    source: Option<String>,
    fade_in_s: Option<u64>,
}

/// Parses a command payload. The `Err` text is meant for the log.
pub fn parse_command(payload: &[u8]) -> Result<Command, String> {
    let raw: Raw = serde_json::from_slice(payload).map_err(|e| e.to_string())?;
    match raw.command.as_str() {
        "play" => Ok(Command::Play),
        "pause" => Ok(Command::Pause),
        "next" => Ok(Command::Next),
        "previous" => Ok(Command::Previous),
        "play_media" => match raw.query.as_deref().map(str::trim) {
            Some(q) if !q.is_empty() => Ok(Command::PlayMedia(q.to_string())),
            _ => Err("play_media needs a non-blank \"query\"".to_string()),
        },
        "play_playlist" => match raw.playlist.as_deref().map(str::trim) {
            Some(id) if !id.is_empty() => Ok(Command::PlayPlaylist(PlayPlaylist {
                playlist: id.to_string(),
                shuffle: raw.shuffle.unwrap_or(true),
                source: raw
                    .source
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty()),
                fade_in: Duration::from_secs(raw.fade_in_s.unwrap_or(0).min(MAX_FADE_IN_S)),
            })),
            _ => Err("play_playlist needs a non-blank \"playlist\"".to_string()),
        },
        other => Err(format!("unknown command {other:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cfg(root: &str, prefix: &str) -> MqttConfig {
        MqttConfig {
            host: "localhost".into(),
            port: 1883,
            user: None,
            password: None,
            discovery_prefix: prefix.into(),
            root: root.into(),
        }
    }

    #[test]
    fn topics_default() {
        let t = Topics::new(&cfg("pinson", "homeassistant"));
        assert_eq!(
            t.discovery,
            "homeassistant/media_player/pinson/player/config"
        );
        assert_eq!(t.state, "pinson/player/state");
        assert_eq!(t.command, "pinson/player/set");
        assert_eq!(t.availability, "pinson/status");
    }

    #[test]
    fn topics_custom_root() {
        let t = Topics::new(&cfg("music", "ha"));
        assert_eq!(t.discovery, "ha/media_player/music/player/config");
        assert_eq!(t.state, "music/player/state");
        assert_eq!(t.command, "music/player/set");
        assert_eq!(t.availability, "music/status");
    }

    #[test]
    fn discovery_matches_contract() {
        let c = cfg("pinson", "homeassistant");
        let t = Topics::new(&c);
        assert_eq!(
            discovery_payload(&c, &t),
            json!({
                "name": "Music",
                "unique_id": "pinson_player",
                "state_topic": "pinson/player/state",
                "command_topic": "pinson/player/set",
                "availability_topic": "pinson/status",
                "supported_features": ["play", "pause", "next", "previous", "play_media", "play_playlist"],
                "device": {
                    "identifiers": ["pinson"],
                    "name": "Pinson",
                    "model": "Music server",
                    "manufacturer": "Gekkotron"
                }
            })
        );
    }

    #[test]
    fn parse_each_command() {
        assert_eq!(parse_command(br#"{"command":"play"}"#), Ok(Command::Play));
        assert_eq!(parse_command(br#"{"command":"pause"}"#), Ok(Command::Pause));
        assert_eq!(parse_command(br#"{"command":"next"}"#), Ok(Command::Next));
        assert_eq!(
            parse_command(br#"{"command":"previous"}"#),
            Ok(Command::Previous)
        );
        assert_eq!(
            parse_command(br#"{"command":"play_media","query":" daft punk "}"#),
            Ok(Command::PlayMedia("daft punk".into()))
        );
    }

    #[test]
    fn play_playlist_defaults() {
        assert_eq!(
            parse_command(br#"{"command":"play_playlist","playlist":"favorites"}"#),
            Ok(Command::PlayPlaylist(PlayPlaylist {
                playlist: "favorites".into(),
                shuffle: true,
                source: None,
                fade_in: Duration::ZERO,
            }))
        );
    }

    #[test]
    fn play_playlist_full() {
        assert_eq!(
            parse_command(
                br#"{"command":"play_playlist","playlist":"abc123","shuffle":false,"source":"nidoo","fade_in_s":60}"#
            ),
            Ok(Command::PlayPlaylist(PlayPlaylist {
                playlist: "abc123".into(),
                shuffle: false,
                source: Some("nidoo".into()),
                fade_in: Duration::from_secs(60),
            }))
        );
        // A blank source counts as none.
        let Ok(Command::PlayPlaylist(p)) =
            parse_command(br#"{"command":"play_playlist","playlist":"x","source":" "}"#)
        else {
            panic!("should parse");
        };
        assert_eq!(p.source, None);
    }

    #[test]
    fn play_playlist_clamps_fade() {
        let Ok(Command::PlayPlaylist(p)) =
            parse_command(br#"{"command":"play_playlist","playlist":"x","fade_in_s":900}"#)
        else {
            panic!("should parse");
        };
        assert_eq!(p.fade_in, Duration::from_secs(600));
    }

    #[test]
    fn play_playlist_needs_playlist() {
        for bad in [
            &br#"{"command":"play_playlist"}"#[..],
            br#"{"command":"play_playlist","playlist":"  "}"#,
        ] {
            assert!(parse_command(bad).is_err());
        }
    }

    #[test]
    fn parse_rejects() {
        for bad in [
            &b"not json"[..],
            br#"{}"#,
            br#"{"command":"volume_up"}"#,
            br#"{"command":"play_media"}"#,
            br#"{"command":"play_media","query":"  "}"#,
        ] {
            assert!(
                parse_command(bad).is_err(),
                "expected Err for {:?}",
                String::from_utf8_lossy(bad)
            );
        }
    }
}
