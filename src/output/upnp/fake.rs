//! An in-memory renderer for session tests. Clones share state, and no method awaits
//! anything, so it works under paused time.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;

use super::control::{PositionInfo, RendererControl, TransportInfo, TransportState};
use super::xml::SoapError;
use crate::output::renderer::Volume;

struct State {
    state: TransportState,
    error: bool,
    rel_time: Option<Duration>,
    track_uri: String,
    reachable: bool,
    fail_next: HashMap<String, SoapError>,
    volume: Option<Volume>,
    calls: Vec<String>,
    metadata: String,
}

#[derive(Clone)]
pub(crate) struct FakeControl {
    st: Arc<Mutex<State>>,
}

#[allow(dead_code)]
impl FakeControl {
    pub fn new() -> Self {
        Self {
            st: Arc::new(Mutex::new(State {
                state: TransportState::NoMedia,
                error: false,
                rel_time: None,
                track_uri: String::new(),
                reachable: true,
                fail_next: HashMap::new(),
                volume: None,
                calls: Vec::new(),
                metadata: String::new(),
            })),
        }
    }

    pub fn set_state(&self, s: TransportState) {
        self.st.lock().unwrap().state = s;
    }
    pub fn set_error(&self, e: bool) {
        self.st.lock().unwrap().error = e;
    }
    pub fn set_rel_time(&self, t: Option<Duration>) {
        self.st.lock().unwrap().rel_time = t;
    }
    pub fn set_track_uri(&self, u: &str) {
        self.st.lock().unwrap().track_uri = u.to_string();
    }
    /// `false`: every call fails with `Transport` (and is not recorded).
    pub fn set_reachable(&self, r: bool) {
        self.st.lock().unwrap().reachable = r;
    }
    /// The next call named `action` (e.g. `"Play"`) is recorded, then fails with `e`.
    pub fn fail_next(&self, action: &str, e: SoapError) {
        self.st
            .lock()
            .unwrap()
            .fail_next
            .insert(action.to_string(), e);
    }
    pub fn set_volume_state(&self, v: Option<Volume>) {
        self.st.lock().unwrap().volume = v;
    }

    pub fn calls(&self) -> Vec<String> {
        self.st.lock().unwrap().calls.clone()
    }
    pub fn count(&self, prefix: &str) -> usize {
        self.st
            .lock()
            .unwrap()
            .calls
            .iter()
            .filter(|c| c.starts_with(prefix))
            .count()
    }
    pub fn last_metadata(&self) -> String {
        self.st.lock().unwrap().metadata.clone()
    }

    /// Records `call` and applies reachability and `fail_next`; `f` then changes the state.
    fn run<T>(
        &self,
        action: &str,
        call: String,
        f: impl FnOnce(&mut State) -> T,
    ) -> Result<T, SoapError> {
        let mut st = self.st.lock().unwrap();
        if !st.reachable {
            return Err(SoapError::Transport("unreachable".into()));
        }
        st.calls.push(call);
        if let Some(e) = st.fail_next.remove(action) {
            return Err(e);
        }
        Ok(f(&mut st))
    }
}

#[async_trait]
impl RendererControl for FakeControl {
    async fn set_uri(&self, uri: &str, metadata: &str) -> Result<(), SoapError> {
        self.run(
            "SetAVTransportURI",
            format!("SetAVTransportURI {uri}"),
            |s| {
                s.track_uri = uri.to_string();
                s.metadata = metadata.to_string();
                s.state = TransportState::Stopped;
            },
        )
    }
    async fn play(&self) -> Result<(), SoapError> {
        self.run("Play", "Play".into(), |s| s.state = TransportState::Playing)
    }
    async fn pause(&self) -> Result<(), SoapError> {
        self.run("Pause", "Pause".into(), |s| {
            s.state = TransportState::Paused
        })
    }
    async fn stop(&self) -> Result<(), SoapError> {
        self.run("Stop", "Stop".into(), |s| s.state = TransportState::Stopped)
    }
    async fn transport_info(&self) -> Result<TransportInfo, SoapError> {
        self.run("GetTransportInfo", "GetTransportInfo".into(), |s| {
            TransportInfo {
                state: s.state,
                error: s.error,
            }
        })
    }
    async fn position_info(&self) -> Result<PositionInfo, SoapError> {
        self.run("GetPositionInfo", "GetPositionInfo".into(), |s| {
            PositionInfo {
                rel_time: s.rel_time,
                track_uri: s.track_uri.clone(),
            }
        })
    }
    async fn volume(&self) -> Result<Option<Volume>, SoapError> {
        self.run("GetVolume", "GetVolume".into(), |s| s.volume)
    }
    async fn set_volume(&self, level: Option<f32>, muted: Option<bool>) -> Result<(), SoapError> {
        self.run(
            "SetVolume",
            format!("SetVolume {level:?} {muted:?}"),
            |s| match s.volume.as_mut() {
                Some(v) => {
                    if let Some(l) = level {
                        v.level = l.clamp(0.0, 1.0);
                    }
                    if let Some(m) = muted {
                        v.muted = m;
                    }
                    Ok(())
                }
                None => Err(SoapError::Fault {
                    code: None,
                    description: "no volume control".into(),
                }),
            },
        )?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fake_tracks_state_and_calls() {
        let f = FakeControl::new();
        let g = f.clone();
        g.set_uri("http://h/a", "<D/>").await.unwrap();
        assert_eq!(
            f.transport_info().await.unwrap().state,
            TransportState::Stopped
        );
        g.play().await.unwrap();
        assert_eq!(
            f.transport_info().await.unwrap().state,
            TransportState::Playing
        );
        assert_eq!(f.last_metadata(), "<D/>");
        assert_eq!(f.count("SetAVTransportURI"), 1);
        f.fail_next("Pause", SoapError::BadReply("x".into()));
        assert!(g.pause().await.is_err());
        assert!(g.pause().await.is_ok());
        f.set_reachable(false);
        assert!(matches!(g.play().await, Err(SoapError::Transport(_))));
        assert_eq!(f.count("Play"), 1);
    }
}
