//! 48 kHz -> 44.1 kHz stereo s16 resampler (ratio 147/160).
//!
//! Windowed-sinc polyphase FIR: 147 phases of 32 taps, Kaiser window, each
//! phase normalised to unity DC gain. State carries across calls.

const PHASES: usize = 147;
const STEP: usize = 160;
const TAPS: usize = 32;
/// Taps before the interpolation point.
const LEAD: usize = TAPS / 2 - 1;
/// Low-pass cutoff in Hz at the 48 kHz input rate.
const CUTOFF_HZ: f64 = 20_000.0;
const KAISER_BETA: f64 = 7.0;

fn bessel_i0(x: f64) -> f64 {
    let mut sum = 1.0;
    let mut term = 1.0;
    let q = x * x / 4.0;
    for k in 1..50 {
        term *= q / (k * k) as f64;
        sum += term;
    }
    sum
}

fn build_taps() -> Vec<[f32; TAPS]> {
    let fc = CUTOFF_HZ / 48_000.0; // cycles per input sample
    let half = TAPS as f64 / 2.0;
    let norm = bessel_i0(KAISER_BETA);
    (0..PHASES)
        .map(|p| {
            let frac = p as f64 / PHASES as f64;
            let mut h = [0f64; TAPS];
            for (k, slot) in h.iter_mut().enumerate() {
                let t = k as f64 - LEAD as f64 - frac;
                let x = 2.0 * std::f64::consts::PI * fc * t;
                let sinc = if x.abs() < 1e-12 { 1.0 } else { x.sin() / x };
                let r = t / half;
                let w = if r.abs() >= 1.0 {
                    0.0
                } else {
                    bessel_i0(KAISER_BETA * (1.0 - r * r).sqrt()) / norm
                };
                *slot = sinc * w;
            }
            let sum: f64 = h.iter().sum();
            let mut out = [0f32; TAPS];
            for (o, v) in out.iter_mut().zip(h) {
                *o = (v / sum) as f32;
            }
            out
        })
        .collect()
}

pub struct Resampler {
    taps: Vec<[f32; TAPS]>,
    /// Buffered input frames (left, right).
    buf: Vec<[f32; 2]>,
    /// Index in `buf` of the input frame at the next output's base position.
    pos: usize,
    /// Fractional position numerator, 0..PHASES.
    phase: usize,
}

impl Default for Resampler {
    fn default() -> Self {
        Self::new()
    }
}

impl Resampler {
    pub fn new() -> Resampler {
        Resampler {
            taps: build_taps(),
            buf: vec![[0.0; 2]; LEAD],
            pos: LEAD,
            phase: 0,
        }
    }

    pub fn reset(&mut self) {
        self.buf.clear();
        self.buf.resize(LEAD, [0.0; 2]);
        self.pos = LEAD;
        self.phase = 0;
    }

    /// Appends the resampled interleaved stereo samples to `out`.
    pub fn process(&mut self, input_le: &[u8], out: &mut Vec<i16>) {
        for f in input_le.as_chunks::<4>().0 {
            self.buf.push([
                f32::from(i16::from_le_bytes([f[0], f[1]])),
                f32::from(i16::from_le_bytes([f[2], f[3]])),
            ]);
        }
        while self.pos + TAPS - LEAD <= self.buf.len() {
            let taps = &self.taps[self.phase];
            let window = &self.buf[self.pos - LEAD..self.pos - LEAD + TAPS];
            let (mut l, mut r) = (0f32, 0f32);
            for (h, s) in taps.iter().zip(window) {
                l += h * s[0];
                r += h * s[1];
            }
            out.push(l.round().clamp(-32768.0, 32767.0) as i16);
            out.push(r.round().clamp(-32768.0, 32767.0) as i16);
            self.phase += STEP;
            self.pos += self.phase / PHASES;
            self.phase %= PHASES;
        }
        let drop = self.pos - LEAD;
        self.buf.drain(..drop);
        self.pos -= drop;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine_bytes(freq: f64, amp: f64, frames: usize) -> Vec<u8> {
        let mut v = Vec::with_capacity(frames * 4);
        for n in 0..frames {
            let s = (amp * (2.0 * std::f64::consts::PI * freq * n as f64 / 48_000.0).sin()) as i16;
            v.extend_from_slice(&s.to_le_bytes());
            v.extend_from_slice(&s.to_le_bytes());
        }
        v
    }

    fn left(out: &[i16]) -> Vec<f64> {
        out.chunks(2).map(|c| c[0] as f64).collect()
    }

    fn rms(x: &[f64]) -> f64 {
        (x.iter().map(|v| v * v).sum::<f64>() / x.len() as f64).sqrt()
    }

    #[test]
    fn length_ratio_is_exact() {
        let input = sine_bytes(1000.0, 8000.0, 160_000);
        let mut r = Resampler::new();
        let mut out = Vec::new();
        let mut pos = 0;
        let mut size = 1;
        while pos < input.len() {
            let end = (pos + size * 4).min(input.len());
            r.process(&input[pos..end], &mut out);
            pos = end;
            size = size % 997 + 13;
        }
        let frames = out.len() / 2;
        assert!(frames.abs_diff(147_000) <= 32, "got {frames}");
    }

    #[test]
    fn chunking_does_not_change_output() {
        let input = sine_bytes(3000.0, 9000.0, 5000);
        let mut a = Vec::new();
        Resampler::new().process(&input, &mut a);
        let mut r = Resampler::new();
        let mut b = Vec::new();
        for c in input.chunks(37 * 4) {
            r.process(c, &mut b);
        }
        assert_eq!(a, b);
    }

    #[test]
    fn sine_keeps_frequency_and_level() {
        let input = sine_bytes(1000.0, 10_000.0, 48_000);
        let mut out = Vec::new();
        Resampler::new().process(&input, &mut out);
        let l = left(&out);
        let l = &l[64..];
        let crossings = l.windows(2).filter(|w| w[0] < 0.0 && w[1] >= 0.0).count();
        let secs = l.len() as f64 / 44_100.0;
        let freq = crossings as f64 / secs;
        assert!((freq - 1000.0).abs() <= 5.0, "freq {freq}");
        let in_rms = 10_000.0 / 2f64.sqrt();
        let db = 20.0 * (rms(l) / in_rms).log10();
        assert!(db.abs() <= 0.5, "level off by {db} dB");
    }

    #[test]
    fn aliasing_is_suppressed() {
        let input = sine_bytes(23_000.0, 10_000.0, 48_000);
        let mut out = Vec::new();
        Resampler::new().process(&input, &mut out);
        let l = left(&out);
        let l = &l[64..];
        let db = 20.0 * (rms(l) / (10_000.0 / 2f64.sqrt())).log10();
        assert!(db <= -40.0, "only {db} dB down");
    }
}
