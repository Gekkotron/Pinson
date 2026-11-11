//! This computer: the server's own sound card, through cpal.
//!
//! The paced writer fills a PCM queue; the sound card's callback drains it.
//! The cpal stream lives on its own thread, which exists as long as the sink.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use axum::http::StatusCode;
use futures_util::FutureExt;
use tokio::sync::{mpsc, oneshot};

use super::{Output, OutputInfo, OutputKind};
use crate::engine::EngineEvent;
use crate::engine::paced::{PacedConfig, PacedEngine};
use crate::engine::queue::{PcmQueue, QueueConnector};
use crate::speakers::{Speaker, SpeakerChange, SpeakerControl, SpeakerError};

/// An output device Pinson can use.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalDevice {
    pub name: String,
}

/// Software volume and mute, shared with the sound card callback.
pub struct Gain {
    volume: AtomicU8,
    muted: AtomicBool,
}

impl Default for Gain {
    fn default() -> Self {
        Self {
            volume: AtomicU8::new(100),
            muted: AtomicBool::new(false),
        }
    }
}

impl Gain {
    /// 0 when muted, else the volume squared, which sounds more even than a straight line.
    pub fn factor(&self) -> f32 {
        if self.muted.load(Ordering::Relaxed) {
            return 0.0;
        }
        let v = f32::from(self.volume.load(Ordering::Relaxed)) / 100.0;
        v * v
    }
}

/// An s16le sample as an `f32` in -1..1.
fn sample(bytes: [u8; 2]) -> f32 {
    f32::from(i16::from_le_bytes(bytes)) / 32_768.0
}

/// Maps interleaved s16le stereo to `channels` per frame: left, right, then
/// silence; mono gets the average. Frames the PCM doesn't cover are silent.
fn fill<T: Copy>(pcm: &[u8], out: &mut [T], channels: usize, silence: T, conv: impl Fn(f32) -> T) {
    let mut chunks = pcm.as_chunks::<4>().0.iter();
    for frame in out.chunks_exact_mut(channels) {
        let (l, r) = match chunks.next() {
            Some(&[a, b, c, d]) => (sample([a, b]), sample([c, d])),
            None => (0.0, 0.0),
        };
        for (ch, slot) in frame.iter_mut().enumerate() {
            *slot = match (channels, ch) {
                (1, _) => conv((l + r) / 2.0),
                (_, 0) => conv(l),
                (_, 1) => conv(r),
                _ => silence,
            };
        }
    }
}

pub fn fill_f32(pcm: &[u8], out: &mut [f32], channels: usize, gain: f32) {
    fill(pcm, out, channels, 0.0, |s| s * gain);
}

pub fn fill_i16(pcm: &[u8], out: &mut [i16], channels: usize, gain: f32) {
    fill(pcm, out, channels, 0, |s| {
        (s * gain * 32_768.0).clamp(-32_768.0, 32_767.0) as i16
    });
}

/// The single speaker of This computer: a software volume.
pub struct LocalSpeakers {
    name: String,
    gain: Arc<Gain>,
}

impl LocalSpeakers {
    pub fn new(name: String, gain: Arc<Gain>) -> Self {
        Self { name, gain }
    }
}

#[async_trait]
impl SpeakerControl for LocalSpeakers {
    async fn list(&self) -> Result<Vec<Speaker>, SpeakerError> {
        Ok(vec![Speaker {
            id: "local".into(),
            name: self.name.clone(),
            volume: self.gain.volume.load(Ordering::Relaxed),
            muted: self.gain.muted.load(Ordering::Relaxed),
            connected: true,
        }])
    }

    async fn set(&self, id: &str, change: &SpeakerChange) -> Result<(), SpeakerError> {
        if id != "local" {
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
        if let Some(v) = change.volume {
            self.gain.volume.store(v, Ordering::Relaxed);
        }
        if let Some(m) = change.muted {
            self.gain.muted.store(m, Ordering::Relaxed);
        }
        Ok(())
    }
}

#[cfg(feature = "local-audio")]
use cpal::{
    SampleFormat, StreamConfig,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};

#[cfg(feature = "local-audio")]
type AudioStream = cpal::Stream;
#[cfg(not(feature = "local-audio"))]
struct AudioStream;

/// The default output device's config at 48 kHz in `f32` or `i16`, stereo first.
#[cfg(feature = "local-audio")]
fn pick_config(device: &cpal::Device) -> Result<(StreamConfig, SampleFormat), String> {
    let ranges = device
        .supported_output_configs()
        .map_err(|e| format!("can't read its formats: {e}"))?;
    let mut best: Option<(u8, StreamConfig, SampleFormat)> = None;
    for range in ranges {
        let format = range.sample_format();
        if !matches!(format, SampleFormat::F32 | SampleFormat::I16) {
            continue;
        }
        let Some(cfg) = range.try_with_sample_rate(48_000) else {
            continue;
        };
        // Prefer stereo, then f32.
        let score = u8::from(cfg.channels() == 2) * 2 + u8::from(format == SampleFormat::F32);
        if best.as_ref().is_none_or(|b| score > b.0) {
            best = Some((score, cfg.config(), format));
        }
    }
    best.map(|(_, c, f)| (c, f))
        .ok_or_else(|| "no 48 kHz f32 or i16 output format".to_string())
}

/// The default output device if it can play 48 kHz; logs why otherwise.
pub fn probe() -> Option<LocalDevice> {
    #[cfg(feature = "local-audio")]
    {
        let Some(device) = cpal::default_host().default_output_device() else {
            tracing::info!("this computer: no audio output device");
            return None;
        };
        let name = device.to_string();
        match pick_config(&device) {
            Ok(_) => {
                tracing::info!("this computer: {name}");
                Some(LocalDevice { name })
            }
            Err(why) => {
                tracing::info!("this computer: {name} isn't usable: {why}");
                None
            }
        }
    }
    #[cfg(not(feature = "local-audio"))]
    {
        None
    }
}

/// Pulls PCM for the sound card callback without allocating.
#[cfg(feature = "local-audio")]
struct Puller {
    queue: Arc<PcmQueue>,
    scratch: Vec<u8>,
}

#[cfg(feature = "local-audio")]
impl Puller {
    fn new(queue: Arc<PcmQueue>) -> Self {
        // Room for 8192 frames. The buffer only grows if a callback ever
        // asks for more, which is rare and acceptable.
        Self {
            queue,
            scratch: vec![0; 8_192 * 4],
        }
    }

    fn pull(&mut self, frames: usize) -> &[u8] {
        let need = frames * 4;
        if self.scratch.len() < need {
            self.scratch.resize(need, 0);
        }
        self.queue.pop_into(&mut self.scratch[..need]);
        &self.scratch[..need]
    }
}

/// Opens and starts the stream on the default device, pulling from `queue`.
#[cfg(feature = "local-audio")]
fn start_stream(queue: Arc<PcmQueue>, gain: Arc<Gain>) -> io::Result<AudioStream> {
    let device = cpal::default_host()
        .default_output_device()
        .ok_or_else(|| io::Error::other("no audio output device"))?;
    let (config, format) = pick_config(&device).map_err(io::Error::other)?;
    let channels = usize::from(config.channels);
    let on_error = {
        let queue = queue.clone();
        move |e: cpal::Error| {
            if is_fatal(&e) {
                queue.fail(format!("audio output: {e}"));
            } else {
                tracing::info!("local: audio output: {e}; still playing");
            }
        }
    };
    let mut puller = Puller::new(queue);
    let stream = if format == SampleFormat::F32 {
        device.build_output_stream(
            config,
            move |out: &mut [f32], _| {
                let pcm = puller.pull(out.len() / channels);
                fill_f32(pcm, out, channels, gain.factor());
            },
            on_error,
            None,
        )
    } else {
        device.build_output_stream(
            config,
            move |out: &mut [i16], _| {
                let pcm = puller.pull(out.len() / channels);
                fill_i16(pcm, out, channels, gain.factor());
            },
            on_error,
            None,
        )
    }
    .map_err(|e| io::Error::other(format!("can't open the sound card: {e}")))?;
    stream
        .play()
        .map_err(|e| io::Error::other(format!("can't start the sound card: {e}")))?;
    Ok(stream)
}

/// Whether a cpal error means the stream is dead. Xrun, DeviceChanged and
/// RealtimeDenied are informational: the stream keeps playing.
#[cfg(feature = "local-audio")]
fn is_fatal(e: &cpal::Error) -> bool {
    use cpal::ErrorKind;
    !matches!(
        e.kind(),
        ErrorKind::Xrun | ErrorKind::DeviceChanged | ErrorKind::RealtimeDenied
    )
}

#[cfg(not(feature = "local-audio"))]
fn start_stream(_queue: Arc<PcmQueue>, _gain: Arc<Gain>) -> io::Result<AudioStream> {
    Err(io::Error::other("built without local-audio"))
}

/// Dropping it stops the audio thread, which drops the stream.
struct StreamGuard(#[allow(dead_code)] std::sync::mpsc::Sender<()>);

/// Starts the audio thread and waits until the stream is playing or failed.
async fn open_stream(queue: Arc<PcmQueue>, gain: Arc<Gain>) -> io::Result<Box<dyn Send>> {
    let (ready_tx, ready_rx) = oneshot::channel();
    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
    std::thread::Builder::new()
        .name("local-audio".into())
        .spawn(move || match start_stream(queue, gain) {
            Ok(_stream) => {
                let _ = ready_tx.send(Ok(()));
                // Returns when the guard (the only sender) is dropped; the stream
                // then drops at the end of this arm.
                let _ = stop_rx.recv();
            }
            Err(e) => {
                let _ = ready_tx.send(Err(e));
            }
        })?;
    await_ready(ready_rx, OPEN_TIMEOUT)
        .await
        .map(|()| Box::new(StreamGuard(stop_tx)) as Box<dyn Send>)
}

/// How long the sound card gets to open before the writer reports the link down.
const OPEN_TIMEOUT: Duration = Duration::from_secs(2);

async fn await_ready(
    ready_rx: oneshot::Receiver<io::Result<()>>,
    wait: Duration,
) -> io::Result<()> {
    match tokio::time::timeout(wait, ready_rx).await {
        Ok(Ok(res)) => res,
        Ok(Err(_)) => Err(io::Error::other("audio thread ended")),
        Err(_) => Err(io::Error::other("the sound card did not open in time")),
    }
}

pub fn build_local(
    device: &LocalDevice,
    gain: Arc<Gain>,
    events: mpsc::UnboundedSender<EngineEvent>,
) -> Output {
    let queue = PcmQueue::new(Duration::from_millis(200), Duration::from_secs(2));
    let connector = QueueConnector {
        queue: queue.clone(),
        clear_on_close: true,
        target: device.name.clone(),
        open: Some(Arc::new({
            let queue = queue.clone();
            let gain = gain.clone();
            move || open_stream(queue.clone(), gain.clone()).boxed()
        })),
    };
    let engine = PacedEngine::spawn(
        PacedConfig {
            label: "local",
            connector: Arc::new(connector),
            max_ahead: Duration::from_secs(5),
            latency: Duration::from_millis(250),
            resend_on_resume: true,
        },
        events,
    );
    let link = engine.link();
    Output {
        engine: Box::new(engine),
        info: OutputInfo {
            id: "local".into(),
            kind: OutputKind::Local,
            name: "This computer".into(),
            address: None,
            hint: None,
        },
        speakers: Some(Arc::new(LocalSpeakers::new(device.name.clone(), gain))),
        link: Some(link),
        browser_player: None,
        stream: None,
        closed: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "local-audio")]
    #[test]
    fn only_dead_streams_are_fatal() {
        use cpal::{Error, ErrorKind};
        for kind in [
            ErrorKind::Xrun,
            ErrorKind::DeviceChanged,
            ErrorKind::RealtimeDenied,
        ] {
            assert!(!is_fatal(&Error::new(kind)), "{kind:?}");
        }
        for kind in [
            ErrorKind::DeviceNotAvailable,
            ErrorKind::StreamInvalidated,
            ErrorKind::HostUnavailable,
            ErrorKind::BackendError,
            ErrorKind::Other,
        ] {
            assert!(is_fatal(&Error::new(kind)), "{kind:?}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn open_that_hangs_times_out() {
        let (tx, rx) = oneshot::channel::<io::Result<()>>();
        let err = await_ready(rx, Duration::from_secs(2)).await.unwrap_err();
        assert!(err.to_string().contains("in time"));
        drop(tx);
    }

    fn stereo(frames: &[(i16, i16)]) -> Vec<u8> {
        frames
            .iter()
            .flat_map(|(l, r)| [l.to_le_bytes(), r.to_le_bytes()].concat())
            .collect()
    }

    #[test]
    fn fill_maps_stereo_mono_and_extra_channels() {
        let pcm = stereo(&[(16_384, -16_384), (32_768_i32 as i16, 0)]);
        let mut out = [9.0f32; 4];
        fill_f32(&pcm, &mut out, 2, 1.0);
        assert_eq!(out, [0.5, -0.5, -1.0, 0.0]);
        let mut out = [9.0f32; 6];
        fill_f32(&pcm, &mut out, 3, 1.0);
        assert_eq!(out, [0.5, -0.5, 0.0, -1.0, 0.0, 0.0]);
        let mut out = [9.0f32; 2];
        fill_f32(&stereo(&[(16_384, 0), (0, 16_384)]), &mut out, 1, 1.0);
        assert_eq!(out, [0.25, 0.25]);
        let mut out = [9.0f32; 4];
        fill_f32(&pcm, &mut out, 2, 0.5);
        assert_eq!(out, [0.25, -0.25, -0.5, 0.0]);
        let mut out = [9i16; 4];
        fill_i16(&pcm, &mut out, 2, 1.0);
        assert_eq!(out, [16_384, -16_384, -32_768, 0]);
        // PCM shorter than the buffer leaves silence.
        let mut out = [9.0f32; 4];
        fill_f32(&stereo(&[(16_384, 16_384)]), &mut out, 2, 1.0);
        assert_eq!(out, [0.5, 0.5, 0.0, 0.0]);
    }

    #[test]
    fn gain_curve_and_mute() {
        let g = Gain::default();
        assert_eq!(g.factor(), 1.0);
        g.volume.store(50, Ordering::Relaxed);
        assert_eq!(g.factor(), 0.25);
        g.muted.store(true, Ordering::Relaxed);
        assert_eq!(g.factor(), 0.0);
    }

    #[tokio::test]
    async fn local_speakers_list_and_set() {
        let gain = Arc::new(Gain::default());
        let s = LocalSpeakers::new("Built-in".into(), gain.clone());
        let list = s.list().await.unwrap();
        assert_eq!(
            list,
            [Speaker {
                id: "local".into(),
                name: "Built-in".into(),
                volume: 100,
                muted: false,
                connected: true
            }]
        );
        let change = |volume, muted| SpeakerChange { volume, muted };
        s.set("local", &change(Some(30), Some(true))).await.unwrap();
        let now = &s.list().await.unwrap()[0];
        assert_eq!((now.volume, now.muted), (30, true));
        assert_eq!(gain.factor(), 0.0);
        let err = |r: Result<(), SpeakerError>| match r {
            Err(SpeakerError::Rejected(code, _)) => code,
            other => panic!("{other:?}"),
        };
        assert_eq!(
            err(s.set("other", &change(Some(1), None)).await),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            err(s.set("local", &change(Some(101), None)).await),
            StatusCode::BAD_REQUEST
        );
    }

    #[cfg(not(feature = "local-audio"))]
    #[tokio::test]
    async fn open_fails_without_the_feature() {
        let q = PcmQueue::new(Duration::from_millis(200), Duration::from_secs(2));
        let e = open_stream(q, Arc::new(Gain::default())).await.err();
        assert_eq!(e.unwrap().to_string(), "built without local-audio");
    }

    /// Needs a real sound card: `cargo test --lib local -- --ignored`.
    #[cfg(feature = "local-audio")]
    #[tokio::test]
    #[ignore]
    async fn plays_a_tone_on_the_default_device() {
        use crate::source::PcmReader;

        struct Tone {
            left: usize,
            at: usize,
        }
        #[async_trait]
        impl PcmReader for Tone {
            async fn read(&mut self, buf: &mut [u8]) -> anyhow::Result<usize> {
                let n = buf.len().min(self.left) / 4 * 4;
                for frame in buf[..n].as_chunks_mut::<4>().0 {
                    let t = self.at as f32 / 48_000.0;
                    let s = ((t * 440.0 * std::f32::consts::TAU).sin() * 8_000.0) as i16;
                    frame[..2].copy_from_slice(&s.to_le_bytes());
                    frame[2..].copy_from_slice(&s.to_le_bytes());
                    self.at += 1;
                }
                self.left -= n;
                Ok(n)
            }
        }

        let device = probe().expect("no usable audio device");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut out = build_local(&device, Arc::new(Gain::default()), tx);
        out.engine
            .play(
                Box::new(Tone {
                    left: 48_000 * 4,
                    at: 0,
                }),
                1,
            )
            .unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_millis(2_500);
        while let Ok(Some(event)) = tokio::time::timeout_at(deadline, rx.recv()).await {
            assert!(
                !matches!(event, EngineEvent::TrackFailed { .. }),
                "{event:?}"
            );
        }
    }
}
