//! Audio outputs: what Pinson can play on, and how to build one.

pub mod browser;
pub mod cast;
pub mod discovery;
pub mod fake;
pub mod local;
pub mod manager;
pub mod renderer;
pub mod snapcast;
pub mod ssdp;
pub mod stream_url;
pub mod upnp;

pub use manager::{CurrentOutput, OUTPUT_SETTING, OutputView, Outputs, SetOutputError};

use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, watch};

use self::browser::{BrowserEngine, DeviceHub, Encoder, FfmpegMp3};
use self::cast::session::SessionConfig;
use self::cast::{CAST_PORT, CastEngine, DiscoveredConnector};
use self::discovery::{Discovery, Found, normalize_cast_id};
use self::local::{Gain, LocalDevice, build_local};
use self::renderer::{External, RendererSpeakers, SongStreams};
use self::snapcast::{SnapcastSpeakers, snapcast_control};
use self::stream_url::{FfmpegLive, StreamTap, build_stream};
use crate::engine::log::LoggingEngine;
use crate::engine::paced::{PacedConfig, PacedEngine};
use crate::engine::{AudioEngine, EngineEvent};
use crate::player::PlayerHandle;
use crate::speakers::{NidooSpeakers, SpeakerControl, control_url, player_url};

/// The kinds of output Pinson can play on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputKind {
    /// A browser tab on a device (the MP3 stream plays in its `<audio>`).
    #[value(skip)]
    Browser,
    /// Speakers through nidoo-sound (paced PCM over TCP).
    Nidoo,
    /// A snapserver tcp source (paced PCM over TCP).
    Snapcast,
    /// One continuous MP3 at `/stream.mp3`, for VLC or a browser.
    Stream,
    /// The server's own sound card.
    Local,
    /// A Chromecast (or other Cast receiver) on the network.
    Chromecast,
    /// Log engine calls only (no sound).
    Log,
}

/// A kind of output plus the address it needs, if any.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputChoice {
    pub kind: OutputKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    /// A label for the output, used by browser devices.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
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
    /// One line shown under the name in the picker.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// The output's id, for options that stand for one device (a discovered Chromecast).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// What to send as `address` when picking this option.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
}

/// A built output: its engine and the extras it offers.
pub struct Output {
    pub engine: Box<dyn AudioEngine>,
    pub info: OutputInfo,
    pub speakers: Option<Arc<dyn SpeakerControl>>,
    pub link: Option<watch::Receiver<Option<bool>>>,
    /// Sendspin WebSocket URL for browsers that play the sound themselves.
    pub browser_player: Option<String>,
    /// Where `/stream.mp3` listeners subscribe (Stream URL only).
    pub stream: Option<StreamTap>,
    /// Becomes `true` once the output has finished closing after its engine was dropped
    /// (a Chromecast flushing its STOPs); `None` for outputs that close at once.
    pub closed: Option<watch::Receiver<bool>>,
}

/// Builds outputs from choices.
pub trait OutputFactory: Send + Sync + 'static {
    /// Fills defaults and validates; `Err` is the message for a 400.
    fn resolve(&self, choice: OutputChoice) -> Result<OutputChoice, String>;
    /// Builds a resolved choice; can't fail.
    fn build(&self, choice: &OutputChoice, events: mpsc::UnboundedSender<EngineEvent>) -> Output;
    /// The outputs a client may pick.
    fn available(&self) -> Vec<OutputOption>;
    /// Fires when discovery may have changed `available`.
    fn changed(&self) -> Option<watch::Receiver<Found>> {
        None
    }
    /// Whether a client may pick this kind right now.
    fn pickable(&self, kind: OutputKind) -> bool {
        self.available().iter().any(|o| o.kind == kind)
    }
}

pub(crate) const NIDOO_HINT: &str = "Approve speakers in Nidoo.";
pub(crate) const SNAPCAST_HINT: &str =
    "Add Pinson's tcp source to snapserver.conf (see the README).";

pub(crate) const CAST_HINT: &str =
    "The Cast device fetches the music from Pinson, so keep both on the same network.";

pub(crate) const STREAM_HINT: &str =
    "Open /stream.mp3 in VLC or a browser. The progress bar is approximate.";

pub(crate) const THIS_DEVICE_NOTE: &str = "Plays in this browser tab";
pub(crate) const NIDOO_NOTE: &str = "Speakers through nidoo-sound";
pub(crate) const SNAPCAST_NOTE: &str = "A snapserver tcp source (see the README)";
pub(crate) const STREAM_NOTE: &str = "Open a link in VLC or a browser";
pub(crate) const LOCAL_NOTE: &str = "The server's speakers or headphones";
pub(crate) const NO_LOCAL_DEVICE: &str = "This computer has no usable audio device";

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

const BROWSER_NAME_MAX: usize = 40;

pub(crate) fn valid_device_id(id: &str) -> bool {
    (8..=64).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

/// Checks the device id and tidies the name; shared by the real and fake factories.
fn resolve_browser(choice: OutputChoice) -> Result<OutputChoice, String> {
    let id = choice
        .address
        .filter(|a| valid_device_id(a))
        .ok_or_else(|| "expected a device id".to_string())?;
    let name: String = choice
        .name
        .as_deref()
        .unwrap_or("")
        .trim()
        .chars()
        .take(BROWSER_NAME_MAX)
        .collect();
    let name = name.trim().to_string();
    Ok(OutputChoice {
        kind: OutputKind::Browser,
        address: Some(id),
        name: Some(if name.is_empty() {
            "Browser".into()
        } else {
            name
        }),
    })
}

fn build_browser(
    devices: &DeviceHub,
    encoder: Arc<dyn Encoder>,
    choice: &OutputChoice,
    events: mpsc::UnboundedSender<EngineEvent>,
) -> Output {
    let id = choice.address.clone().unwrap_or_default();
    let link = devices.link(&id);
    let engine = BrowserEngine::new(devices.clone(), id.clone(), encoder, events);
    Output {
        engine: Box::new(engine),
        info: OutputInfo {
            id: format!("browser:{id}"),
            kind: OutputKind::Browser,
            name: choice.name.clone().unwrap_or_else(|| "Browser".into()),
            address: None,
            hint: None,
        },
        speakers: None,
        link: Some(link),
        browser_player: None,
        stream: None,
        closed: None,
    }
}

fn this_device_option() -> OutputOption {
    OutputOption {
        kind: OutputKind::Browser,
        name: "This device".into(),
        needs_address: false,
        default_address: None,
        note: Some(THIS_DEVICE_NOTE.into()),
        id: None,
        address: None,
    }
}

/// The real outputs: nidoo-sound and a log-only engine.
pub struct DefaultOutputs {
    pub devices: DeviceHub,
    pub ffmpeg: PathBuf,
    pub nidoo_sound: String,
    pub nidoo_control: Option<String>,
    pub nidoo_player_url: Option<String>,
    pub nidoo_latency: Duration,
    pub snapcast: Option<String>,
    pub snapcast_control: Option<String>,
    pub snapcast_latency: Duration,
    pub stream_latency: Duration,
    /// The server's sound card, if it has a usable one.
    pub local: Option<LocalDevice>,
    /// Software volume shared by every This computer output.
    pub gain: Arc<Gain>,
    pub discovery: Discovery,
    /// Tells the player when a Cast device is paused or resumed from outside Pinson.
    pub player: PlayerHandle,
    /// The Cast stream a device fetches; shared with the `Services`.
    pub streams: SongStreams,
    /// `--chromecast`: a device name, Cast id or `host:port`.
    pub chromecast: Option<String>,
    /// `--public-url`: where Cast devices reach Pinson; default is the address facing the device.
    pub public_url: Option<String>,
    pub port: u16,
    pub bind: IpAddr,
}

/// A Chromecast's name as typed: 1 to 64 characters.
const CAST_NAME_MAX: usize = 64;
const CAST_LABEL_MAX: usize = 40;
const CAST_EXPECTED: &str = "expected a Chromecast: its name, id or host:port";

/// A host written without a port: an IP address or a dotted name without spaces.
fn bare_host(s: &str) -> bool {
    s.parse::<IpAddr>().is_ok()
        || (s.contains('.')
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')))
}

/// A callback that queues each call and runs `handler` on them one at a time, in the
/// order they were made (a pause then a play on the device reach the player in order).
/// The forwarding task ends when the callback is dropped.
pub(crate) fn in_order<T: Send + 'static>(
    handler: impl Fn(T) -> futures_util::future::BoxFuture<'static, ()> + Send + Sync + 'static,
) -> Arc<dyn Fn(T) + Send + Sync> {
    let (tx, mut rx) = mpsc::unbounded_channel::<T>();
    tokio::spawn(async move {
        while let Some(item) = rx.recv().await {
            handler(item).await;
        }
    });
    Arc::new(move |item| {
        tx.send(item).ok();
    })
}

impl DefaultOutputs {
    /// Validates a Chromecast choice: the address a session connects through and a label.
    fn resolve_chromecast(&self, choice: OutputChoice) -> Result<OutputChoice, String> {
        let raw = choice
            .address
            .as_deref()
            .map(str::trim)
            .filter(|a| !a.is_empty())
            .or(self
                .chromecast
                .as_deref()
                .map(str::trim)
                .filter(|a| !a.is_empty()))
            .ok_or_else(|| CAST_EXPECTED.to_string())?;
        let (address, discovered_name) = if validate_host_port(raw).is_ok() {
            (raw.to_string(), None)
        } else if let Some(d) = self.discovery.cast(raw) {
            (d.id, Some(d.name))
        } else if let Some(id) = normalize_cast_id(raw) {
            (id, None)
        } else if bare_host(raw) {
            (format!("{raw}:{CAST_PORT}"), None)
        } else if raw.chars().count() <= CAST_NAME_MAX {
            (raw.to_string(), None)
        } else {
            return Err(format!(
                "a Chromecast name has at most {CAST_NAME_MAX} characters"
            ));
        };
        let name = discovered_name.unwrap_or_else(|| {
            let label: String = choice
                .name
                .as_deref()
                .unwrap_or("")
                .trim()
                .chars()
                .take(CAST_LABEL_MAX)
                .collect();
            let label = label.trim();
            if label.is_empty() {
                "Chromecast".into()
            } else {
                label.to_string()
            }
        });
        Ok(OutputChoice {
            kind: OutputKind::Chromecast,
            address: Some(address),
            name: Some(name),
        })
    }

    fn build_chromecast(
        &self,
        choice: &OutputChoice,
        events: mpsc::UnboundedSender<EngineEvent>,
    ) -> Output {
        let address = choice.address.clone().unwrap_or_default();
        let name = choice.name.clone().unwrap_or_else(|| "Chromecast".into());
        if self.bind.is_loopback() && self.public_url.is_none() {
            tracing::warn!(
                "Pinson listens on {}: a Chromecast can't reach a loopback address. Set MUSIC_BIND or MUSIC_PUBLIC_URL.",
                self.bind
            );
        }
        let player = self.player.clone();
        let external = in_order(move |e: External| {
            let player = player.clone();
            Box::pin(async move {
                let _ = match e {
                    External::Pause => player.pause().await,
                    External::Resume => player.resume().await,
                };
            })
        });
        let connector = Arc::new(DiscoveredConnector {
            address: address.clone(),
            discovery: self.discovery.clone(),
        });
        let (session, guard) = cast::session::spawn(
            SessionConfig::new(connector, self.public_url.clone(), self.port),
            events.clone(),
            external,
        );
        let link = session.link();
        let closed = session.closed();
        let setter_session = session.clone();
        let speakers: Arc<dyn SpeakerControl> = Arc::new(RendererSpeakers::new(
            session.volume(),
            link.clone(),
            Arc::new(move |level, muted| {
                let s = setter_session.clone();
                Box::pin(async move { s.set_volume(level, muted).await })
            }),
            name.clone(),
            "cast",
            "Can't reach the Chromecast",
        ));
        let encoder = Arc::new(FfmpegMp3 {
            ffmpeg: self.ffmpeg.clone(),
        });
        let engine =
            CastEngine::new(session, encoder, self.streams.clone(), events).with_guard(guard);
        Output {
            engine: Box::new(engine),
            info: OutputInfo {
                id: format!("chromecast:{address}"),
                kind: OutputKind::Chromecast,
                name,
                address: None,
                hint: Some(CAST_HINT.into()),
            },
            speakers: Some(speakers),
            link: Some(link),
            browser_player: None,
            stream: None,
            closed: Some(closed),
        }
    }

    /// The Snapcast address: configured, else a discovered snapserver, else localhost.
    pub(crate) fn snapcast_default(&self) -> String {
        if let Some(a) = &self.snapcast {
            return a.clone();
        }
        match self.discovery.found().snapservers.first() {
            Some(s) => match s.host {
                IpAddr::V4(h) => format!("{h}:4953"),
                IpAddr::V6(h) => format!("[{h}]:4953"),
            },
            None => "127.0.0.1:4953".into(),
        }
    }

    /// nidoo-sound's HTTP API for `address`; the explicit URL applies only to the configured address.
    pub(crate) fn nidoo_control_base(&self, address: &str) -> String {
        let explicit = (address == self.nidoo_sound)
            .then_some(self.nidoo_control.as_deref())
            .flatten();
        control_url(address, explicit)
    }

    /// snapserver's JSON-RPC address; the explicit one applies only to the configured address.
    pub(crate) fn snapcast_control_addr(&self, address: &str) -> String {
        let explicit = (Some(address) == self.snapcast.as_deref())
            .then_some(self.snapcast_control.as_deref())
            .flatten();
        snapcast_control(address, explicit)
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
            OutputKind::Browser => resolve_browser(choice),
            OutputKind::Nidoo => {
                let address = choice.address.unwrap_or_else(|| self.nidoo_sound.clone());
                validate_host_port(&address)?;
                Ok(OutputChoice {
                    kind: OutputKind::Nidoo,
                    address: Some(address),
                    name: None,
                })
            }
            OutputKind::Snapcast => {
                let address = choice.address.unwrap_or_else(|| self.snapcast_default());
                validate_host_port(&address)?;
                Ok(OutputChoice {
                    kind: OutputKind::Snapcast,
                    address: Some(address),
                    name: None,
                })
            }
            OutputKind::Stream => Ok(OutputChoice {
                kind: OutputKind::Stream,
                address: None,
                name: None,
            }),
            OutputKind::Local => {
                if self.local.is_none() {
                    return Err(NO_LOCAL_DEVICE.into());
                }
                Ok(OutputChoice {
                    kind: OutputKind::Local,
                    address: None,
                    name: None,
                })
            }
            OutputKind::Chromecast => self.resolve_chromecast(choice),
            OutputKind::Log => Ok(OutputChoice {
                kind: OutputKind::Log,
                address: None,
                name: None,
            }),
        }
    }

    fn build(&self, choice: &OutputChoice, events: mpsc::UnboundedSender<EngineEvent>) -> Output {
        match choice.kind {
            OutputKind::Browser => {
                let encoder = Arc::new(FfmpegMp3 {
                    ffmpeg: self.ffmpeg.clone(),
                });
                build_browser(&self.devices, encoder, choice, events)
            }
            OutputKind::Nidoo => {
                let address = choice
                    .address
                    .clone()
                    .unwrap_or_else(|| self.nidoo_sound.clone());
                let engine = PacedEngine::spawn(
                    PacedConfig::tcp("nidoo-sound", address.clone(), self.nidoo_latency),
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
                    stream: None,
                    closed: None,
                }
            }
            OutputKind::Snapcast => {
                let address = choice
                    .address
                    .clone()
                    .unwrap_or_else(|| self.snapcast_default());
                let engine = PacedEngine::spawn(
                    PacedConfig {
                        resend_on_resume: false,
                        ..PacedConfig::tcp("snapcast", address.clone(), self.snapcast_latency)
                    },
                    events,
                );
                let link = engine.link();
                let control = self.snapcast_control_addr(&address);
                tracing::info!("speaker control: {control}");
                let speakers: Arc<dyn SpeakerControl> =
                    Arc::new(SnapcastSpeakers::new(control, Duration::from_secs(2)));
                Output {
                    engine: Box::new(engine),
                    info: OutputInfo {
                        id: format!("snapcast:{address}"),
                        kind: OutputKind::Snapcast,
                        name: "Snapcast".into(),
                        address: Some(address),
                        hint: Some(SNAPCAST_HINT.into()),
                    },
                    speakers: Some(speakers),
                    link: Some(link),
                    browser_player: None,
                    stream: None,
                    closed: None,
                }
            }
            OutputKind::Stream => build_stream(
                Arc::new(FfmpegLive {
                    ffmpeg: self.ffmpeg.clone(),
                }),
                self.stream_latency,
                events,
            ),
            OutputKind::Local => {
                let device = self.local.as_ref().expect("resolve checked the device");
                build_local(device, self.gain.clone(), events)
            }
            OutputKind::Chromecast => self.build_chromecast(choice, events),
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
                    stream: None,
                    closed: None,
                }
            }
        }
    }

    fn available(&self) -> Vec<OutputOption> {
        let mut options = vec![
            this_device_option(),
            OutputOption {
                kind: OutputKind::Nidoo,
                name: "Nidoo".into(),
                needs_address: true,
                default_address: Some(self.nidoo_sound.clone()),
                note: Some(NIDOO_NOTE.into()),
                id: None,
                address: None,
            },
            OutputOption {
                kind: OutputKind::Snapcast,
                name: "Snapcast".into(),
                needs_address: true,
                default_address: Some(self.snapcast_default()),
                note: Some(SNAPCAST_NOTE.into()),
                id: None,
                address: None,
            },
            OutputOption {
                kind: OutputKind::Stream,
                name: "Stream URL".into(),
                needs_address: false,
                default_address: None,
                note: Some(STREAM_NOTE.into()),
                id: None,
                address: None,
            },
        ];
        if self.local.is_some() {
            options.push(OutputOption {
                kind: OutputKind::Local,
                name: "This computer".into(),
                needs_address: false,
                default_address: None,
                note: Some(LOCAL_NOTE.into()),
                id: None,
                address: None,
            });
        }
        for d in self.discovery.found().casts {
            options.push(OutputOption {
                kind: OutputKind::Chromecast,
                name: d.name,
                needs_address: false,
                default_address: None,
                note: Some(d.model.unwrap_or_else(|| "Chromecast".into())),
                id: Some(format!("chromecast:{}", d.id)),
                address: Some(d.id),
            });
        }
        options
    }

    fn pickable(&self, kind: OutputKind) -> bool {
        kind == OutputKind::Chromecast || self.available().iter().any(|o| o.kind == kind)
    }

    fn changed(&self) -> Option<watch::Receiver<Found>> {
        Some(self.discovery.subscribe())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn in_order_runs_calls_one_after_another() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let rec = seen.clone();
        let call = in_order(move |i: u64| {
            let rec = rec.clone();
            Box::pin(async move {
                // Earlier calls take longer: spawned separately, they would finish last.
                tokio::time::sleep(Duration::from_millis(100 - i * 10)).await;
                rec.lock().unwrap().push(i);
            })
        });
        for i in 0..10 {
            call(i);
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert_eq!(*seen.lock().unwrap(), (0..10).collect::<Vec<_>>());
    }

    fn player() -> PlayerHandle {
        let (_tx, rx) = mpsc::unbounded_channel();
        crate::player::spawn_player(
            Some(Box::new(crate::engine::fake::FakeEngine::new(
                Duration::ZERO,
            ))),
            rx,
            Arc::new(crate::engine::fake::FakeSource::default()),
            crate::player::Autoplay {
                radio: Arc::new(crate::radio::fake::FakeRadio::default()),
                history: crate::history::History::new(crate::db::Db::in_memory()),
                enabled: false,
            },
        )
    }

    fn hub() -> DeviceHub {
        DeviceHub::new(player(), Duration::from_secs(10))
    }

    fn outputs() -> DefaultOutputs {
        DefaultOutputs {
            devices: hub(),
            ffmpeg: "ffmpeg".into(),
            nidoo_sound: "192.168.1.91:4953".into(),
            nidoo_control: Some("http://h:9".into()),
            nidoo_player_url: Some("ws://explicit:1/x".into()),
            nidoo_latency: Duration::from_millis(750),
            snapcast: Some("127.0.0.1:4953".into()),
            discovery: Discovery::none(),
            snapcast_control: Some("h:7".into()),
            snapcast_latency: Duration::from_millis(1200),
            stream_latency: Duration::from_secs(1),
            local: None,
            gain: Arc::new(Gain::default()),
            player: player(),
            streams: SongStreams::default(),
            chromecast: None,
            public_url: None,
            port: 8082,
            bind: "0.0.0.0".parse().unwrap(),
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

    #[tokio::test]
    async fn nidoo_resolves_default_address() {
        let choice = OutputChoice {
            kind: OutputKind::Nidoo,
            address: None,
            name: None,
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
                name: None,
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
            name: None,
        };
        let out = o.build(&default, tx.clone());
        assert_eq!(out.browser_player.as_deref(), Some("ws://explicit:1/x"));
        let other = OutputChoice {
            kind: OutputKind::Nidoo,
            address: Some("10.0.0.2:4953".into()),
            name: None,
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
                name: None,
            },
            tx,
        );
        assert_eq!(out.info.id, "log");
        assert_eq!(out.info.name, "Log only (no sound)");
        assert!(out.info.address.is_none() && out.info.hint.is_none());
        assert!(out.speakers.is_none() && out.link.is_none() && out.browser_player.is_none());
        assert!(out.stream.is_none());
        let kinds: Vec<_> = o.available().iter().map(|o| o.kind).collect();
        assert_eq!(
            kinds,
            [
                OutputKind::Browser,
                OutputKind::Nidoo,
                OutputKind::Snapcast,
                OutputKind::Stream
            ]
        );
    }

    #[test]
    fn output_choice_json_round_trips() {
        let choice = OutputChoice {
            kind: OutputKind::Nidoo,
            address: Some("h:1".into()),
            name: None,
        };
        let json = r#"{"kind":"nidoo","address":"h:1"}"#;
        assert_eq!(serde_json::to_string(&choice).unwrap(), json);
        assert_eq!(serde_json::from_str::<OutputChoice>(json).unwrap(), choice);
        let browser = OutputChoice {
            kind: OutputKind::Browser,
            address: Some("abcdefgh12".into()),
            name: Some("Pixel".into()),
        };
        let json = r#"{"kind":"browser","address":"abcdefgh12","name":"Pixel"}"#;
        assert_eq!(serde_json::to_string(&browser).unwrap(), json);
        assert_eq!(serde_json::from_str::<OutputChoice>(json).unwrap(), browser);
        assert!(serde_json::from_str::<OutputChoice>(r#"{"kind":"airplay"}"#).is_err());
    }

    fn browser(address: Option<&str>, name: Option<&str>) -> OutputChoice {
        OutputChoice {
            kind: OutputKind::Browser,
            address: address.map(Into::into),
            name: name.map(Into::into),
        }
    }

    #[tokio::test]
    async fn browser_resolves_and_trims_name() {
        let o = outputs();
        let r = o
            .resolve(browser(Some("abcdefgh12"), Some("  Pixel  ")))
            .unwrap();
        assert_eq!(r.name.as_deref(), Some("Pixel"));
        assert_eq!(r.address.as_deref(), Some("abcdefgh12"));
        let r = o.resolve(browser(Some("abcdefgh12"), Some("   "))).unwrap();
        assert_eq!(r.name.as_deref(), Some("Browser"));
        let r = o.resolve(browser(Some("abcdefgh12"), None)).unwrap();
        assert_eq!(r.name.as_deref(), Some("Browser"));
        let long = "x".repeat(60);
        let r = o.resolve(browser(Some("abcdefgh12"), Some(&long))).unwrap();
        assert_eq!(r.name.unwrap().chars().count(), 40);
    }

    #[tokio::test]
    async fn browser_rejects_bad_device_id() {
        let o = outputs();
        for bad in [None, Some("short"), Some("has space1")] {
            assert_eq!(
                o.resolve(browser(bad, None)),
                Err("expected a device id".to_string()),
                "{bad:?}"
            );
        }
    }

    #[tokio::test]
    async fn browser_build_info() {
        let o = outputs();
        let (tx, _rx) = mpsc::unbounded_channel();
        let choice = o
            .resolve(browser(Some("abcdefgh12"), Some("Pixel")))
            .unwrap();
        let out = o.build(&choice, tx);
        assert_eq!(out.info.id, "browser:abcdefgh12");
        assert_eq!(out.info.name, "Pixel");
        assert!(out.info.address.is_none() && out.info.hint.is_none());
        assert!(out.speakers.is_none());
        assert!(out.browser_player.is_none());
        assert!(out.link.is_some());
    }

    #[tokio::test]
    async fn this_device_is_listed_first() {
        assert_eq!(outputs().available()[0].kind, OutputKind::Browser);
        assert_eq!(outputs().available()[0].name, "This device");
    }

    #[tokio::test]
    async fn snapcast_resolves_and_builds() {
        let o = outputs();
        let (tx, _rx) = mpsc::unbounded_channel();
        let choice = o
            .resolve(OutputChoice {
                kind: OutputKind::Snapcast,
                address: None,
                name: None,
            })
            .unwrap();
        assert_eq!(choice.address.as_deref(), Some("127.0.0.1:4953"));
        let out = o.build(&choice, tx);
        assert_eq!(out.info.id, "snapcast:127.0.0.1:4953");
        assert_eq!(out.info.name, "Snapcast");
        assert_eq!(out.info.hint.as_deref(), Some(SNAPCAST_HINT));
        assert!(out.speakers.is_some() && out.link.is_some());
        assert!(out.browser_player.is_none());
        assert_eq!(o.snapcast_control_addr("127.0.0.1:4953"), "h:7");
        assert_eq!(o.snapcast_control_addr("10.0.0.2:4953"), "10.0.0.2:1705");
        let bad = OutputChoice {
            kind: OutputKind::Snapcast,
            address: Some("not an address".into()),
            name: None,
        };
        assert!(o.resolve(bad).is_err());
    }

    #[tokio::test]
    async fn available_lists_snapcast_after_nidoo() {
        let list = outputs().available();
        let snap = &list[2];
        assert_eq!(snap.kind, OutputKind::Snapcast);
        assert!(snap.needs_address);
        assert_eq!(snap.default_address.as_deref(), Some("127.0.0.1:4953"));
        assert!(list.iter().all(|o| o.note.is_some()));
    }

    fn local_outputs(local: Option<LocalDevice>) -> DefaultOutputs {
        DefaultOutputs { local, ..outputs() }
    }

    #[tokio::test]
    async fn resolve_local_without_device_fails() {
        let choice = OutputChoice {
            kind: OutputKind::Local,
            address: None,
            name: None,
        };
        assert_eq!(
            local_outputs(None).resolve(choice.clone()),
            Err("This computer has no usable audio device".to_string())
        );
        let device = LocalDevice {
            name: "Test".into(),
        };
        assert_eq!(
            local_outputs(Some(device)).resolve(choice.clone()),
            Ok(choice)
        );
    }

    #[tokio::test]
    async fn available_hides_local_without_device() {
        let list = local_outputs(None).available();
        assert!(list.iter().all(|o| o.kind != OutputKind::Local));
    }

    #[tokio::test]
    async fn available_lists_local_with_device() {
        let device = LocalDevice {
            name: "Test".into(),
        };
        let list = local_outputs(Some(device)).available();
        let last = list.last().unwrap();
        assert_eq!(last.kind, OutputKind::Local);
        assert_eq!(last.name, "This computer");
        assert_eq!(
            last.note.as_deref(),
            Some("The server's speakers or headphones")
        );
        assert!(!last.needs_address);
    }

    #[tokio::test]
    async fn snapcast_default_prefers_config_then_discovery() {
        use self::discovery::SnapServer;
        let (discovery, tx) = Discovery::manual();
        let mut o = DefaultOutputs {
            snapcast: Some("1.2.3.4:4953".into()),
            discovery,
            ..outputs()
        };
        tx.send(Found {
            casts: vec![],
            snapservers: vec![SnapServer {
                name: "snap".into(),
                host: "10.0.0.7".parse().unwrap(),
            }],
            renderers: vec![],
        })
        .unwrap();
        assert_eq!(o.snapcast_default(), "1.2.3.4:4953");
        o.snapcast = None;
        assert_eq!(o.snapcast_default(), "10.0.0.7:4953");
        assert_eq!(
            o.available()[2].default_address.as_deref(),
            Some("10.0.0.7:4953")
        );
        tx.send(Found::default()).unwrap();
        assert_eq!(o.snapcast_default(), "127.0.0.1:4953");
        assert_eq!(
            o.available()[2].default_address.as_deref(),
            Some("127.0.0.1:4953")
        );
    }

    #[test]
    fn engine_flag_accepts_snapcast() {
        use clap::ValueEnum;
        assert_eq!(
            OutputKind::from_str("snapcast", true).unwrap(),
            OutputKind::Snapcast
        );
    }

    #[test]
    fn engine_flag_rejects_browser() {
        use clap::ValueEnum;
        assert!(OutputKind::from_str("browser", true).is_err());
    }

    fn cast_choice(address: Option<&str>) -> OutputChoice {
        OutputChoice {
            kind: OutputKind::Chromecast,
            address: address.map(Into::into),
            name: None,
        }
    }

    fn cast_device(id: &str, name: &str, model: Option<&str>, addr: &str) -> discovery::CastDevice {
        discovery::CastDevice {
            id: id.into(),
            name: name.into(),
            model: model.map(Into::into),
            addr: addr.parse().unwrap(),
        }
    }

    const CAST_ID: &str = "0123456789abcdef0123456789abcdef";

    #[tokio::test]
    async fn chromecast_resolve_rules() {
        let o = outputs();
        assert_eq!(
            o.resolve(cast_choice(None)),
            Err("expected a Chromecast: its name, id or host:port".to_string())
        );
        let r = o.resolve(cast_choice(Some("10.0.0.5"))).unwrap();
        assert_eq!(r.address.as_deref(), Some("10.0.0.5:8009"));
        assert_eq!(r.name.as_deref(), Some("Chromecast"));
        let r = o.resolve(cast_choice(Some("10.0.0.5:9000"))).unwrap();
        assert_eq!(r.address.as_deref(), Some("10.0.0.5:9000"));
        let r = o
            .resolve(cast_choice(Some("01234567-89AB-cdef-0123-456789abcdef")))
            .unwrap();
        assert_eq!(r.address.as_deref(), Some(CAST_ID));
        let r = o.resolve(cast_choice(Some("  Living Room "))).unwrap();
        assert_eq!(r.address.as_deref(), Some("Living Room"));
        assert!(o.resolve(cast_choice(Some(&"x".repeat(65)))).is_err());
        let r = o
            .resolve(OutputChoice {
                name: Some(format!("  {}  ", "n".repeat(60))),
                ..cast_choice(Some("10.0.0.5"))
            })
            .unwrap();
        assert_eq!(r.name.unwrap().chars().count(), 40);
        // The configured device is the default.
        let configured = DefaultOutputs {
            chromecast: Some("10.0.0.6".into()),
            ..outputs()
        };
        let r = configured.resolve(cast_choice(None)).unwrap();
        assert_eq!(r.address.as_deref(), Some("10.0.0.6:8009"));
    }

    #[tokio::test]
    async fn chromecast_resolves_discovered_name_to_id() {
        let (discovery, tx) = Discovery::manual();
        tx.send(Found {
            casts: vec![cast_device(CAST_ID, "Living Room", None, "10.0.0.4:8009")],
            snapservers: vec![],
            renderers: vec![],
        })
        .unwrap();
        let o = DefaultOutputs {
            discovery,
            ..outputs()
        };
        for key in ["living room", CAST_ID] {
            let r = o.resolve(cast_choice(Some(key))).unwrap();
            assert_eq!(r.address.as_deref(), Some(CAST_ID), "{key}");
            assert_eq!(r.name.as_deref(), Some("Living Room"), "{key}");
        }
    }

    #[tokio::test]
    async fn chromecast_build_info() {
        let o = outputs();
        let (tx, _rx) = mpsc::unbounded_channel();
        let choice = o.resolve(cast_choice(Some("10.0.0.5"))).unwrap();
        let out = o.build(&choice, tx);
        assert_eq!(out.info.id, "chromecast:10.0.0.5:8009");
        assert_eq!(out.info.kind, OutputKind::Chromecast);
        assert_eq!(out.info.name, "Chromecast");
        assert_eq!(out.info.hint.as_deref(), Some(CAST_HINT));
        assert!(out.info.address.is_none());
        assert!(out.speakers.is_some() && out.link.is_some());
        assert!(out.browser_player.is_none() && out.stream.is_none());
    }

    #[tokio::test]
    async fn available_lists_discovered_casts() {
        let (discovery, tx) = Discovery::manual();
        let a = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        tx.send(Found {
            casts: vec![
                cast_device(a, "Kitchen", Some("Google Nest Mini"), "10.0.0.3:8009"),
                cast_device(CAST_ID, "Living Room", None, "10.0.0.4:8009"),
            ],
            snapservers: vec![],
            renderers: vec![],
        })
        .unwrap();
        let o = DefaultOutputs {
            discovery,
            local: Some(LocalDevice {
                name: "Test".into(),
            }),
            ..outputs()
        };
        let list = o.available();
        let kinds: Vec<_> = list.iter().map(|o| o.kind).collect();
        assert_eq!(
            kinds,
            [
                OutputKind::Browser,
                OutputKind::Nidoo,
                OutputKind::Snapcast,
                OutputKind::Stream,
                OutputKind::Local,
                OutputKind::Chromecast,
                OutputKind::Chromecast
            ]
        );
        let (k, l) = (&list[5], &list[6]);
        assert_eq!(k.name, "Kitchen");
        assert_eq!(k.note.as_deref(), Some("Google Nest Mini"));
        assert_eq!(k.id.as_deref(), Some(format!("chromecast:{a}").as_str()));
        assert_eq!(k.address.as_deref(), Some(a));
        assert!(!k.needs_address && k.default_address.is_none());
        assert_eq!(l.name, "Living Room");
        assert_eq!(l.note.as_deref(), Some("Chromecast"));
        assert!(
            list[..5]
                .iter()
                .all(|o| o.id.is_none() && o.address.is_none())
        );
    }

    #[tokio::test]
    async fn chromecast_is_pickable_without_discovery() {
        let o = outputs();
        assert!(o.pickable(OutputKind::Chromecast));
        assert!(!o.pickable(OutputKind::Local));
    }

    #[test]
    fn engine_flag_accepts_chromecast() {
        use clap::ValueEnum;
        assert_eq!(
            OutputKind::from_str("chromecast", true).unwrap(),
            OutputKind::Chromecast
        );
        assert_eq!(
            serde_json::to_string(&OutputKind::Chromecast).unwrap(),
            "\"chromecast\""
        );
    }
}
