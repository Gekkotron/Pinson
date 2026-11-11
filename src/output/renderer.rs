//! Pieces shared by the renderer outputs (Chromecast, UPnP): the per-song stream slot,
//! the song's metadata, the events a device raises, and its volume as one speaker.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::http::StatusCode;
use futures_util::future::BoxFuture;
use tokio::sync::watch;

use super::browser::Stream;
use crate::speakers::{Speaker, SpeakerChange, SpeakerControl, SpeakerError};

/// The one per-song stream being served: the renderer fetches it from
/// `/api/cast/stream/{generation}` or `/api/upnp/stream/{generation}/song.mp3`.
#[derive(Clone, Default)]
pub struct SongStreams {
    current: Arc<Mutex<Option<Arc<Stream>>>>,
}

impl SongStreams {
    pub fn set(&self, s: Option<Arc<Stream>>) {
        *self.current.lock().unwrap() = s;
    }

    /// The stream for `generation`, if it is the current one.
    pub fn get(&self, generation: u64) -> Option<Arc<Stream>> {
        self.current
            .lock()
            .unwrap()
            .as_ref()
            .filter(|s| s.generation == generation)
            .cloned()
    }

    /// Drops the current stream if it is `generation`'s; a newer one stays.
    pub fn clear_if(&self, generation: u64) {
        let mut cur = self.current.lock().unwrap();
        if cur.as_ref().is_some_and(|s| s.generation == generation) {
            *cur = None;
        }
    }
}

/// What a renderer is told about the song. `duration` is unused by Cast.
#[derive(Clone, Debug, PartialEq)]
pub struct SongMeta {
    pub title: String,
    pub artist: String,
    pub album: Option<String>,
    pub image: Option<String>,
    pub duration: Option<Duration>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Volume {
    pub level: f32,
    pub muted: bool,
}

/// Something that happened on the device which the player should mirror.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum External {
    Pause,
    Resume,
}

/// Sets a renderer's level (0.0 to 1.0) and/or mute.
pub type VolumeSetter =
    Arc<dyn Fn(Option<f32>, Option<bool>) -> BoxFuture<'static, Result<(), String>> + Send + Sync>;

/// The volume of a renderer, as one speaker.
pub struct RendererSpeakers {
    volume: watch::Receiver<Option<Volume>>,
    link: watch::Receiver<Option<bool>>,
    setter: VolumeSetter,
    name: String,
    id: &'static str,
    unreachable: &'static str,
}

impl RendererSpeakers {
    pub fn new(
        volume: watch::Receiver<Option<Volume>>,
        link: watch::Receiver<Option<bool>>,
        setter: VolumeSetter,
        name: String,
        id: &'static str,
        unreachable: &'static str,
    ) -> Self {
        Self {
            volume,
            link,
            setter,
            name,
            id,
            unreachable,
        }
    }
}

#[async_trait]
impl SpeakerControl for RendererSpeakers {
    async fn list(&self) -> Result<Vec<Speaker>, SpeakerError> {
        let volume = *self.volume.borrow();
        match (*self.link.borrow(), volume) {
            (Some(true), Some(v)) => Ok(vec![Speaker {
                id: self.id.into(),
                name: self.name.clone(),
                volume: (v.level * 100.0).round().clamp(0.0, 100.0) as u8,
                muted: v.muted,
                connected: true,
            }]),
            _ => Err(SpeakerError::Unreachable(self.unreachable.into())),
        }
    }

    async fn set(&self, id: &str, change: &SpeakerChange) -> Result<(), SpeakerError> {
        if id != self.id {
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

    type Calls = Arc<Mutex<Vec<(Option<f32>, Option<bool>)>>>;

    fn make(volume: Option<Volume>, link: Option<bool>) -> (RendererSpeakers, Calls) {
        // The senders stay alive in the setter closure, so the receivers keep their values.
        let (vt, vrx) = watch::channel(volume);
        let (lt, lrx) = watch::channel(link);
        let calls = Arc::new(Mutex::new(Vec::new()));
        let c = calls.clone();
        let keep = (vt, lt);
        let setter: VolumeSetter = Arc::new(move |l, m| {
            let _ = &keep;
            c.lock().unwrap().push((l, m));
            Box::pin(async { Ok(()) })
        });
        (
            RendererSpeakers::new(
                vrx,
                lrx,
                setter,
                "Kitchen".into(),
                "cast",
                "Can't reach the Chromecast",
            ),
            calls,
        )
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

    fn stream(generation: u64) -> Arc<Stream> {
        Arc::new(Stream::new(
            generation,
            Duration::ZERO,
            crate::output::browser::StreamBuf::new(),
            tokio::spawn(async { Ok(()) }),
        ))
    }

    #[tokio::test]
    async fn streams_get_and_clear_if() {
        let s = SongStreams::default();
        s.set(Some(stream(3)));
        assert!(s.get(3).is_some());
        assert!(s.get(4).is_none());
        s.clear_if(4);
        assert!(s.get(3).is_some());
        s.clear_if(3);
        assert!(s.get(3).is_none());
    }
}
