//! Loudness normalisation: one gain per song, measured ahead of playback.
//!
//! `Leveled` wraps a PCM reader (s16le, 48 kHz, stereo). A background task
//! reads the inner reader as fast as it delivers and measures ITU-R BS.1770-4
//! loudness; the consumer plays the audio with a gain that brings the song to
//! the target loudness.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{Semaphore, mpsc, watch};
use tokio::task::JoinHandle;

use crate::source::PcmReader;

const SAMPLE_RATE: usize = 48_000;
const BYTES_PER_FRAME: usize = 4;
const BYTES_PER_SEC: usize = SAMPLE_RATE * BYTES_PER_FRAME;
/// Frames in one 100 ms sub-block.
const SUB_BLOCK_FRAMES: usize = SAMPLE_RATE / 10;
const SUB_BLOCK: Duration = Duration::from_millis(100);
/// Sub-blocks in one 400 ms gating block.
const BLOCK_SUBS: usize = 4;
const ABSOLUTE_GATE_LUFS: f64 = -70.0;
const RELATIVE_GATE_LU: f64 = 10.0;

/// How much audio the read-ahead task may keep queued ahead of the consumer.
const READ_AHEAD: Duration = Duration::from_secs(120);
const CHUNK_BYTES: usize = 64 * 1024;
/// `Leveled::start` waits for this much audio measured by the reader itself...
const START_MEASURED: Duration = Duration::from_secs(30);
/// ...but not longer than this.
const START_WAIT: Duration = Duration::from_millis(250);

const MIN_GAIN_DB: f32 = -20.0;
const MAX_GAIN_DB: f32 = 10.0;
/// The boosted peak stays at or below this level.
const PEAK_CEILING_DBFS: f32 = -1.0;
/// How fast the applied gain follows the target.
const MAX_GAIN_SLEW_DB_PER_SEC: f32 = 6.0;

/// The gain, in dB, that brings a song measured at `integrated` LUFS to
/// `target_lufs`: clamped to -20..+10 dB and capped so that `peak` (linear,
/// 0..1) stays at or below -1 dBFS. 0 dB while nothing is measured.
pub fn gain_db(target_lufs: f32, integrated: Option<f32>, peak: f32) -> f32 {
    let Some(integrated) = integrated else {
        return 0.0;
    };
    let gain = (target_lufs - integrated).clamp(MIN_GAIN_DB, MAX_GAIN_DB);
    if peak > 0.0 {
        gain.min(PEAK_CEILING_DBFS - 20.0 * peak.log10())
    } else {
        gain
    }
}

fn lufs(energy: f64) -> f64 {
    -0.691 + 10.0 * energy.log10()
}

/// What is known about one song's loudness.
#[derive(Debug, Default)]
pub struct SongLevel {
    /// Summed channel mean-square energy of each 100 ms sub-block, by
    /// position in the song.
    sub_blocks: Vec<Option<f32>>,
    peak: f32,
}

impl SongLevel {
    fn set(&mut self, index: usize, energy: f32) {
        if self.sub_blocks.len() <= index {
            self.sub_blocks.resize(index + 1, None);
        }
        self.sub_blocks[index] = Some(energy);
    }

    pub fn peak(&self) -> f32 {
        self.peak
    }

    /// Integrated loudness (BS.1770-4 gating), or `None` while no block
    /// passes the gates.
    pub fn integrated_lufs(&self) -> Option<f32> {
        let blocks: Vec<f64> = self
            .sub_blocks
            .windows(BLOCK_SUBS)
            .filter_map(|w| {
                let mut sum = 0.0f64;
                for e in w {
                    sum += f64::from((*e)?);
                }
                Some(sum / BLOCK_SUBS as f64)
            })
            .filter(|&z| z > 0.0 && lufs(z) > ABSOLUTE_GATE_LUFS)
            .collect();
        if blocks.is_empty() {
            return None;
        }
        let mean = |zs: &[f64]| zs.iter().sum::<f64>() / zs.len() as f64;
        let threshold = lufs(mean(&blocks)) - RELATIVE_GATE_LU;
        let kept: Vec<f64> = blocks
            .into_iter()
            .filter(|&z| lufs(z) > threshold)
            .collect();
        if kept.is_empty() {
            return None;
        }
        Some(lufs(mean(&kept)) as f32)
    }

    /// How much audio has been measured.
    pub fn measured(&self) -> Duration {
        let present = self.sub_blocks.iter().filter(|e| e.is_some()).count();
        SUB_BLOCK * present as u32
    }
}

/// Per-song records, by video id. Holds at most `CAPACITY` songs.
#[derive(Clone, Default)]
pub struct LevelCache {
    inner: Arc<Mutex<CacheInner>>,
}

#[derive(Default)]
struct CacheInner {
    songs: HashMap<String, Arc<Mutex<SongLevel>>>,
    order: VecDeque<String>,
}

impl LevelCache {
    const CAPACITY: usize = 256;

    /// The record of `video_id`, created empty on a miss.
    pub fn get(&self, video_id: &str) -> Arc<Mutex<SongLevel>> {
        let mut inner = self.inner.lock().unwrap();
        if let Some(level) = inner.songs.get(video_id) {
            return level.clone();
        }
        while inner.order.len() >= Self::CAPACITY {
            if let Some(oldest) = inner.order.pop_front() {
                inner.songs.remove(&oldest);
            }
        }
        let level = Arc::new(Mutex::new(SongLevel::default()));
        inner.songs.insert(video_id.to_owned(), level.clone());
        inner.order.push_back(video_id.to_owned());
        level
    }
}

/// The loudness target and the records of the songs measured so far.
#[derive(Clone)]
pub struct Loudness {
    pub target_lufs: f32,
    pub cache: LevelCache,
}

#[derive(Clone, Copy, Default)]
struct Biquad {
    b: [f64; 3],
    a: [f64; 3],
    z1: f64,
    z2: f64,
}

impl Biquad {
    fn new(b: [f64; 3], a: [f64; 3]) -> Self {
        Self {
            b,
            a,
            z1: 0.0,
            z2: 0.0,
        }
    }

    fn process(&mut self, x: f64) -> f64 {
        let y = self.b[0] * x + self.z1;
        self.z1 = self.b[1] * x - self.a[1] * y + self.z2;
        self.z2 = self.b[2] * x - self.a[2] * y;
        y
    }
}

/// K-weighting (BS.1770-4, 48 kHz coefficients) and 100 ms energy sub-blocks.
struct Meter {
    /// Per channel: shelf, then high-pass.
    filters: [[Biquad; 2]; 2],
    sum: f64,
    frames: usize,
    next_index: usize,
}

impl Meter {
    /// A meter whose first sub-block is song position `start`.
    fn new(start: Duration) -> Self {
        let channel = [
            Biquad::new(
                [1.53512485958697, -2.69169618940638, 1.19839281085285],
                [1.0, -1.69065929318241, 0.73248077421585],
            ),
            Biquad::new([1.0, -2.0, 1.0], [1.0, -1.99004745483398, 0.99007225036621]),
        ];
        Self {
            filters: [channel, channel],
            sum: 0.0,
            frames: 0,
            next_index: (start.as_secs_f64() / SUB_BLOCK.as_secs_f64()).round() as usize,
        }
    }

    /// Measures whole frames of s16le stereo into `level`.
    fn push(&mut self, pcm: &[u8], level: &mut SongLevel) {
        let mut peak = level.peak;
        for frame in pcm.as_chunks::<BYTES_PER_FRAME>().0 {
            for (ch, filters) in self.filters.iter_mut().enumerate() {
                let s = i16::from_le_bytes([frame[2 * ch], frame[2 * ch + 1]]);
                let x = f64::from(s) / 32768.0;
                peak = peak.max(x.abs() as f32);
                let shelved = filters[0].process(x);
                let y = filters[1].process(shelved);
                self.sum += y * y;
            }
            self.frames += 1;
            if self.frames == SUB_BLOCK_FRAMES {
                level.set(self.next_index, (self.sum / SUB_BLOCK_FRAMES as f64) as f32);
                self.next_index += 1;
                self.sum = 0.0;
                self.frames = 0;
            }
        }
        level.peak = peak;
    }
}

enum Message {
    Chunk(Vec<u8>),
    Error(anyhow::Error),
}

#[derive(Clone, Copy, Default)]
struct Progress {
    /// Frames measured by this reader.
    frames: u64,
    done: bool,
}

/// A `PcmReader` that plays its inner reader at the song's own gain.
pub struct Leveled {
    rx: mpsc::UnboundedReceiver<Message>,
    /// Bytes of read-ahead the task may still queue.
    budget: Arc<Semaphore>,
    level: Arc<Mutex<SongLevel>>,
    target_lufs: f32,
    current: Vec<u8>,
    pos: usize,
    gain_db: Option<f32>,
    #[cfg(test)]
    progress: watch::Receiver<Progress>,
    task: JoinHandle<()>,
}

impl Leveled {
    /// Starts measuring `inner`, which is the song from `start` on, into
    /// `level`. Returns once 30 s have been measured, the song has ended, or
    /// 250 ms have passed.
    pub async fn start(
        inner: Box<dyn PcmReader>,
        level: Arc<Mutex<SongLevel>>,
        start: Duration,
        target_lufs: f32,
    ) -> Leveled {
        let (tx, rx) = mpsc::unbounded_channel();
        let budget = Arc::new(Semaphore::new(
            READ_AHEAD.as_secs() as usize * BYTES_PER_SEC,
        ));
        let (progress_tx, mut progress) = watch::channel(Progress::default());
        let task = tokio::spawn(read_ahead(
            inner,
            level.clone(),
            start,
            tx,
            budget.clone(),
            progress_tx,
        ));
        let wanted = START_MEASURED.as_secs() * SAMPLE_RATE as u64;
        let _ = tokio::time::timeout(
            START_WAIT,
            progress.wait_for(|p| p.done || p.frames >= wanted),
        )
        .await;
        Leveled {
            rx,
            budget,
            level,
            target_lufs,
            current: Vec::new(),
            pos: 0,
            gain_db: None,
            #[cfg(test)]
            progress: progress.clone(),
            task,
        }
    }

    /// Logs what is known about the song now and the gain it gets.
    pub fn log_start(&self, video_id: &str) {
        let (integrated, peak, measured) = {
            let level = self.level.lock().unwrap();
            (level.integrated_lufs(), level.peak(), level.measured())
        };
        let gain = gain_db(self.target_lufs, integrated, peak);
        match integrated {
            Some(l) => tracing::info!(
                "{video_id}: {l:.1} LUFS ({:.0} s), gain {gain:+.1} dB",
                measured.as_secs_f32()
            ),
            None => tracing::info!(
                "{video_id}: no loudness yet ({:.0} s), gain {gain:+.1} dB",
                measured.as_secs_f32()
            ),
        }
    }

    /// Takes the next queued chunk and applies the gain to it.
    fn load(&mut self, mut chunk: Vec<u8>) {
        self.budget.add_permits(chunk.len());
        let (integrated, peak) = {
            let level = self.level.lock().unwrap();
            (level.integrated_lufs(), level.peak())
        };
        let target = gain_db(self.target_lufs, integrated, peak);
        let frames = chunk.len() / BYTES_PER_FRAME;
        let from = self.gain_db.unwrap_or(target);
        let max_step = MAX_GAIN_SLEW_DB_PER_SEC * frames as f32 / SAMPLE_RATE as f32;
        let to = from + (target - from).clamp(-max_step, max_step);
        self.gain_db = Some(to);
        if from != 0.0 || to != 0.0 {
            apply_gain(&mut chunk[..frames * BYTES_PER_FRAME], from, to);
        }
        self.current = chunk;
        self.pos = 0;
    }

    #[cfg(test)]
    async fn wait_done(&mut self) {
        let _ = self.progress.wait_for(|p| p.done).await;
    }
}

impl Drop for Leveled {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Multiplies whole frames by a gain moving linearly from `from_db` to `to_db`.
fn apply_gain(pcm: &mut [u8], from_db: f32, to_db: f32) {
    let from = 10f32.powf(from_db / 20.0);
    let to = 10f32.powf(to_db / 20.0);
    let frames = (pcm.len() / BYTES_PER_FRAME).max(1) as f32;
    for (i, frame) in pcm
        .as_chunks_mut::<BYTES_PER_FRAME>()
        .0
        .iter_mut()
        .enumerate()
    {
        let gain = from + (to - from) * (i as f32 / frames);
        for sample in frame.as_chunks_mut::<2>().0 {
            let s = i16::from_le_bytes([sample[0], sample[1]]);
            let v = (f32::from(s) * gain).round().clamp(-32768.0, 32767.0) as i16;
            sample.copy_from_slice(&v.to_le_bytes());
        }
    }
}

async fn read_ahead(
    mut inner: Box<dyn PcmReader>,
    level: Arc<Mutex<SongLevel>>,
    start: Duration,
    tx: mpsc::UnboundedSender<Message>,
    budget: Arc<Semaphore>,
    progress: watch::Sender<Progress>,
) {
    let mut meter = Meter::new(start);
    let mut buf = vec![0u8; CHUNK_BYTES];
    let mut carried = 0usize;
    let mut frames = 0u64;
    loop {
        let n = match inner.read(&mut buf[carried..]).await {
            Ok(n) => n,
            Err(e) => {
                let _ = tx.send(Message::Error(e));
                break;
            }
        };
        if n == 0 {
            if carried > 0 {
                // A trailing partial frame passes through untouched.
                let rest = buf[..carried].to_vec();
                if let Ok(permit) = budget.acquire_many(rest.len() as u32).await {
                    permit.forget();
                    let _ = tx.send(Message::Chunk(rest));
                }
            }
            break;
        }
        let total = carried + n;
        let whole = total - total % BYTES_PER_FRAME;
        if whole > 0 {
            meter.push(&buf[..whole], &mut level.lock().unwrap());
            frames += (whole / BYTES_PER_FRAME) as u64;
            let Ok(permit) = budget.acquire_many(whole as u32).await else {
                return;
            };
            permit.forget();
            if tx.send(Message::Chunk(buf[..whole].to_vec())).is_err() {
                return;
            }
            progress.send_replace(Progress {
                frames,
                done: false,
            });
        }
        buf.copy_within(whole..total, 0);
        carried = total - whole;
    }
    progress.send_replace(Progress { frames, done: true });
}

#[async_trait]
impl PcmReader for Leveled {
    async fn read(&mut self, buf: &mut [u8]) -> anyhow::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        while self.pos >= self.current.len() {
            match self.rx.recv().await {
                Some(Message::Chunk(chunk)) => self.load(chunk),
                Some(Message::Error(e)) => return Err(e),
                None => return Ok(0),
            }
        }
        let n = buf.len().min(self.current.len() - self.pos);
        buf[..n].copy_from_slice(&self.current[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn sine(amplitude: f32, seconds: f32) -> Vec<u8> {
        let frames = (seconds * SAMPLE_RATE as f32) as usize;
        let mut pcm = Vec::with_capacity(frames * BYTES_PER_FRAME);
        for i in 0..frames {
            let t = i as f32 / SAMPLE_RATE as f32;
            let s = (amplitude * (2.0 * std::f32::consts::PI * 1000.0 * t).sin() * 32768.0).round()
                as i16;
            pcm.extend_from_slice(&s.to_le_bytes());
            pcm.extend_from_slice(&s.to_le_bytes());
        }
        pcm
    }

    fn measure(pcm: &[u8]) -> Option<f32> {
        let mut level = SongLevel::default();
        Meter::new(Duration::ZERO).push(pcm, &mut level);
        level.integrated_lufs()
    }

    /// Hands out `data` in reads of at most `step` bytes, then ends or fails.
    struct Fake {
        data: Vec<u8>,
        pos: usize,
        step: usize,
        error_at_end: bool,
        delay: Option<Duration>,
        dropped: Option<Arc<AtomicBool>>,
    }

    impl Fake {
        fn new(data: Vec<u8>, step: usize) -> Self {
            Self {
                data,
                pos: 0,
                step,
                error_at_end: false,
                delay: None,
                dropped: None,
            }
        }
    }

    impl Drop for Fake {
        fn drop(&mut self) {
            if let Some(flag) = &self.dropped {
                flag.store(true, Ordering::SeqCst);
            }
        }
    }

    #[async_trait]
    impl PcmReader for Fake {
        async fn read(&mut self, buf: &mut [u8]) -> anyhow::Result<usize> {
            if let Some(delay) = self.delay {
                tokio::time::sleep(delay).await;
            }
            if self.pos >= self.data.len() {
                if self.error_at_end {
                    anyhow::bail!("boom");
                }
                return Ok(0);
            }
            let n = buf.len().min(self.step).min(self.data.len() - self.pos);
            buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }
    }

    async fn read_all(reader: &mut Leveled, read_size: usize) -> Vec<u8> {
        let mut out = Vec::new();
        let mut buf = vec![0u8; read_size];
        loop {
            let n = reader.read(&mut buf).await.expect("read");
            if n == 0 {
                return out;
            }
            out.extend_from_slice(&buf[..n]);
        }
    }

    fn samples(pcm: &[u8]) -> Vec<i16> {
        pcm.as_chunks::<2>()
            .0
            .iter()
            .map(|b| i16::from_le_bytes([b[0], b[1]]))
            .collect()
    }

    async fn leveled(fake: Fake, level: &Arc<Mutex<SongLevel>>, start: Duration) -> Leveled {
        Leveled::start(Box::new(fake), level.clone(), start, -14.0).await
    }

    #[test]
    fn sine_reads_minus_20_lufs() {
        let l = measure(&sine(0.1, 5.0)).expect("measured");
        assert!((l + 20.0).abs() < 0.3, "{l}");
    }

    #[test]
    fn silence_is_gated_out() {
        let mut pcm = vec![0u8; 10 * BYTES_PER_SEC];
        pcm.extend(sine(0.1, 10.0));
        let l = measure(&pcm).expect("measured");
        assert!((l + 20.0).abs() < 0.3, "{l}");
        assert_eq!(measure(&vec![0u8; 5 * BYTES_PER_SEC]), None);
    }

    #[test]
    fn measured_counts_present_sub_blocks() {
        let mut level = SongLevel::default();
        Meter::new(Duration::from_secs(2)).push(&sine(0.1, 1.0), &mut level);
        assert_eq!(level.measured(), Duration::from_secs(1));
        assert!(level.sub_blocks[..20].iter().all(|e| e.is_none()));
        assert!(level.sub_blocks[20..30].iter().all(|e| e.is_some()));
    }

    #[test]
    fn gain_clamps_and_caps_peak() {
        assert_eq!(gain_db(-14.0, None, 0.5), 0.0);
        assert!((gain_db(-14.0, Some(-20.0), 0.1) - 6.0).abs() < 1e-4);
        assert!((gain_db(-14.0, Some(-6.0), 0.1) + 8.0).abs() < 1e-4);
        assert_eq!(gain_db(-14.0, Some(-50.0), 0.001), 10.0);
        assert_eq!(gain_db(-14.0, Some(10.0), 0.001), -20.0);
        // Peak at -3 dBFS: boost limited to +2 dB (peak ends at -1 dBFS).
        let peak = 10f32.powf(-3.0 / 20.0);
        assert!((gain_db(-14.0, Some(-20.0), peak) - 2.0).abs() < 1e-3);
        // A louder-than-target song is cut even with a full-scale peak.
        assert!(gain_db(-14.0, Some(-6.0), 1.0) < -7.0);
    }

    #[test]
    fn cache_reuses_and_evicts_oldest() {
        let cache = LevelCache::default();
        let a = cache.get("a");
        assert!(Arc::ptr_eq(&a, &cache.get("a")));
        for i in 0..LevelCache::CAPACITY {
            cache.get(&format!("s{i}"));
        }
        assert!(!Arc::ptr_eq(&a, &cache.get("a")), "a was evicted");
        assert!(cache.inner.lock().unwrap().songs.len() <= LevelCache::CAPACITY);
    }

    #[tokio::test]
    async fn output_is_input_times_gain() {
        let input = sine(0.1, 35.0);
        let level = Arc::new(Mutex::new(SongLevel::default()));
        let mut reader = leveled(Fake::new(input.clone(), 10_000), &level, Duration::ZERO).await;
        reader.wait_done().await;
        let gain = {
            let l = level.lock().unwrap();
            gain_db(-14.0, l.integrated_lufs(), l.peak())
        };
        assert!((gain - 6.0).abs() < 0.3, "{gain}");
        let k = 10f32.powf(gain / 20.0);
        let out = read_all(&mut reader, 4096).await;
        assert_eq!(out.len(), input.len());
        for (o, i) in samples(&out).into_iter().zip(samples(&input)) {
            let expected = (f32::from(i) * k).round();
            assert!((f32::from(o) - expected).abs() <= 1.0, "{o} vs {expected}");
        }
    }

    #[tokio::test]
    async fn odd_reads_stay_aligned_and_lossless_at_0_db() {
        // A level at the target and headroom to spare: exactly 0 dB.
        let input = sine(0.2, 3.0);
        let level = Arc::new(Mutex::new(SongLevel::default()));
        // Pin the target to the measured loudness.
        let measured = measure(&input).unwrap();
        let mut reader = Leveled::start(
            Box::new(Fake::new(input.clone(), 3_001)),
            level.clone(),
            Duration::ZERO,
            measured,
        )
        .await;
        reader.wait_done().await;
        let mut out = Vec::new();
        let mut buf = vec![0u8; 7];
        let mut sizes = 0usize;
        loop {
            let n = reader.read(&mut buf).await.unwrap();
            if n == 0 {
                break;
            }
            sizes += 1;
            out.extend_from_slice(&buf[..n]);
        }
        assert!(sizes > 1000);
        assert_eq!(out, input, "gain 0 dB is bit-exact");
    }

    #[tokio::test]
    async fn trailing_partial_frame_passes_through() {
        let mut input = sine(0.1, 1.0);
        input.extend_from_slice(&[1, 2, 3]);
        let level = Arc::new(Mutex::new(SongLevel::default()));
        let mut reader = leveled(Fake::new(input.clone(), 1000), &level, Duration::ZERO).await;
        let out = read_all(&mut reader, 4096).await;
        assert_eq!(out.len(), input.len());
        assert_eq!(&out[out.len() - 3..], &[1, 2, 3]);
        assert_eq!(reader.read(&mut [0u8; 16]).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn error_comes_after_the_audio() {
        let input = sine(0.1, 1.0);
        let mut fake = Fake::new(input.clone(), 5000);
        fake.error_at_end = true;
        let level = Arc::new(Mutex::new(SongLevel::default()));
        let mut reader = leveled(fake, &level, Duration::ZERO).await;
        let mut got = 0usize;
        let mut buf = vec![0u8; 4096];
        let err = loop {
            match reader.read(&mut buf).await {
                Ok(0) => panic!("ended without the error"),
                Ok(n) => got += n,
                Err(e) => break e,
            }
        };
        assert_eq!(got, input.len());
        assert!(err.to_string().contains("boom"));
    }

    #[tokio::test(start_paused = true)]
    async fn start_returns_by_the_deadline_when_the_source_is_slow() {
        let mut fake = Fake::new(sine(0.1, 60.0), 192_000);
        fake.delay = Some(Duration::from_secs(1));
        let level = Arc::new(Mutex::new(SongLevel::default()));
        let began = tokio::time::Instant::now();
        let reader = leveled(fake, &level, Duration::ZERO).await;
        assert!(began.elapsed() <= START_WAIT + Duration::from_millis(50));
        drop(reader);
    }

    #[tokio::test]
    async fn start_returns_at_eof_without_waiting() {
        let level = Arc::new(Mutex::new(SongLevel::default()));
        let began = std::time::Instant::now();
        let _reader = leveled(Fake::new(sine(0.1, 2.0), 50_000), &level, Duration::ZERO).await;
        assert!(began.elapsed() < Duration::from_millis(200));
        assert_eq!(level.lock().unwrap().measured(), Duration::from_secs(2));
    }

    #[tokio::test]
    async fn drop_drops_the_inner_reader() {
        let flag = Arc::new(AtomicBool::new(false));
        let mut fake = Fake::new(sine(0.1, 60.0), 192_000);
        fake.delay = Some(Duration::from_secs(1));
        fake.dropped = Some(flag.clone());
        let level = Arc::new(Mutex::new(SongLevel::default()));
        let reader = leveled(fake, &level, Duration::ZERO).await;
        assert!(!flag.load(Ordering::SeqCst));
        drop(reader);
        for _ in 0..50 {
            if flag.load(Ordering::SeqCst) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("inner reader not dropped");
    }

    #[tokio::test]
    async fn open_at_a_later_start_reuses_the_record() {
        // 5 s quiet, then 35 s loud.
        let mut song = sine(0.01, 5.0);
        song.extend(sine(0.2, 35.0));
        let level = Arc::new(Mutex::new(SongLevel::default()));

        let mut first = leveled(Fake::new(song.clone(), 20_000), &level, Duration::ZERO).await;
        first.wait_done().await;
        let first_out = read_all(&mut first, 1 << 20).await;

        // Reopened 30 s in: only the loud tail comes from the inner reader.
        let tail_from = 30 * BYTES_PER_SEC;
        let tail = song[tail_from..].to_vec();
        let mut second = leveled(
            Fake::new(tail.clone(), 20_000),
            &level,
            Duration::from_secs(30),
        )
        .await;
        second.wait_done().await;
        let second_out = read_all(&mut second, 1 << 20).await;

        let ratio = |out: &[u8], input: &[u8], from: usize| {
            let o = samples(&out[from..from + 40_000])
                .iter()
                .map(|s| s.abs())
                .max()
                .unwrap();
            let i = samples(&input[from..from + 40_000])
                .iter()
                .map(|s| s.abs())
                .max()
                .unwrap();
            f32::from(o) / f32::from(i)
        };
        // Gains are compared in the loud part: the first open's gain there is
        // the record's gain.
        let loud_first = ratio(&first_out, &song, 6 * BYTES_PER_SEC);
        let loud_second = ratio(&second_out, &tail, 0);
        assert!(
            (loud_first / loud_second - 1.0).abs() < 0.03,
            "{loud_first} vs {loud_second}"
        );
    }

    #[test]
    fn meter_matches_ffmpeg_ebur128() {
        let dir = std::env::temp_dir().join(format!("pinson-loudness-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let wav = dir.join("noise.wav");
        let generated = Command::new("ffmpeg")
            .args(["-nostdin", "-loglevel", "error", "-y", "-f", "lavfi", "-i"])
            .arg(
                "anoisesrc=d=6:c=pink:a=0.2:seed=1[a];anoisesrc=d=6:c=white:a=0.02:seed=2[b];[a][b]concat=n=2:v=0:a=1",
            )
            .args(["-ac", "2", "-ar", "48000", "-c:a", "pcm_s16le"])
            .arg(&wav)
            .status();
        if !matches!(generated, Ok(s) if s.success()) {
            eprintln!("ffmpeg not available: skipping");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        let pcm = Command::new("ffmpeg")
            .args(["-nostdin", "-loglevel", "error", "-i"])
            .arg(&wav)
            .args(["-f", "s16le", "-ar", "48000", "-ac", "2", "-"])
            .output()
            .expect("ffmpeg decode");
        let ebur = Command::new("ffmpeg")
            .args(["-nostdin", "-nostats", "-i"])
            .arg(&wav)
            .args(["-af", "ebur128", "-f", "null", "-"])
            .output()
            .expect("ffmpeg ebur128");
        let _ = std::fs::remove_dir_all(&dir);
        let stderr = String::from_utf8_lossy(&ebur.stderr);
        let summary = stderr.rsplit("Integrated loudness:").next().unwrap();
        let reference: f32 = summary
            .lines()
            .find_map(|l| l.trim().strip_prefix("I:"))
            .and_then(|v| v.trim().trim_end_matches("LUFS").trim().parse().ok())
            .unwrap_or_else(|| panic!("no ebur128 summary in: {stderr}"));
        let ours = measure(&pcm.stdout).expect("measured");
        assert!((ours - reference).abs() < 0.5, "{ours} vs {reference}");
    }
}
