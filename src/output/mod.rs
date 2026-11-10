//! Audio outputs: what Pinson can play on, and how to build one.

pub mod fake;
pub mod manager;

pub use manager::{CurrentOutput, OUTPUT_SETTING, OutputView, Outputs, SetOutputError};

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, watch};

use crate::engine::log::LoggingEngine;
use crate::engine::nidoo::{NidooSoundConfig, NidooSoundEngine};
use crate::engine::{AudioEngine, EngineEvent};
use crate::speakers::{NidooSpeakers, SpeakerControl, control_url, player_url};

/// The kinds of output Pinson can play on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputKind {
    /// Speakers through nidoo-sound (paced PCM over TCP).
    Nidoo,
    /// Log engine calls only (no sound).
    Log,
}

/// A kind of output plus the address it needs, if any.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputChoice {
    pub kind: OutputKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
}

/// A built output as shown to clients.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OutputInfo {
    pub id: String,
    pub kind: OutputKind,
    pub name: String,
    pub address: Option<String>,
    pub hint: Option<String>,
}

/// An output a client may pick.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OutputOption {
    pub kind: OutputKind,
    pub name: String,
    pub needs_address: bool,
    pub default_address: Option<String>,
}

/// A built output: its engine and the extras it offers.
pub struct Output {
    pub engine: Box<dyn AudioEngine>,
    pub info: OutputInfo,
    pub speakers: Option<Arc<dyn SpeakerControl>>,
    pub link: Option<watch::Receiver<Option<bool>>>,
    /// Sendspin WebSocket URL for browsers that play the sound themselves.
    pub browser_player: Option<String>,
}

/// Builds outputs from choices.
pub trait OutputFactory: Send + Sync + 'static {
    /// Fills defaults and validates; `Err` is the message for a 400.
    fn resolve(&self, choice: OutputChoice) -> Result<OutputChoice, String>;
    /// Builds a resolved choice; can't fail.
    fn build(&self, choice: &OutputChoice, events: mpsc::UnboundedSender<EngineEvent>) -> Output;
    /// The outputs a client may pick.
    fn available(&self) -> Vec<OutputOption>;
}

pub(crate) const NIDOO_HINT: &str = "Approve speakers in Nidoo.";

/// Checks `host:port`: a plain host name, IPv4 address or `[IPv6]`, and a port 1 to 65535.
pub fn validate_host_port(s: &str) -> Result<(), String> {
    let bad = || format!("expected host:port, got {s:?}");
    let (host, port) = s.rsplit_once(':').ok_or_else(bad)?;
    let host_ok = if let Some(inner) = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
        !inner.is_empty()
            && inner
                .chars()
                .all(|c| c.is_ascii_hexdigit() || c == ':' || c == '.')
    } else {
        !host.is_empty()
            && host
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
    };
    if !host_ok {
        return Err(bad());
    }
    match port.parse::<u16>() {
        Ok(p) if p > 0 => Ok(()),
        _ => Err(bad()),
    }
}

/// The real outputs: nidoo-sound and a log-only engine.
pub struct DefaultOutputs {
    pub nidoo_sound: String,
    pub nidoo_control: Option<String>,
    pub nidoo_player_url: Option<String>,
    pub nidoo_latency: Duration,
}

impl DefaultOutputs {
    /// nidoo-sound's HTTP API for `address`; the explicit URL applies only to the configured address.
    pub(crate) fn nidoo_control_base(&self, address: &str) -> String {
        let explicit = (address == self.nidoo_sound)
            .then_some(self.nidoo_control.as_deref())
            .flatten();
        control_url(address, explicit)
    }

    fn nidoo_player(&self, address: &str) -> String {
        let explicit = (address == self.nidoo_sound)
            .then_some(self.nidoo_player_url.as_deref())
            .flatten();
        player_url(address, explicit)
    }
}

impl OutputFactory for DefaultOutputs {
    fn resolve(&self, choice: OutputChoice) -> Result<OutputChoice, String> {
        match choice.kind {
            OutputKind::Nidoo => {
                let address = choice.address.unwrap_or_else(|| self.nidoo_sound.clone());
                validate_host_port(&address)?;
                Ok(OutputChoice {
                    kind: OutputKind::Nidoo,
                    address: Some(address),
                })
            }
            OutputKind::Log => Ok(OutputChoice {
                kind: OutputKind::Log,
                address: None,
            }),
        }
    }

    fn build(&self, choice: &OutputChoice, events: mpsc::UnboundedSender<EngineEvent>) -> Output {
        match choice.kind {
            OutputKind::Nidoo => {
                let address = choice
                    .address
                    .clone()
                    .unwrap_or_else(|| self.nidoo_sound.clone());
                let engine = NidooSoundEngine::spawn(
                    NidooSoundConfig {
                        addr: address.clone(),
                        max_ahead: Duration::from_millis(200),
                        latency: self.nidoo_latency,
                    },
                    events,
                );
                let link = engine.link();
                let base = self.nidoo_control_base(&address);
                tracing::info!("speaker control: {base}");
                let speakers: Arc<dyn SpeakerControl> =
                    Arc::new(NidooSpeakers::new(base, Duration::from_secs(2)));
                let browser_player = self.nidoo_player(&address);
                tracing::info!("browser player: {browser_player}");
                Output {
                    engine: Box::new(engine),
                    info: OutputInfo {
                        id: format!("nidoo:{address}"),
                        kind: OutputKind::Nidoo,
                        name: "Nidoo".into(),
                        address: Some(address),
                        hint: Some(NIDOO_HINT.into()),
                    },
                    speakers: Some(speakers),
                    link: Some(link),
                    browser_player: Some(browser_player),
                }
            }
            OutputKind::Log => {
                drop(events);
                Output {
                    engine: Box::new(LoggingEngine::default()),
                    info: OutputInfo {
                        id: "log".into(),
                        kind: OutputKind::Log,
                        name: "Log only (no sound)".into(),
                        address: None,
                        hint: None,
                    },
                    speakers: None,
                    link: None,
                    browser_player: None,
                }
            }
        }
    }

    fn available(&self) -> Vec<OutputOption> {
        vec![OutputOption {
            kind: OutputKind::Nidoo,
            name: "Nidoo".into(),
            needs_address: true,
            default_address: Some(self.nidoo_sound.clone()),
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outputs() -> DefaultOutputs {
        DefaultOutputs {
            nidoo_sound: "192.168.1.91:4953".into(),
            nidoo_control: Some("http://h:9".into()),
            nidoo_player_url: Some("ws://explicit:1/x".into()),
            nidoo_latency: Duration::from_millis(750),
        }
    }

    #[test]
    fn validate_host_port_accepts_and_rejects() {
        for ok in ["192.168.1.91:4953", "nidoo.local:4953", "[::1]:4953"] {
            assert!(validate_host_port(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "host",
            ":4953",
            "host:0",
            "host:70000",
            "http://h:1",
            "h st:1",
        ] {
            assert!(validate_host_port(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn nidoo_resolves_default_address() {
        let choice = OutputChoice {
            kind: OutputKind::Nidoo,
            address: None,
        };
        let resolved = outputs().resolve(choice).unwrap();
        assert_eq!(resolved.address.as_deref(), Some("192.168.1.91:4953"));
    }

    #[tokio::test]
    async fn nidoo_info_and_control_url() {
        let o = outputs();
        let (tx, _rx) = mpsc::unbounded_channel();
        let choice = o
            .resolve(OutputChoice {
                kind: OutputKind::Nidoo,
                address: None,
            })
            .unwrap();
        let out = o.build(&choice, tx);
        assert_eq!(out.info.id, "nidoo:192.168.1.91:4953");
        assert_eq!(out.info.name, "Nidoo");
        assert_eq!(out.info.hint.as_deref(), Some("Approve speakers in Nidoo."));
        assert!(out.speakers.is_some());
        assert!(out.link.is_some());
        assert_eq!(o.nidoo_control_base("192.168.1.91:4953"), "http://h:9");
        assert_eq!(
            o.nidoo_control_base("10.0.0.2:4953"),
            "http://10.0.0.2:4954"
        );
    }

    #[tokio::test]
    async fn nidoo_browser_player_url_rule() {
        let o = outputs();
        let (tx, _rx) = mpsc::unbounded_channel();
        let default = OutputChoice {
            kind: OutputKind::Nidoo,
            address: Some("192.168.1.91:4953".into()),
        };
        let out = o.build(&default, tx.clone());
        assert_eq!(out.browser_player.as_deref(), Some("ws://explicit:1/x"));
        let other = OutputChoice {
            kind: OutputKind::Nidoo,
            address: Some("10.0.0.2:4953".into()),
        };
        let out = o.build(&other, tx);
        assert_eq!(
            out.browser_player.as_deref(),
            Some("ws://10.0.0.2:8927/sendspin")
        );
    }

    #[tokio::test]
    async fn log_output_has_no_extras() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let o = outputs();
        let out = o.build(
            &OutputChoice {
                kind: OutputKind::Log,
                address: None,
            },
            tx,
        );
        assert_eq!(out.info.id, "log");
        assert_eq!(out.info.name, "Log only (no sound)");
        assert!(out.info.address.is_none() && out.info.hint.is_none());
        assert!(out.speakers.is_none() && out.link.is_none() && out.browser_player.is_none());
        let kinds: Vec<_> = o.available().iter().map(|o| o.kind).collect();
        assert_eq!(kinds, [OutputKind::Nidoo]);
    }

    #[test]
    fn output_choice_json_round_trips() {
        let choice = OutputChoice {
            kind: OutputKind::Nidoo,
            address: Some("h:1".into()),
        };
        let json = r#"{"kind":"nidoo","address":"h:1"}"#;
        assert_eq!(serde_json::to_string(&choice).unwrap(), json);
        assert_eq!(serde_json::from_str::<OutputChoice>(json).unwrap(), choice);
        assert!(serde_json::from_str::<OutputChoice>(r#"{"kind":"airplay"}"#).is_err());
    }
}
