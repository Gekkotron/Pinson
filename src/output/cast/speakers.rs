//! Volume of a Chromecast, as the one speaker "cast".

use std::sync::Arc;

use async_trait::async_trait;
use axum::http::StatusCode;
use futures_util::future::BoxFuture;
use tokio::sync::watch;

use super::session::Volume;
use crate::speakers::{Speaker, SpeakerChange, SpeakerControl, SpeakerError};

const ID: &str = "cast";
const UNREACHABLE: &str = "Can't reach the Chromecast";

type Setter =
    Arc<dyn Fn(Option<f32>, Option<bool>) -> BoxFuture<'static, Result<(), String>> + Send + Sync>;

pub struct CastSpeakers {
    volume: watch::Receiver<Option<Volume>>,
    link: watch::Receiver<Option<bool>>,
    setter: Setter,
    name: String,
}

impl CastSpeakers {
    pub fn new(
        session_volume: watch::Receiver<Option<Volume>>,
        link: watch::Receiver<Option<bool>>,
        setter: Setter,
        name: String,
    ) -> Self {
        Self {
            volume: session_volume,
            link,
            setter,
            name,
        }
    }
}

#[async_trait]
impl SpeakerControl for CastSpeakers {
    async fn list(&self) -> Result<Vec<Speaker>, SpeakerError> {
        let volume = *self.volume.borrow();
        match (*self.link.borrow(), volume) {
            (Some(true), Some(v)) => Ok(vec![Speaker {
                id: ID.into(),
                name: self.name.clone(),
                volume: (v.level * 100.0).round().clamp(0.0, 100.0) as u8,
                muted: v.muted,
                connected: true,
            }]),
            _ => Err(SpeakerError::Unreachable(UNREACHABLE.into())),
        }
    }

    async fn set(&self, id: &str, change: &SpeakerChange) -> Result<(), SpeakerError> {
        if id != ID {
            return Err(SpeakerError::Rejected(
                StatusCode::NOT_FOUND,
                "unknown speaker".into(),
            ));
        }
        if change.volume.is_some_and(|v| v > 100) {
            return Err(SpeakerError::Rejected(
                StatusCode::BAD_REQUEST,
                "volume must be 0 to 100".into(),
            ));
        }
        let level = change.volume.map(|v| f32::from(v) / 100.0);
        (self.setter)(level, change.muted)
            .await
            .map_err(SpeakerError::Unreachable)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    type Calls = Arc<Mutex<Vec<(Option<f32>, Option<bool>)>>>;

    fn make(volume: Option<Volume>, link: Option<bool>) -> (CastSpeakers, Calls) {
        // The senders stay alive in the setter closure, so the receivers keep their values.
        let (vt, vrx) = watch::channel(volume);
        let (lt, lrx) = watch::channel(link);
        let calls = Arc::new(Mutex::new(Vec::new()));
        let c = calls.clone();
        let keep = (vt, lt);
        let setter: Setter = Arc::new(move |l, m| {
            let _ = &keep;
            c.lock().unwrap().push((l, m));
            Box::pin(async { Ok(()) })
        });
        (CastSpeakers::new(vrx, lrx, setter, "Kitchen".into()), calls)
    }

    #[tokio::test]
    async fn list_maps_level_to_percent() {
        let (s, _) = make(
            Some(Volume {
                level: 0.335,
                muted: true,
            }),
            Some(true),
        );
        let list = s.list().await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, "cast");
        assert_eq!(list[0].name, "Kitchen");
        assert_eq!(list[0].volume, 34);
        assert!(list[0].muted && list[0].connected);
    }

    #[tokio::test]
    async fn down_is_unreachable() {
        let v = Some(Volume {
            level: 0.5,
            muted: false,
        });
        for (vol, link) in [(v, Some(false)), (v, None), (None, Some(true))] {
            let (s, _) = make(vol, link);
            assert!(matches!(
                s.list().await,
                Err(SpeakerError::Unreachable(m)) if m == "Can't reach the Chromecast"
            ));
        }
    }

    #[tokio::test]
    async fn unknown_id_is_404() {
        let (s, calls) = make(None, Some(true));
        let change = SpeakerChange {
            volume: Some(10),
            muted: None,
        };
        assert!(matches!(
            s.set("other", &change).await,
            Err(SpeakerError::Rejected(StatusCode::NOT_FOUND, m)) if m == "unknown speaker"
        ));
        assert!(calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn volume_over_100_is_400() {
        let (s, _) = make(None, Some(true));
        let change = SpeakerChange {
            volume: Some(101),
            muted: None,
        };
        assert!(matches!(
            s.set("cast", &change).await,
            Err(SpeakerError::Rejected(StatusCode::BAD_REQUEST, m)) if m == "volume must be 0 to 100"
        ));
    }

    #[tokio::test]
    async fn set_passes_level_and_mute() {
        let (s, calls) = make(None, Some(true));
        let change = SpeakerChange {
            volume: Some(50),
            muted: Some(true),
        };
        s.set("cast", &change).await.unwrap();
        assert_eq!(*calls.lock().unwrap(), vec![(Some(0.5), Some(true))]);
    }
}
