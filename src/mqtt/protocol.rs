//! MQTT topics, the Home Assistant discovery payload and command parsing. Pure: no I/O.

use serde::Deserialize;
use serde_json::{Value, json};

use crate::config::MqttConfig;

pub const ONLINE: &str = "online";
pub const OFFLINE: &str = "offline";

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
        "supported_features": ["play", "pause", "next", "previous", "play_media"],
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
}

#[derive(Deserialize)]
struct Raw {
    command: String,
    query: Option<String>,
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
                "supported_features": ["play", "pause", "next", "previous", "play_media"],
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
