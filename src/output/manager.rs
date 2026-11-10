//! The current output: startup precedence, switching, persistence and the view clients see.

use std::sync::{Arc, Mutex};

use serde::Serialize;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use super::{OutputChoice, OutputFactory, OutputInfo, OutputOption};
use crate::db::Db;
use crate::engine::EngineEvent;
use crate::player::PlayerHandle;
use crate::speakers::SpeakerControl;

/// The `settings` key holding the JSON of the resolved `OutputChoice`.
pub const OUTPUT_SETTING: &str = "output";

/// The output in use, as shown to clients.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CurrentOutput {
    #[serde(flatten)]
    pub info: OutputInfo,
    /// `None` when the output has no link to watch or hasn't tried yet.
    pub reachable: Option<bool>,
    pub speakers: bool,
    pub browser_player: bool,
}

/// Everything a client needs to show and pick outputs.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OutputView {
    pub current: Option<CurrentOutput>,
    pub startup_id: Option<String>,
    pub available: Vec<OutputOption>,
}

#[derive(Debug)]
pub enum SetOutputError {
    /// The client asked for something that can't work (400).
    Invalid(String),
    /// Switching failed inside Pinson.
    Failed(anyhow::Error),
}

#[derive(Default)]
struct Extras {
    speakers: Option<Arc<dyn SpeakerControl>>,
    browser_player: Option<String>,
    /// The resolved choice the current output was built from.
    choice: Option<OutputChoice>,
}

struct Inner {
    factory: Arc<dyn OutputFactory>,
    events: mpsc::UnboundedSender<EngineEvent>,
    db: Db,
    player: PlayerHandle,
    configured: Option<OutputChoice>,
    view: watch::Sender<OutputView>,
    extras: Mutex<Extras>,
    /// Serializes `set`; holds the running link watcher.
    watcher: tokio::sync::Mutex<Option<JoinHandle<()>>>,
}

#[derive(Clone)]
pub struct Outputs {
    inner: Arc<Inner>,
}

impl Outputs {
    pub async fn start(
        factory: Arc<dyn OutputFactory>,
        events: mpsc::UnboundedSender<EngineEvent>,
        db: Db,
        player: PlayerHandle,
        configured: Option<OutputChoice>,
    ) -> anyhow::Result<Outputs> {
        let (initial, from_config) = match configured {
            Some(choice) => {
                let resolved = factory
                    .resolve(choice)
                    .map_err(|e| anyhow::anyhow!("invalid configured output: {e}"))?;
                (Some(resolved), true)
            }
            None => (Self::saved(&factory, &db), false),
        };
        let (view, _) = watch::channel(OutputView {
            current: None,
            startup_id: None,
            available: factory.available(),
        });
        let outputs = Outputs {
            inner: Arc::new(Inner {
                factory,
                events,
                db,
                player,
                configured: from_config.then(|| initial.clone()).flatten(),
                view,
                extras: Mutex::new(Extras::default()),
                watcher: tokio::sync::Mutex::new(None),
            }),
        };
        if let Some(choice) = initial {
            let mut watcher = outputs.inner.watcher.lock().await;
            let id = outputs.activate(&choice, &mut watcher).await?;
            if from_config {
                outputs.inner.view.send_modify(|v| v.startup_id = Some(id));
            }
        }
        Ok(outputs)
    }

    /// The saved choice, or `None` when absent or no longer valid.
    fn saved(factory: &Arc<dyn OutputFactory>, db: &Db) -> Option<OutputChoice> {
        let raw = match db.setting(OUTPUT_SETTING) {
            Ok(Some(raw)) => raw,
            Ok(None) => return None,
            Err(e) => {
                tracing::warn!("couldn't read the saved output: {e:#}");
                return None;
            }
        };
        let choice: OutputChoice = match serde_json::from_str(&raw) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("ignoring the saved output {raw:?}: {e}");
                return None;
            }
        };
        match factory.resolve(choice) {
            Ok(c) => Some(c),
            Err(e) => {
                tracing::warn!("ignoring the saved output {raw:?}: {e}");
                None
            }
        }
    }

    /// Builds `choice`, moves the player onto it and publishes it; returns its id.
    async fn activate(
        &self,
        choice: &OutputChoice,
        watcher: &mut Option<JoinHandle<()>>,
    ) -> anyhow::Result<String> {
        let inner = &self.inner;
        let output = inner.factory.build(choice, inner.events.clone());
        inner.player.set_output(output.engine).await?;
        if let Some(old) = watcher.take() {
            old.abort();
        }
        let current = CurrentOutput {
            info: output.info,
            reachable: output.link.as_ref().and_then(|l| *l.borrow()),
            speakers: output.speakers.is_some(),
            browser_player: output.browser_player.is_some(),
        };
        let id = current.info.id.clone();
        *inner.extras.lock().unwrap() = Extras {
            speakers: output.speakers,
            browser_player: output.browser_player,
            choice: Some(choice.clone()),
        };
        inner.view.send_modify(|v| v.current = Some(current));
        if let Some(link) = output.link {
            *watcher = Some(tokio::spawn(watch_link(
                inner.view.clone(),
                id.clone(),
                link,
            )));
        }
        Ok(id)
    }

    pub fn view(&self) -> OutputView {
        self.inner.view.borrow().clone()
    }

    pub fn subscribe(&self) -> watch::Receiver<OutputView> {
        self.inner.view.subscribe()
    }

    pub async fn set(&self, choice: OutputChoice) -> Result<OutputView, SetOutputError> {
        let inner = &self.inner;
        let mut watcher = inner.watcher.lock().await;
        if !inner
            .factory
            .available()
            .iter()
            .any(|o| o.kind == choice.kind)
        {
            return Err(SetOutputError::Invalid("unknown output".into()));
        }
        let resolved = inner
            .factory
            .resolve(choice)
            .map_err(SetOutputError::Invalid)?;
        if inner.extras.lock().unwrap().choice.as_ref() == Some(&resolved) {
            return Ok(self.view());
        }
        self.activate(&resolved, &mut watcher)
            .await
            .map_err(SetOutputError::Failed)?;
        if inner.configured.is_none() {
            let json =
                serde_json::to_string(&resolved).map_err(|e| SetOutputError::Failed(e.into()))?;
            if let Err(e) = inner.db.set_setting(OUTPUT_SETTING, &json) {
                tracing::warn!("couldn't save the output choice: {e:#}");
            }
        }
        Ok(self.view())
    }

    pub fn speakers(&self) -> Option<Arc<dyn SpeakerControl>> {
        self.inner.extras.lock().unwrap().speakers.clone()
    }

    /// The current output's browser-player (Sendspin) URL, if it has one.
    pub fn browser_player(&self) -> Option<String> {
        self.inner.extras.lock().unwrap().browser_player.clone()
    }
}

/// Copies link changes into the view; republishes only when `reachable` changes.
async fn watch_link(
    view: watch::Sender<OutputView>,
    id: String,
    mut link: watch::Receiver<Option<bool>>,
) {
    loop {
        let reachable = *link.borrow_and_update();
        view.send_if_modified(|v| match &mut v.current {
            Some(c) if c.info.id == id && c.reachable != reachable => {
                c.reachable = reachable;
                true
            }
            _ => false,
        });
        if link.changed().await.is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::engine::fake::FakeSource;
    use crate::history::History;
    use crate::output::OutputKind;
    use crate::output::fake::FakeOutputs;
    use crate::player::Autoplay;
    use crate::radio::fake::FakeRadio;
    use crate::song::Song;

    struct Rig {
        outputs: Outputs,
        fake: Arc<FakeOutputs>,
        db: Db,
        player: PlayerHandle,
    }

    fn nidoo(address: &str) -> OutputChoice {
        OutputChoice {
            kind: OutputKind::Nidoo,
            address: Some(address.into()),
            name: None,
        }
    }

    fn song(id: &str) -> Song {
        Song {
            video_id: id.into(),
            title: id.into(),
            artists: "artist".into(),
            album: None,
            duration_secs: Some(200),
            artwork_url: None,
        }
    }

    async fn rig_with(
        fake: FakeOutputs,
        db: Db,
        configured: Option<OutputChoice>,
    ) -> anyhow::Result<Rig> {
        let (tx, rx) = mpsc::unbounded_channel();
        let history = History::new(Db::in_memory());
        let player = crate::player::spawn_player(
            None,
            rx,
            Arc::new(FakeSource::default()),
            Autoplay {
                radio: Arc::new(FakeRadio::default()),
                history,
                enabled: false,
            },
        );
        let fake = Arc::new(fake);
        let outputs =
            Outputs::start(fake.clone(), tx, db.clone(), player.clone(), configured).await?;
        Ok(Rig {
            outputs,
            fake,
            db,
            player,
        })
    }

    async fn rig(db: Db, configured: Option<OutputChoice>) -> Rig {
        rig_with(FakeOutputs::default(), db, configured)
            .await
            .unwrap()
    }

    fn current_id(r: &Rig) -> Option<String> {
        r.outputs.view().current.map(|c| c.info.id)
    }

    #[tokio::test]
    async fn starts_without_output_when_nothing_configured_or_saved() {
        let r = rig(Db::in_memory(), None).await;
        let v = r.outputs.view();
        assert_eq!(v.current, None);
        assert_eq!(v.startup_id, None);
        assert_eq!(v.available.len(), 1);
        assert!(r.fake.engines.lock().unwrap().is_empty());
        assert!(r.outputs.speakers().is_none());
    }

    #[tokio::test]
    async fn configured_output_wins_over_saved() {
        let db = Db::in_memory();
        db.set_setting(OUTPUT_SETTING, r#"{"kind":"nidoo","address":"saved:1"}"#)
            .unwrap();
        let r = rig(db, Some(nidoo("conf:2"))).await;
        let v = r.outputs.view();
        assert_eq!(current_id(&r).as_deref(), Some("nidoo:conf:2"));
        assert_eq!(v.startup_id.as_deref(), Some("nidoo:conf:2"));
    }

    #[tokio::test]
    async fn invalid_configured_output_is_an_error() {
        let res = rig_with(FakeOutputs::default(), Db::in_memory(), Some(nidoo("bad"))).await;
        assert!(res.is_err());
    }

    #[tokio::test]
    async fn saved_output_is_used_when_not_configured() {
        let db = Db::in_memory();
        db.set_setting(OUTPUT_SETTING, r#"{"kind":"nidoo","address":"saved:1"}"#)
            .unwrap();
        let r = rig(db, None).await;
        assert_eq!(current_id(&r).as_deref(), Some("nidoo:saved:1"));
        assert_eq!(r.outputs.view().startup_id, None);
    }

    #[tokio::test]
    async fn corrupt_saved_output_is_ignored() {
        for bad in [
            "{not json",
            r#"{"kind":"airplay"}"#,
            r#"{"kind":"nidoo","address":"host"}"#,
        ] {
            let db = Db::in_memory();
            db.set_setting(OUTPUT_SETTING, bad).unwrap();
            let r = rig(db, None).await;
            assert_eq!(r.outputs.view().current, None, "{bad}");
        }
    }

    #[tokio::test]
    async fn set_saves_when_not_configured() {
        let r = rig(Db::in_memory(), None).await;
        let v = r.outputs.set(nidoo("a:1")).await.unwrap();
        assert_eq!(v.current.unwrap().info.id, "nidoo:a:1");
        assert_eq!(v.startup_id, None);
        let saved = r.db.setting(OUTPUT_SETTING).unwrap().unwrap();
        assert_eq!(saved, r#"{"kind":"nidoo","address":"a:1"}"#);
    }

    #[tokio::test]
    async fn set_same_output_is_a_no_op() {
        let r = rig(Db::in_memory(), None).await;
        let first = r.outputs.set(nidoo("a:1")).await.unwrap();
        assert_eq!(r.fake.engines.lock().unwrap().len(), 1);
        let before = r.player.state().version;
        let again = r.outputs.set(nidoo("a:1")).await.unwrap();
        assert_eq!(r.fake.engines.lock().unwrap().len(), 1);
        assert_eq!(
            again.current.unwrap().info.id,
            first.current.unwrap().info.id
        );
        assert_eq!(r.player.state().version, before);
    }

    #[tokio::test]
    async fn set_is_not_saved_when_configured() {
        let db = Db::in_memory();
        db.set_setting(OUTPUT_SETTING, r#"{"kind":"nidoo","address":"old:1"}"#)
            .unwrap();
        let r = rig(db, Some(nidoo("conf:2"))).await;
        let v = r.outputs.set(nidoo("b:3")).await.unwrap();
        assert_eq!(v.current.unwrap().info.id, "nidoo:b:3");
        assert_eq!(v.startup_id.as_deref(), Some("nidoo:conf:2"));
        assert_eq!(
            r.db.setting(OUTPUT_SETTING).unwrap().as_deref(),
            Some(r#"{"kind":"nidoo","address":"old:1"}"#)
        );
    }

    #[tokio::test]
    async fn set_rejects_bad_address_and_keeps_current() {
        let r = rig(Db::in_memory(), Some(nidoo("conf:2"))).await;
        for bad in ["", "host", "host:0", "host:70000", "http://h:1", "h st:1"] {
            let res = r.outputs.set(nidoo(bad)).await;
            assert!(matches!(res, Err(SetOutputError::Invalid(_))), "{bad}");
        }
        assert_eq!(current_id(&r).as_deref(), Some("nidoo:conf:2"));
        assert_eq!(r.fake.engines.lock().unwrap().len(), 1);
        assert_eq!(r.db.setting(OUTPUT_SETTING).unwrap(), None);
    }

    #[tokio::test]
    async fn set_rejects_kind_not_offered() {
        let r = rig(Db::in_memory(), None).await;
        let res = r
            .outputs
            .set(OutputChoice {
                kind: OutputKind::Log,
                address: None,
                name: None,
            })
            .await;
        match res {
            Err(SetOutputError::Invalid(m)) => assert_eq!(m, "unknown output"),
            other => panic!("expected Invalid, got {other:?}"),
        }
        assert_eq!(r.outputs.view().current, None);
    }

    async fn wait_reachable(rx: &mut watch::Receiver<OutputView>, want: Option<bool>) {
        tokio::time::timeout(
            Duration::from_secs(5),
            rx.wait_for(|v| v.current.as_ref().is_some_and(|c| c.reachable == want)),
        )
        .await
        .expect("timed out")
        .unwrap();
    }

    #[tokio::test]
    async fn reachable_follows_link() {
        let r = rig(Db::in_memory(), Some(nidoo("conf:2"))).await;
        let mut rx = r.outputs.subscribe();
        let link = r.fake.links.lock().unwrap().last().unwrap().clone();
        link.send(Some(false)).unwrap();
        wait_reachable(&mut rx, Some(false)).await;
        link.send(Some(true)).unwrap();
        wait_reachable(&mut rx, Some(true)).await;
        assert_eq!(r.outputs.view().current.unwrap().reachable, Some(true));
    }

    #[tokio::test]
    async fn repeated_link_value_does_not_republish() {
        let r = rig(Db::in_memory(), Some(nidoo("conf:2"))).await;
        let mut rx = r.outputs.subscribe();
        let link = r.fake.links.lock().unwrap().last().unwrap().clone();
        link.send(Some(false)).unwrap();
        wait_reachable(&mut rx, Some(false)).await;
        rx.mark_unchanged();
        link.send(Some(false)).unwrap();
        let res = tokio::time::timeout(Duration::from_millis(100), rx.changed()).await;
        assert!(res.is_err(), "republished an unchanged reachable");
    }

    #[tokio::test]
    async fn old_link_is_ignored_after_set() {
        let r = rig(Db::in_memory(), None).await;
        r.outputs.set(nidoo("a:1")).await.unwrap();
        let old = r.fake.links.lock().unwrap().last().unwrap().clone();
        r.outputs.set(nidoo("b:2")).await.unwrap();
        let _ = old.send(Some(false));
        let mut rx = r.outputs.subscribe();
        let res = tokio::time::timeout(Duration::from_millis(100), rx.changed()).await;
        assert!(res.is_err());
        assert_eq!(r.outputs.view().current.unwrap().reachable, None);
    }

    #[tokio::test]
    async fn subscribers_see_set() {
        let r = rig(Db::in_memory(), None).await;
        let mut rx = r.outputs.subscribe();
        r.outputs.set(nidoo("a:1")).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), rx.changed())
            .await
            .expect("timed out")
            .unwrap();
        let v = rx.borrow().clone();
        assert_eq!(v.current.unwrap().info.id, "nidoo:a:1");
    }

    #[tokio::test]
    async fn set_moves_player_to_new_engine() {
        let r = rig(Db::in_memory(), None).await;
        r.player.play_next(song("A")).await.unwrap();
        r.outputs.set(nidoo("a:1")).await.unwrap();
        let engine = r.fake.engines.lock().unwrap().last().unwrap().clone();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !engine.calls().iter().any(|c| c == "play:A") {
            assert!(
                tokio::time::Instant::now() < deadline,
                "{:?}",
                engine.calls()
            );
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test]
    async fn browser_player_and_speakers_follow_current_output() {
        let fake = FakeOutputs {
            browser_player: Some("ws://x/sendspin".into()),
            ..FakeOutputs::default()
        };
        let r = rig_with(fake, Db::in_memory(), None).await.unwrap();
        assert_eq!(r.outputs.browser_player(), None);
        assert!(!r.outputs.view().current.is_some_and(|c| c.browser_player));
        r.outputs.set(nidoo("a:1")).await.unwrap();
        assert_eq!(
            r.outputs.browser_player().as_deref(),
            Some("ws://x/sendspin")
        );
        assert!(r.outputs.view().current.unwrap().browser_player);
        // Default FakeOutputs has no browser player.
        let r2 = rig(Db::in_memory(), Some(nidoo("a:1"))).await;
        assert_eq!(r2.outputs.browser_player(), None);
        assert!(!r2.outputs.view().current.unwrap().browser_player);
    }

    #[test]
    fn view_serializes_camel_case() {
        let v = OutputView {
            current: Some(CurrentOutput {
                info: OutputInfo {
                    id: "log".into(),
                    kind: OutputKind::Log,
                    name: "Log".into(),
                    address: None,
                    hint: None,
                },
                reachable: None,
                speakers: false,
                browser_player: false,
            }),
            startup_id: None,
            available: vec![],
        };
        let json = serde_json::to_value(&v).unwrap();
        assert_eq!(json["current"]["id"], "log");
        assert_eq!(json["current"]["browserPlayer"], false);
        assert!(json["startupId"].is_null());
    }
}
