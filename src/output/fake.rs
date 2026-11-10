//! A test `OutputFactory` that builds `FakeEngine`s and keeps handles to them.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{mpsc, watch};

use super::browser::DeviceHub;
use super::browser::stream::{Encoder, StreamBuf};
use super::stream_url::{LiveEncoder, PushFn, build_stream};

use super::{
    NIDOO_NOTE, Output, OutputChoice, OutputFactory, OutputInfo, OutputKind, OutputOption,
    SNAPCAST_HINT, SNAPCAST_NOTE, STREAM_NOTE, build_browser, resolve_browser, this_device_option,
    validate_host_port,
};
use crate::engine::EngineEvent;
use crate::engine::fake::FakeEngine;
use crate::speakers::SpeakerControl;

const DEFAULT_ADDRESS: &str = "127.0.0.1:4953";

/// An `Encoder` that copies the PCM bytes unchanged, for tests.
pub struct Passthrough;

impl Encoder for Passthrough {
    fn start(
        &self,
        mut reader: Box<dyn crate::source::PcmReader>,
        out: Arc<StreamBuf>,
    ) -> tokio::task::JoinHandle<anyhow::Result<()>> {
        tokio::spawn(async move {
            let mut buf = vec![0u8; 16 * 1024];
            loop {
                match reader.read(&mut buf).await {
                    Ok(0) => {
                        out.finish();
                        return Ok(());
                    }
                    Ok(n) => out.push(&buf[..n]),
                    Err(e) => {
                        out.fail(format!("{e:#}"));
                        return Err(e);
                    }
                }
            }
        })
    }
}

/// A `LiveEncoder` that copies the PCM bytes unchanged, for tests.
pub struct PassthroughLive;

impl LiveEncoder for PassthroughLive {
    fn run(
        &self,
        mut reader: Box<dyn crate::source::PcmReader>,
        mut push: PushFn,
    ) -> futures_util::future::BoxFuture<'static, anyhow::Result<()>> {
        Box::pin(async move {
            let mut buf = vec![0u8; 3_840];
            loop {
                match reader.read(&mut buf).await? {
                    0 => return Ok(()),
                    n => push(&buf[..n]),
                }
            }
        })
    }
}

#[derive(Default)]
pub struct FakeOutputs {
    pub speakers: Option<Arc<dyn SpeakerControl>>,
    pub browser_player: Option<String>,
    /// Every engine built, in order.
    pub engines: Arc<Mutex<Vec<FakeEngine>>>,
    /// The sender side of every link built, in order.
    pub links: Arc<Mutex<Vec<watch::Sender<Option<bool>>>>>,
    /// When set, "This device" is offered and builds a real `BrowserEngine`.
    pub devices: Option<DeviceHub>,
}

impl OutputFactory for FakeOutputs {
    fn resolve(&self, choice: OutputChoice) -> Result<OutputChoice, String> {
        match choice.kind {
            OutputKind::Browser => resolve_browser(choice),
            kind @ (OutputKind::Nidoo | OutputKind::Snapcast) => {
                let address = choice.address.unwrap_or_else(|| DEFAULT_ADDRESS.into());
                validate_host_port(&address)?;
                Ok(OutputChoice {
                    kind,
                    address: Some(address),
                    name: None,
                })
            }
            OutputKind::Stream => Ok(OutputChoice {
                kind: OutputKind::Stream,
                address: None,
                name: None,
            }),
            OutputKind::Log => Ok(OutputChoice {
                kind: OutputKind::Log,
                address: None,
                name: None,
            }),
        }
    }

    fn build(&self, choice: &OutputChoice, events: mpsc::UnboundedSender<EngineEvent>) -> Output {
        if choice.kind == OutputKind::Browser {
            let devices = self
                .devices
                .as_ref()
                .expect("browser output built without a device hub");
            return build_browser(devices, Arc::new(Passthrough), choice, events);
        }
        if choice.kind == OutputKind::Stream {
            return build_stream(Arc::new(PassthroughLive), Duration::from_secs(1), events);
        }
        let engine = FakeEngine::new(Duration::ZERO);
        self.engines.lock().unwrap().push(engine.clone());
        let (tx, rx) = watch::channel(None);
        self.links.lock().unwrap().push(tx);
        let info = match choice.kind {
            OutputKind::Browser | OutputKind::Stream => unreachable!("built above"),
            OutputKind::Nidoo => {
                let address = choice
                    .address
                    .clone()
                    .unwrap_or_else(|| DEFAULT_ADDRESS.into());
                OutputInfo {
                    id: format!("nidoo:{address}"),
                    kind: OutputKind::Nidoo,
                    name: "Nidoo".into(),
                    address: Some(address),
                    hint: Some(super::NIDOO_HINT.into()),
                }
            }
            OutputKind::Snapcast => {
                let address = choice
                    .address
                    .clone()
                    .unwrap_or_else(|| DEFAULT_ADDRESS.into());
                OutputInfo {
                    id: format!("snapcast:{address}"),
                    kind: OutputKind::Snapcast,
                    name: "Snapcast".into(),
                    address: Some(address),
                    hint: Some(SNAPCAST_HINT.into()),
                }
            }
            OutputKind::Log => OutputInfo {
                id: "log".into(),
                kind: OutputKind::Log,
                name: "Log only (no sound)".into(),
                address: None,
                hint: None,
            },
        };
        Output {
            engine: Box::new(engine),
            info,
            speakers: self.speakers.clone(),
            link: Some(rx),
            browser_player: self.browser_player.clone(),
            stream: None,
        }
    }

    fn available(&self) -> Vec<OutputOption> {
        let mut options = Vec::new();
        if self.devices.is_some() {
            options.push(this_device_option());
        }
        options.push(OutputOption {
            kind: OutputKind::Nidoo,
            name: "Nidoo".into(),
            needs_address: true,
            default_address: Some(DEFAULT_ADDRESS.into()),
            note: Some(NIDOO_NOTE.into()),
        });
        options.push(OutputOption {
            kind: OutputKind::Snapcast,
            name: "Snapcast".into(),
            needs_address: true,
            default_address: Some(DEFAULT_ADDRESS.into()),
            note: Some(SNAPCAST_NOTE.into()),
        });
        options.push(OutputOption {
            kind: OutputKind::Stream,
            name: "Stream URL".into(),
            needs_address: false,
            default_address: None,
            note: Some(STREAM_NOTE.into()),
        });
        options
    }
}
