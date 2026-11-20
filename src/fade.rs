//! A PCM reader wrapper that fades the sound, for the sleep timer (out) and
//! the wake-up alarm (in).
//!
//! The player gives every reader it hands to an engine a [`Fade`] sharing one
//! [`FadeControl`]. While the control holds no window and no fade-in the
//! samples pass through untouched; during a window each sample is scaled by a
//! gain that falls linearly to zero at the window's end. A fade-in rises from
//! silence as `t * t`, where `t` is the share of its length already played;
//! it counts audio read, not wall time, and multiplies the sleep gain.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::time::Instant;

use crate::source::PcmReader;

/// How long the fade-out before a sleep timer's deadline lasts.
pub const FADE_LEN: Duration = Duration::from_secs(30);

/// Samples per second the readers produce: 48 kHz, stereo, s16le.
const SAMPLES_PER_SEC: u64 = 48_000 * 2;

#[derive(Clone, Copy, Debug)]
struct FadeIn {
    total: u64,
    done: u64,
}

#[derive(Debug, Default)]
struct State {
    window: Option<Window>,
    fade_in: Option<FadeIn>,
}

#[derive(Clone, Copy, Debug)]
struct Window {
    start: Instant,
    end: Instant,
    /// The gain at `start`.
    from: f32,
}

/// The fade the player has asked for, shared with every [`Fade`] reader.
#[derive(Clone, Debug, Default)]
pub struct FadeControl(Arc<Mutex<State>>);

impl FadeControl {
    /// Fades to silence at `deadline`: the last [`FADE_LEN`] ramp the gain
    /// down from 1. With less than that left, the ramp starts now from the
    /// current gain.
    pub fn start(&self, deadline: Instant, now: Instant) {
        let mut state = self.0.lock().unwrap();
        let window = &mut state.window;
        let current = gain_at(window.as_ref(), now);
        let ramp_start = deadline.checked_sub(FADE_LEN).unwrap_or(now);
        *window = Some(if ramp_start > now {
            Window {
                start: ramp_start,
                end: deadline,
                from: 1.0,
            }
        } else {
            Window {
                start: now,
                end: deadline,
                from: current,
            }
        });
    }

    /// Back to full volume (the sleep fade only; a fade-in is kept).
    pub fn clear(&self) {
        self.0.lock().unwrap().window = None;
    }

    /// The gain (0.0 to 1.0) at `now`.
    pub fn gain(&self, now: Instant) -> f32 {
        gain_at(self.0.lock().unwrap().window.as_ref(), now)
    }

    /// Starts a fade-in lasting `len` of audio: silent at the first sample,
    /// full volume `len` later. Replaces a running one; a zero `len` ends it.
    pub fn arm_fade_in(&self, len: Duration) {
        let total = (len.as_secs_f64() * SAMPLES_PER_SEC as f64) as u64;
        self.0.lock().unwrap().fade_in = (total > 0).then_some(FadeIn { total, done: 0 });
    }

    /// Full volume at once.
    pub fn end_fade_in(&self) {
        self.0.lock().unwrap().fade_in = None;
    }

    /// Whether a fade-in is armed or running.
    pub fn fade_in_active(&self) -> bool {
        self.0.lock().unwrap().fade_in.is_some()
    }

    /// Counts `samples` as played; returns the fade-in position before them
    /// (`done`, `total`), or `None` when there is no fade-in.
    fn advance_fade_in(&self, samples: u64) -> Option<(u64, u64)> {
        let mut state = self.0.lock().unwrap();
        let fade_in = state.fade_in.as_mut()?;
        let before = (fade_in.done, fade_in.total);
        fade_in.done += samples;
        if fade_in.done >= fade_in.total {
            state.fade_in = None;
        }
        Some(before)
    }
}

fn gain_at(window: Option<&Window>, now: Instant) -> f32 {
    let Some(w) = window else {
        return 1.0;
    };
    if now <= w.start {
        return w.from;
    }
    if now >= w.end {
        return 0.0;
    }
    let left = (w.end - now).as_secs_f32();
    let total = (w.end - w.start).as_secs_f32();
    w.from * left / total
}

/// Scales the samples of `inner` (s16le) by the gain in `control`.
pub struct Fade {
    inner: Box<dyn PcmReader>,
    control: FadeControl,
    /// The first byte of a sample whose second byte hasn't been read yet.
    carry: Option<u8>,
}

impl Fade {
    pub fn new(inner: Box<dyn PcmReader>, control: FadeControl) -> Self {
        Self {
            inner,
            control,
            carry: None,
        }
    }

    /// The wrapped reader (test engines look through the wrapper).
    pub fn inner(&self) -> &dyn PcmReader {
        &*self.inner
    }
}

#[async_trait]
impl PcmReader for Fade {
    async fn read(&mut self, buf: &mut [u8]) -> anyhow::Result<usize> {
        if buf.len() < 2 {
            return self.inner.read(buf).await;
        }
        loop {
            let lead = match self.carry {
                Some(byte) => {
                    buf[0] = byte;
                    1
                }
                None => 0,
            };
            let n = self.inner.read(&mut buf[lead..]).await?;
            self.carry = None;
            if n == 0 {
                // The song ended; a lone byte is half a sample, so drop it.
                return Ok(0);
            }
            let mut total = lead + n;
            if total % 2 == 1 {
                total -= 1;
                self.carry = Some(buf[total]);
            }
            if total == 0 {
                continue;
            }
            let gain = self.control.gain(Instant::now());
            let fade_in = self.control.advance_fade_in((total / 2) as u64);
            if gain < 1.0 || fade_in.is_some() {
                for (i, sample) in buf[..total].as_chunks_mut::<2>().0.iter_mut().enumerate() {
                    let mut gain = gain;
                    if let Some((done, len)) = fade_in {
                        let t = ((done + i as u64) as f32 / len as f32).min(1.0);
                        gain *= t * t;
                    }
                    let v = i16::from_le_bytes(*sample);
                    let scaled = (f32::from(v) * gain).round();
                    let scaled = scaled.clamp(f32::from(i16::MIN), f32::from(i16::MAX)) as i16;
                    *sample = scaled.to_le_bytes();
                }
            }
            return Ok(total);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;

    /// Hands out the given chunks, one per read.
    struct Script(VecDeque<Vec<u8>>);

    #[async_trait]
    impl PcmReader for Script {
        async fn read(&mut self, buf: &mut [u8]) -> anyhow::Result<usize> {
            let Some(chunk) = self.0.pop_front() else {
                return Ok(0);
            };
            buf[..chunk.len()].copy_from_slice(&chunk);
            Ok(chunk.len())
        }
    }

    fn samples(values: &[i16]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    fn fade_of(chunks: Vec<Vec<u8>>, control: &FadeControl) -> Fade {
        Fade::new(Box::new(Script(chunks.into())), control.clone())
    }

    async fn read_all(fade: &mut Fade) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        loop {
            let mut buf = [0u8; 64];
            let n = fade.read(&mut buf).await.unwrap();
            if n == 0 {
                return out;
            }
            out.push(buf[..n].to_vec());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn full_gain_passes_bytes_untouched() {
        let bytes = samples(&[1000, -1000, i16::MAX, i16::MIN]);
        let mut fade = fade_of(vec![bytes.clone()], &FadeControl::default());
        assert_eq!(read_all(&mut fade).await, vec![bytes]);
    }

    #[tokio::test(start_paused = true)]
    async fn half_gain_halves_each_sample() {
        let control = FadeControl::default();
        let now = Instant::now();
        // 30 s ramp, 15 s in: gain 0.5.
        control.start(now + FADE_LEN, now);
        tokio::time::advance(Duration::from_secs(15)).await;
        let mut fade = fade_of(vec![samples(&[1000, -1000, 20000, -20000])], &control);
        assert_eq!(
            read_all(&mut fade).await,
            vec![samples(&[500, -500, 10000, -10000])]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn odd_length_reads_keep_samples_aligned() {
        let control = FadeControl::default();
        let all = samples(&[1000, 2000, 3000]);
        // Three bytes, then three bytes: the split falls inside a sample.
        let mut fade = fade_of(vec![all[..3].to_vec(), all[3..].to_vec()], &control);
        let out = read_all(&mut fade).await;
        assert_eq!(out.concat(), all);
        assert!(out.iter().all(|chunk| chunk.len() % 2 == 0));
    }

    #[tokio::test(start_paused = true)]
    async fn odd_length_reads_are_scaled_per_sample() {
        let control = FadeControl::default();
        let now = Instant::now();
        control.start(now + FADE_LEN, now);
        tokio::time::advance(Duration::from_secs(15)).await;
        let all = samples(&[1000, 2000, 3000]);
        let mut fade = fade_of(
            vec![all[..1].to_vec(), all[1..4].to_vec(), all[4..].to_vec()],
            &control,
        );
        let out = read_all(&mut fade).await;
        assert_eq!(out.concat(), samples(&[500, 1000, 1500]));
    }

    #[test]
    fn control_ramps_linearly_and_clears() {
        let control = FadeControl::default();
        let now = Instant::now();
        assert_eq!(control.gain(now), 1.0);
        control.start(now + Duration::from_secs(60), now);
        assert_eq!(control.gain(now + Duration::from_secs(10)), 1.0);
        assert_eq!(control.gain(now + Duration::from_secs(30)), 1.0);
        let mid = control.gain(now + Duration::from_secs(45));
        assert!((mid - 0.5).abs() < 0.001, "{mid}");
        assert_eq!(control.gain(now + Duration::from_secs(60)), 0.0);
        control.clear();
        assert_eq!(control.gain(now + Duration::from_secs(60)), 1.0);
    }

    #[test]
    fn short_timer_starts_the_ramp_now() {
        let control = FadeControl::default();
        let now = Instant::now();
        control.start(now + Duration::from_secs(10), now);
        assert_eq!(control.gain(now), 1.0);
        let mid = control.gain(now + Duration::from_secs(5));
        assert!((mid - 0.5).abs() < 0.001, "{mid}");
    }

    #[test]
    fn replacing_a_timer_mid_fade_continues_from_the_current_gain() {
        let control = FadeControl::default();
        let now = Instant::now();
        control.start(now + FADE_LEN, now);
        let later = now + Duration::from_secs(15);
        control.start(later + Duration::from_secs(10), later);
        let g = control.gain(later);
        assert!((g - 0.5).abs() < 0.001, "{g}");
    }

    const SECOND: usize = SAMPLES_PER_SEC as usize;

    /// `n` samples of `value`, handed out as many as each read has room for.
    struct Tone(i16, usize);

    #[async_trait]
    impl PcmReader for Tone {
        async fn read(&mut self, buf: &mut [u8]) -> anyhow::Result<usize> {
            let n = (buf.len() / 2).min(self.1);
            for sample in buf[..n * 2].chunks_mut(2) {
                sample.copy_from_slice(&self.0.to_le_bytes());
            }
            self.1 -= n;
            Ok(n * 2)
        }
    }

    fn tone_fade(value: i16, n: usize, control: &FadeControl) -> Fade {
        Fade::new(Box::new(Tone(value, n)), control.clone())
    }

    fn values(bytes: &[u8]) -> Vec<i16> {
        bytes
            .chunks(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]))
            .collect()
    }

    async fn read_n(fade: &mut Fade, n_samples: usize) -> Vec<i16> {
        let mut out = Vec::new();
        while out.len() < n_samples {
            let mut buf = vec![0u8; ((n_samples - out.len()) * 2).min(4096)];
            let n = fade.read(&mut buf).await.unwrap();
            assert!(n > 0, "source ended early");
            out.extend(values(&buf[..n]));
        }
        out
    }

    #[tokio::test]
    async fn fade_in_starts_silent() {
        let control = FadeControl::default();
        control.arm_fade_in(Duration::from_secs(1));
        let mut fade = tone_fade(20000, SECOND, &control);
        let first = read_n(&mut fade, 4).await;
        assert_eq!(first[0], 0);
    }

    #[tokio::test]
    async fn fade_in_rises_monotonically_to_full() {
        let control = FadeControl::default();
        control.arm_fade_in(Duration::from_secs(1));
        let mut fade = tone_fade(20000, 2 * SECOND, &control);
        let mut last_peak = 0;
        let mut all = Vec::new();
        for _ in 0..(2 * SECOND / 1024) {
            let chunk = read_n(&mut fade, 1024).await;
            let peak = *chunk.iter().max().unwrap();
            assert!(peak >= last_peak, "{peak} < {last_peak}");
            last_peak = peak;
            all.extend(chunk);
        }
        assert!(all[SECOND..].iter().all(|v| *v == 20000));
        assert!(!control.fade_in_active());
    }

    #[tokio::test]
    async fn fade_in_quarter_point_is_quadratic() {
        let control = FadeControl::default();
        control.arm_fade_in(Duration::from_secs(1));
        let mut fade = tone_fade(20000, SECOND, &control);
        read_n(&mut fade, SECOND / 2).await;
        let at_half = read_n(&mut fade, 2).await[0];
        let g = f32::from(at_half) / 20000.0;
        assert!((g - 0.25).abs() < 0.01, "{g}");
    }

    #[tokio::test]
    async fn fade_in_continues_across_readers() {
        let control = FadeControl::default();
        control.arm_fade_in(Duration::from_secs(1));
        let mut first = tone_fade(20000, SECOND / 2, &control);
        read_n(&mut first, SECOND / 2).await;
        let mut second = tone_fade(20000, SECOND, &control);
        let head = read_n(&mut second, 2).await[0];
        let g = f32::from(head) / 20000.0;
        assert!((g - 0.25).abs() < 0.01, "restarted: {g}");
        let rest = read_n(&mut second, SECOND / 2 - 2).await;
        assert!(*rest.last().unwrap() > 19000);
    }

    #[tokio::test]
    async fn end_fade_in_is_full_gain_at_once() {
        let control = FadeControl::default();
        control.arm_fade_in(Duration::from_secs(1));
        control.end_fade_in();
        assert!(!control.fade_in_active());
        let mut fade = tone_fade(20000, 1024, &control);
        assert_eq!(read_n(&mut fade, 4).await[0], 20000);
    }

    #[tokio::test(start_paused = true)]
    async fn fade_in_and_sleep_fade_multiply() {
        let control = FadeControl::default();
        let now = Instant::now();
        control.start(now + FADE_LEN, now);
        tokio::time::advance(Duration::from_secs(15)).await;
        control.arm_fade_in(Duration::from_secs(1));
        let mut fade = tone_fade(20000, SECOND, &control);
        read_n(&mut fade, SECOND / 2).await;
        let v = read_n(&mut fade, 2).await[0];
        let g = f32::from(v) / 20000.0;
        assert!((g - 0.125).abs() < 0.01, "{g}");
    }

    #[test]
    fn clearing_the_sleep_fade_keeps_the_fade_in() {
        let control = FadeControl::default();
        control.arm_fade_in(Duration::from_secs(5));
        control.clear();
        assert!(control.fade_in_active());
    }
}
