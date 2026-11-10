//! A test `OutputFactory` that builds `FakeEngine`s and keeps handles to them.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{mpsc, watch};

use super::browser::stream::{Encoder, StreamBuf};

use super::{
    Output, OutputChoice, OutputFactory, OutputInfo, OutputKind, OutputOption, validate_host_port,
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

#[derive(Default)]
pub struct FakeOutputs {
    pub speakers: Option<Arc<dyn SpeakerControl>>,
    pub browser_player: Option<String>,
    /// Every engine built, in order.
    pub engines: Arc<Mutex<Vec<FakeEngine>>>,
    /// The sender side of every link built, in order.
    pub links: Arc<Mutex<Vec<watch::Sender<Option<bool>>>>>,
}

impl OutputFactory for FakeOutputs {
    fn resolve(&self, choice: OutputChoice) -> Result<OutputChoice, String> {
        match choice.kind {
            OutputKind::Nidoo => {
                let address = choice.address.unwrap_or_else(|| DEFAULT_ADDRESS.into());
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

    fn build(&self, choice: &OutputChoice, _events: mpsc::UnboundedSender<EngineEvent>) -> Output {
        let engine = FakeEngine::new(Duration::ZERO);
        self.engines.lock().unwrap().push(engine.clone());
        let (tx, rx) = watch::channel(None);
        self.links.lock().unwrap().push(tx);
        let info = match choice.kind {
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
        }
    }

    fn available(&self) -> Vec<OutputOption> {
        vec![OutputOption {
            kind: OutputKind::Nidoo,
            name: "Nidoo".into(),
            needs_address: true,
            default_address: Some(DEFAULT_ADDRESS.into()),
        }]
    }
}
