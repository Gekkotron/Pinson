//! Crossfade: one PCM reader that plays a song and then blends into the next.
//!
//! A `Chain` is what the engine reads. It plays the current song, and the
//! player can `offer` it the next song's reader through a `ChainHandle`. Near
//! the real end of the current song the chain mixes the two (equal power), or,
//! with a fade of 0, joins them with no gap. Every song sits behind a
//! `Lookahead`: a background task that reads ahead, so the chain knows when
//! the song ends and how much is left.
//!
//! All audio is s16le, 48 kHz, stereo (4 bytes a frame).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{Semaphore, mpsc};
use tokio::task::JoinHandle;

use crate::source::PcmReader;

/// The crossfade lengths the user can pick, in seconds. 0 is off.
pub const CROSSFADE_CHOICES: [u32; 4] = [0, 3, 6, 12];
/// The longest crossfade.
pub const MAX_CROSSFADE: Duration = Duration::from_secs(12);
/// Most trailing silence cut from the end of a song.
pub const MAX_TRAILING_SILENCE: Duration = Duration::from_secs(8);
/// Most leading silence skipped at the start of the next song.
pub const MAX_LEADING_SILENCE: Duration = Duration::from_secs(5);

const SAMPLE_RATE: usize = 48_000;
const BYTES_PER_FRAME: usize = 4;
const BYTES_PER_SEC: usize = SAMPLE_RATE * BYTES_PER_FRAME;
/// A sample this quiet or quieter (about -60 dBFS) counts as silence.
const SILENCE: i16 = 32;
/// How much PCM each song may be read ahead: enough to see the end of the song
/// before the audible part drops to the longest fade, plus 2 s to spare.
const READ_AHEAD_BYTES: usize =
    (MAX_CROSSFADE.as_secs() + MAX_TRAILING_SILENCE.as_secs() + 2) as usize * BYTES_PER_SEC;
/// While more than this is buffered, the end of the song cannot be near.
const ZONE_BYTES: usize =
    (MAX_CROSSFADE.as_secs() + MAX_TRAILING_SILENCE.as_secs()) as usize * BYTES_PER_SEC;
/// Size of one read from the inner reader.
const CHUNK_BYTES: usize = 32 * 1024;
/// How long the chain waits for the read-ahead to tell it the end is near.
const END_WAIT: Duration = Duration::from_millis(100);

/// Something the chain tells the player.
#[derive(Clone, Debug, PartialEq)]
pub enum ChainEvent {
    /// The offered song `seq` started: its first frame is in what the engine
    /// just read.
    Started {
        generation: u64,
        seq: u64,
        /// How much the chain had output before the first mixed frame.
        stream_at: Duration,
        /// Leading silence skipped from the new song.
        lead: Duration,
    },
}

/// The length of `bytes` of PCM.
fn bytes_duration(bytes: u64) -> Duration {
    Duration::from_nanos(bytes * 1_000_000_000 / BYTES_PER_SEC as u64)
}

/// True when both samples of the 4-byte frame are quiet.
fn frame_silent(frame: &[u8]) -> bool {
    let l = i16::from_le_bytes([frame[0], frame[1]]);
    let r = i16::from_le_bytes([frame[2], frame[3]]);
    l.unsigned_abs() <= SILENCE as u16 && r.unsigned_abs() <= SILENCE as u16
}

enum Msg {
    Chunk(Vec<u8>),
    End,
    Error(anyhow::Error),
}

/// Reads `inner` ahead in a background task and keeps what it got, so the
/// song's remaining length and its tail can be looked at.
struct Lookahead {
    rx: mpsc::UnboundedReceiver<Msg>,
    /// Bytes of read-ahead the task may still queue; given back as the
    /// bytes are used.
    budget: Arc<Semaphore>,
    /// What has arrived and not been used yet.
    buf: VecDeque<u8>,
    /// Bytes used so far; tells where whole frames start in `buf`.
    consumed: u64,
    /// The inner reader hit its end.
    done: bool,
    /// The inner reader failed; the error waits in `error`.
    failed: bool,
    error: Option<anyhow::Error>,
    /// Quiet frames at the end of the song, once known.
    tail_silent: Option<usize>,
    task: JoinHandle<()>,
}

impl Lookahead {
    fn new(inner: Box<dyn PcmReader>) -> Lookahead {
        let (tx, rx) = mpsc::unbounded_channel();
        let budget = Arc::new(Semaphore::new(READ_AHEAD_BYTES));
        let task = tokio::spawn(read_ahead(inner, tx, budget.clone()));
        Lookahead {
            rx,
            budget,
            buf: VecDeque::new(),
            consumed: 0,
            done: false,
            failed: false,
            error: None,
            tail_silent: None,
            task,
        }
    }

    fn apply(&mut self, msg: Msg) {
        match msg {
            Msg::Chunk(chunk) => self.buf.extend(chunk),
            Msg::End => self.done = true,
            Msg::Error(e) => {
                self.failed = true;
                self.error = Some(e);
            }
        }
    }

    /// True once nothing more will arrive.
    fn finished(&self) -> bool {
        self.done || self.failed
    }

    /// Takes in everything the task has already sent.
    fn pull(&mut self) {
        while !self.finished() {
            match self.rx.try_recv() {
                Ok(msg) => self.apply(msg),
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => self.done = true,
            }
        }
    }

    /// Waits for the next message from the task.
    async fn recv_one(&mut self) {
        if self.finished() {
            return;
        }
        match self.rx.recv().await {
            Some(msg) => self.apply(msg),
            None => self.done = true,
        }
    }

    /// Like `recv_one`, but gives up after `END_WAIT`. True if a message came.
    async fn wait_more(&mut self) -> bool {
        if self.finished() {
            return false;
        }
        match tokio::time::timeout(END_WAIT, self.rx.recv()).await {
            Ok(Some(msg)) => {
                self.apply(msg);
                true
            }
            Ok(None) => {
                self.done = true;
                true
            }
            Err(_) => false,
        }
    }

    /// Uses `n` buffered bytes without reading them.
    fn discard(&mut self, n: usize) {
        self.buf.drain(..n);
        self.consumed += n as u64;
        self.budget.add_permits(n);
    }

    /// Moves up to `out.len()` buffered bytes into `out`.
    fn take(&mut self, out: &mut [u8]) -> usize {
        let n = out.len().min(self.buf.len());
        let (a, b) = self.buf.as_slices();
        let k = n.min(a.len());
        out[..k].copy_from_slice(&a[..k]);
        out[k..n].copy_from_slice(&b[..n - k]);
        self.discard(n);
        n
    }

    /// Waits until `n` bytes are buffered, or the song has ended. Fails if the
    /// reader failed before that.
    async fn ensure(&mut self, n: usize) -> anyhow::Result<()> {
        loop {
            self.pull();
            if self.buf.len() >= n || self.done {
                return Ok(());
            }
            if self.failed {
                return Err(self
                    .error
                    .take()
                    .unwrap_or_else(|| anyhow::anyhow!("incoming song failed")));
            }
            self.recv_one().await;
        }
    }

    /// Bytes from the read position to the end of the audible part: the end
    /// of the song without its trailing silence (at most
    /// `MAX_TRAILING_SILENCE`), whole frames only. Call once `done`.
    fn audible_bytes(&mut self) -> usize {
        let total = self.consumed + self.buf.len() as u64;
        let whole = total - total % BYTES_PER_FRAME as u64;
        let end = whole.saturating_sub(self.consumed) as usize;
        let first = ((BYTES_PER_FRAME as u64 - self.consumed % BYTES_PER_FRAME as u64)
            % BYTES_PER_FRAME as u64) as usize;
        let silent = match self.tail_silent {
            Some(s) => s,
            None => {
                let max = MAX_TRAILING_SILENCE.as_secs() as usize * SAMPLE_RATE;
                let buf = self.buf.make_contiguous();
                let mut idx = end;
                let mut n = 0;
                while n < max && idx >= first + BYTES_PER_FRAME && frame_silent(&buf[idx - 4..idx])
                {
                    idx -= BYTES_PER_FRAME;
                    n += 1;
                }
                self.tail_silent = Some(n);
                n
            }
        };
        end.saturating_sub(silent * BYTES_PER_FRAME)
    }

    /// Drops the quiet frames at the start of the song, at most
    /// `MAX_LEADING_SILENCE`, waiting for data as needed. Returns the bytes
    /// dropped.
    async fn skip_leading(&mut self) -> usize {
        let max = MAX_LEADING_SILENCE.as_secs() as usize * SAMPLE_RATE * BYTES_PER_FRAME;
        let mut skipped = 0;
        loop {
            self.pull();
            let buf = self.buf.make_contiguous();
            let mut i = 0;
            while skipped + i + BYTES_PER_FRAME <= max
                && i + BYTES_PER_FRAME <= buf.len()
                && frame_silent(&buf[i..i + BYTES_PER_FRAME])
            {
                i += BYTES_PER_FRAME;
            }
            self.discard(i);
            skipped += i;
            if skipped >= max || self.buf.len() >= BYTES_PER_FRAME || self.finished() {
                return skipped;
            }
            self.recv_one().await;
        }
    }
}

impl Drop for Lookahead {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// A song that was offered and then taken back can be used as a plain reader.
#[async_trait]
impl PcmReader for Lookahead {
    async fn read(&mut self, out: &mut [u8]) -> anyhow::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        loop {
            self.pull();
            if !self.buf.is_empty() {
                return Ok(self.take(out));
            }
            if let Some(e) = self.error.take() {
                return Err(e);
            }
            if self.finished() {
                return Ok(0);
            }
            self.recv_one().await;
        }
    }
}

/// Reads `inner` into whole-frame chunks, never more than the budget ahead.
async fn read_ahead(
    mut inner: Box<dyn PcmReader>,
    tx: mpsc::UnboundedSender<Msg>,
    budget: Arc<Semaphore>,
) {
    let mut buf = vec![0u8; CHUNK_BYTES];
    let mut carried = 0usize;
    loop {
        let n = match inner.read(&mut buf[carried..]).await {
            Ok(n) => n,
            Err(e) => {
                let _ = tx.send(Msg::Error(e));
                return;
            }
        };
        if n == 0 {
            if carried > 0 {
                // A trailing partial frame passes through untouched.
                let rest = buf[..carried].to_vec();
                let Ok(permit) = budget.acquire_many(rest.len() as u32).await else {
                    return;
                };
                permit.forget();
                let _ = tx.send(Msg::Chunk(rest));
            }
            let _ = tx.send(Msg::End);
            return;
        }
        let total = carried + n;
        let whole = total - total % BYTES_PER_FRAME;
        if whole > 0 {
            let Ok(permit) = budget.acquire_many(whole as u32).await else {
                return;
            };
            permit.forget();
            if tx.send(Msg::Chunk(buf[..whole].to_vec())).is_err() {
                return;
            }
        }
        buf.copy_within(whole..total, 0);
        carried = total - whole;
    }
}

/// An offered song, waiting for its turn.
struct Offer {
    reader: Lookahead,
    seq: u64,
    fade: Duration,
}

/// What the chain and its handle share.
#[derive(Default)]
struct Shared {
    /// The chain returned 0 or an error.
    ended: bool,
    offer: Option<Offer>,
    /// A mix is running.
    mixing: bool,
    /// The player took the offer back during the mix.
    cut: bool,
}

/// The player's side of a `Chain`.
#[derive(Clone)]
pub struct ChainHandle {
    shared: Arc<Mutex<Shared>>,
}

impl ChainHandle {
    /// Gives the chain the next song, to mix in over `fade` (0 joins with no
    /// gap). Gives `reader` back when the chain has already ended or already
    /// holds an offer.
    pub fn offer(
        &self,
        reader: Box<dyn PcmReader>,
        seq: u64,
        fade: Duration,
    ) -> Result<(), Box<dyn PcmReader>> {
        let mut shared = self.shared.lock().unwrap();
        if shared.ended || shared.offer.is_some() || shared.mixing {
            return Err(reader);
        }
        shared.offer = Some(Offer {
            reader: Lookahead::new(reader),
            seq,
            fade: fade.min(MAX_CROSSFADE),
        });
        Ok(())
    }

    /// Takes an offer back. Returns the song if its mix had not started. If
    /// the mix had started, the new song is cut, the old one carries on alone,
    /// and this returns `None`; so it does when there was no offer.
    pub fn withdraw(&self) -> Option<Box<dyn PcmReader>> {
        let mut shared = self.shared.lock().unwrap();
        if let Some(offer) = shared.offer.take() {
            return Some(Box::new(offer.reader));
        }
        if shared.mixing {
            shared.cut = true;
        }
        None
    }
}

/// A mix in progress.
struct Mix {
    inc: Lookahead,
    /// Frames the mix lasts.
    total: u64,
    done: u64,
}

/// Plays one song, then the offered ones, blending the joins.
pub struct Chain {
    generation: u64,
    events: mpsc::UnboundedSender<ChainEvent>,
    shared: Arc<Mutex<Shared>>,
    cur: Lookahead,
    mix: Option<Mix>,
    /// Mixed bytes that did not fit the last read.
    spill: VecDeque<u8>,
    /// Bytes handed out so far.
    out_bytes: u64,
    /// A read returned 0 or an error; later reads return 0.
    over: bool,
}

impl Chain {
    /// Starts a chain on `reader`. `generation` is passed through in events.
    pub fn new(
        reader: Box<dyn PcmReader>,
        generation: u64,
        events: mpsc::UnboundedSender<ChainEvent>,
    ) -> (Chain, ChainHandle) {
        let shared = Arc::new(Mutex::new(Shared::default()));
        let chain = Chain {
            generation,
            events,
            shared: shared.clone(),
            cur: Lookahead::new(reader),
            mix: None,
            spill: VecDeque::new(),
            out_bytes: 0,
            over: false,
        };
        (chain, ChainHandle { shared })
    }

    /// Tells the player the next song started here.
    fn announce(&self, seq: u64, lead: Duration) {
        let _ = self.events.send(ChainEvent::Started {
            generation: self.generation,
            seq,
            stream_at: bytes_duration(self.out_bytes),
            lead,
        });
    }

    /// Takes the offer if there is one; marks the mix as running when `mix`.
    fn take_offer(&self, mix: bool) -> Option<Offer> {
        let mut shared = self.shared.lock().unwrap();
        let offer = shared.offer.take()?;
        if mix {
            shared.mixing = true;
            shared.cut = false;
        }
        Some(offer)
    }

    /// The mix is over: the new song is the current one.
    fn finish_mix(&mut self) {
        if let Some(mix) = self.mix.take() {
            self.cur = mix.inc;
        }
        self.shared.lock().unwrap().mixing = false;
    }

    /// Begins the mix with the offered song. Returns what the first read of
    /// it gave, or `None` to look at the state again.
    async fn start_mix(&mut self, out: &mut [u8]) -> anyhow::Result<Option<usize>> {
        let Some(offer) = self.take_offer(true) else {
            return Ok(None);
        };
        let mut inc = offer.reader;
        let skipped = inc.skip_leading().await;
        let lead = bytes_duration(skipped as u64);
        let audible = (self.cur.audible_bytes() / BYTES_PER_FRAME) as u64;
        let total = audible.min((offer.fade.as_secs_f64() * SAMPLE_RATE as f64).round() as u64);
        tracing::info!(
            "crossfade: started seq={} fade={:.1}s lead={:.1}s",
            offer.seq,
            total as f32 / SAMPLE_RATE as f32,
            lead.as_secs_f32()
        );
        self.announce(offer.seq, lead);
        self.mix = Some(Mix {
            inc,
            total,
            done: 0,
        });
        if total == 0 {
            self.finish_mix();
            return Ok(None);
        }
        self.read_mix(out).await
    }

    /// Outputs the next stretch of the mix. `None` means the mix ended (or was
    /// cut) without output.
    async fn read_mix(&mut self, out: &mut [u8]) -> anyhow::Result<Option<usize>> {
        let cut = {
            let mut shared = self.shared.lock().unwrap();
            std::mem::take(&mut shared.cut)
        };
        if cut {
            self.mix = None;
            self.shared.lock().unwrap().mixing = false;
            return Ok(None);
        }
        let Some(mix) = self.mix.as_mut() else {
            return Ok(None);
        };
        let wanted = (out.len() / BYTES_PER_FRAME).max(1) as u64;
        let m = wanted.min(mix.total - mix.done) as usize;
        mix.inc.ensure(m * BYTES_PER_FRAME).await?;
        let mut cur_pcm = vec![0u8; m * BYTES_PER_FRAME];
        self.cur.take(&mut cur_pcm);
        let mut inc_pcm = vec![0u8; m * BYTES_PER_FRAME];
        let whole_inc = mix.inc.buf.len().min(inc_pcm.len()) / BYTES_PER_FRAME * BYTES_PER_FRAME;
        mix.inc.take(&mut inc_pcm[..whole_inc]);
        let mut mixed = vec![0u8; m * BYTES_PER_FRAME];
        for k in 0..m {
            let t = (mix.done + k as u64) as f32 / mix.total as f32;
            let (sin, cos) = (std::f32::consts::FRAC_PI_2 * t).sin_cos();
            for ch in 0..2 {
                let o = k * BYTES_PER_FRAME + ch * 2;
                let a = f32::from(i16::from_le_bytes([cur_pcm[o], cur_pcm[o + 1]]));
                let b = f32::from(i16::from_le_bytes([inc_pcm[o], inc_pcm[o + 1]]));
                let v = (a * cos + b * sin).round().clamp(-32768.0, 32767.0) as i16;
                mixed[o..o + 2].copy_from_slice(&v.to_le_bytes());
            }
        }
        mix.done += m as u64;
        let over = mix.done >= mix.total;
        let n = out.len().min(mixed.len());
        out[..n].copy_from_slice(&mixed[..n]);
        self.spill.extend(&mixed[n..]);
        if over {
            self.finish_mix();
        }
        Ok(Some(n))
    }

    async fn read_inner(&mut self, out: &mut [u8]) -> anyhow::Result<usize> {
        if !self.spill.is_empty() {
            let n = out.len().min(self.spill.len());
            for (slot, byte) in out.iter_mut().zip(self.spill.drain(..n)) {
                *slot = byte;
            }
            return Ok(n);
        }
        loop {
            if self.mix.is_some() {
                match self.read_mix(out).await? {
                    Some(n) => return Ok(n),
                    None => continue,
                }
            }
            self.cur.pull();
            let fade = self.shared.lock().unwrap().offer.as_ref().map(|o| o.fade);
            // The most bytes that may pass before the mix starts.
            let mut limit = usize::MAX;
            if let Some(fade) = fade.filter(|f| !f.is_zero()) {
                let fade_bytes =
                    (fade.as_secs_f64() * SAMPLE_RATE as f64).round() as usize * BYTES_PER_FRAME;
                if !self.cur.finished() {
                    if self.cur.buf.len() > ZONE_BYTES {
                        limit = self.cur.buf.len() - ZONE_BYTES;
                    } else if self.cur.wait_more().await {
                        // The end may be in sight now.
                        continue;
                    }
                } else if self.cur.done {
                    let audible = self.cur.audible_bytes();
                    if audible <= fade_bytes {
                        let phase = (self.cur.consumed % BYTES_PER_FRAME as u64) as usize;
                        if phase != 0 && audible > 0 {
                            // Finish the half frame so the mix is aligned.
                            let n = (BYTES_PER_FRAME - phase).min(out.len());
                            return Ok(self.cur.take(&mut out[..n]));
                        }
                        match self.start_mix(out).await? {
                            Some(n) => return Ok(n),
                            None => continue,
                        }
                    }
                    limit = audible - fade_bytes;
                }
            }
            if !self.cur.buf.is_empty() {
                let n = out.len().min(limit);
                return Ok(self.cur.take(&mut out[..n]));
            }
            if let Some(e) = self.cur.error.take() {
                return Err(e);
            }
            if !self.cur.done {
                self.cur.recv_one().await;
                continue;
            }
            // The current song is over.
            let join = {
                let mut shared = self.shared.lock().unwrap();
                match shared.offer.as_ref().map(|o| o.fade.is_zero()) {
                    None => {
                        shared.ended = true;
                        return Ok(0);
                    }
                    // A mix offer came in just now; the next pass starts it.
                    Some(false) => None,
                    Some(true) => shared.offer.take(),
                }
            };
            if let Some(offer) = join {
                tracing::info!("crossfade: gapless join seq={}", offer.seq);
                self.announce(offer.seq, Duration::ZERO);
                self.cur = offer.reader;
            }
        }
    }
}

#[async_trait]
impl PcmReader for Chain {
    async fn read(&mut self, out: &mut [u8]) -> anyhow::Result<usize> {
        if out.is_empty() || self.over {
            return Ok(0);
        }
        let result = self.read_inner(out).await;
        match &result {
            Ok(0) | Err(_) => {
                self.over = true;
                self.shared.lock().unwrap().ended = true;
            }
            Ok(n) => self.out_bytes += *n as u64,
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEC: usize = BYTES_PER_SEC;

    /// `frames` identical frames of `value`.
    fn pcm(frames: usize, value: i16) -> Vec<u8> {
        let mut out = Vec::with_capacity(frames * 4);
        for _ in 0..frames {
            out.extend_from_slice(&value.to_le_bytes());
            out.extend_from_slice(&value.to_le_bytes());
        }
        out
    }

    fn secs(s: f32, value: i16) -> Vec<u8> {
        pcm((s * SAMPLE_RATE as f32) as usize, value)
    }

    /// Hands out `data` in reads of at most `step` bytes, then ends or fails.
    struct Fake {
        data: Vec<u8>,
        pos: usize,
        step: usize,
        error_at_end: bool,
    }

    impl Fake {
        fn new(data: Vec<u8>) -> Box<Fake> {
            Box::new(Fake {
                data,
                pos: 0,
                step: 1001,
                error_at_end: false,
            })
        }
    }

    #[async_trait]
    impl PcmReader for Fake {
        async fn read(&mut self, buf: &mut [u8]) -> anyhow::Result<usize> {
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

    type Events = mpsc::UnboundedReceiver<ChainEvent>;

    fn chain(cur: Vec<u8>) -> (Chain, ChainHandle, Events) {
        let (tx, rx) = mpsc::unbounded_channel();
        let (chain, handle) = Chain::new(Fake::new(cur), 7, tx);
        (chain, handle, rx)
    }

    /// Reads to the end. Also returns each event with the bytes read before the
    /// read call that brought it.
    async fn read_all(
        chain: &mut Chain,
        events: &mut Events,
        size: usize,
    ) -> (Vec<u8>, Vec<(usize, ChainEvent)>) {
        let mut out = Vec::new();
        let mut seen = Vec::new();
        let mut buf = vec![0u8; size];
        loop {
            let before = out.len();
            let n = chain.read(&mut buf).await.expect("read");
            while let Ok(ev) = events.try_recv() {
                seen.push((before, ev));
            }
            if n == 0 {
                return (out, seen);
            }
            out.extend_from_slice(&buf[..n]);
        }
    }

    /// Reads until `bytes` have been read.
    async fn read_to(chain: &mut Chain, out: &mut Vec<u8>, bytes: usize, size: usize) {
        let mut buf = vec![0u8; size];
        while out.len() < bytes {
            let want = size.min(bytes - out.len());
            let n = chain.read(&mut buf[..want]).await.expect("read");
            assert!(n > 0, "ended early");
            out.extend_from_slice(&buf[..n]);
        }
    }

    fn sample_at(pcm: &[u8], frame: usize) -> i16 {
        i16::from_le_bytes([pcm[frame * 4], pcm[frame * 4 + 1]])
    }

    fn started(ev: &ChainEvent) -> (u64, u64, Duration, Duration) {
        let ChainEvent::Started {
            generation,
            seq,
            stream_at,
            lead,
        } = ev.clone();
        (generation, seq, stream_at, lead)
    }

    #[tokio::test]
    async fn pass_through_equals_input_and_ends() {
        let data = secs(3.0, 500);
        let (mut chain, _handle, mut events) = chain(data.clone());
        let (out, seen) = read_all(&mut chain, &mut events, 3001).await;
        assert_eq!(out, data);
        assert!(seen.is_empty());
        let mut buf = [0u8; 100];
        assert_eq!(chain.read(&mut buf).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn offer_after_end_returns_err() {
        let (mut chain, handle, mut events) = chain(secs(1.0, 500));
        read_all(&mut chain, &mut events, 3840).await;
        assert!(
            handle
                .offer(Fake::new(secs(1.0, 9)), 1, Duration::from_secs(3))
                .is_err()
        );
    }

    #[tokio::test]
    async fn mix_starts_fade_before_audible_end() {
        for size in [3840, 3001, 17_000] {
            let (mut chain, handle, mut events) = chain(secs(30.0, 1000));
            let fade = Duration::from_secs(6);
            handle
                .offer(Fake::new(secs(20.0, 2000)), 5, fade)
                .ok()
                .unwrap();
            let (out, seen) = read_all(&mut chain, &mut events, size).await;
            assert_eq!(seen.len(), 1);
            let (generation, seq, stream_at, lead) = started(&seen[0].1);
            assert_eq!((generation, seq, lead), (7, 5, Duration::ZERO));
            assert_eq!(stream_at, Duration::from_secs(24));
            // The event came with the read that output the first mixed frame.
            assert_eq!(seen[0].0, 24 * SEC);
            assert_eq!(out.len(), (30 + 20 - 6) * SEC);
            let start = 24 * SAMPLE_RATE;
            assert_eq!(sample_at(&out, start - 1), 1000);
            assert_eq!(sample_at(&out, start + 6 * SAMPLE_RATE), 2000);
            assert_eq!(sample_at(&out, out.len() / 4 - 1), 2000);
        }
    }

    #[tokio::test]
    async fn trailing_silence_is_cut() {
        let mut cur = secs(20.0, 1000);
        cur.extend(secs(2.0, 0));
        let (mut chain, handle, mut events) = chain(cur);
        handle
            .offer(Fake::new(secs(20.0, 2000)), 1, Duration::from_secs(6))
            .ok()
            .unwrap();
        let (out, seen) = read_all(&mut chain, &mut events, 3840).await;
        assert_eq!(started(&seen[0].1).2, Duration::from_secs(14));
        assert_eq!(out.len(), (20 + 20 - 6) * SEC);
    }

    #[tokio::test]
    async fn leading_silence_is_skipped() {
        let mut inc = secs(1.0, 0);
        inc.extend(secs(20.0, 2000));
        let (mut chain, handle, mut events) = chain(secs(20.0, 1000));
        handle
            .offer(Fake::new(inc), 1, Duration::from_secs(6))
            .ok()
            .unwrap();
        let (out, seen) = read_all(&mut chain, &mut events, 3840).await;
        let (_, _, stream_at, lead) = started(&seen[0].1);
        assert_eq!(lead, Duration::from_secs(1));
        assert_eq!(stream_at, Duration::from_secs(14));
        assert_eq!(out.len(), (20 + 20 - 6) * SEC);
    }

    #[tokio::test]
    async fn gapless_join_is_exact() {
        let mut cur = secs(10.0, 1000);
        cur.extend(secs(1.0, 0));
        let mut inc = secs(1.0, 0);
        inc.extend(secs(10.0, 2000));
        let (mut chain, handle, mut events) = chain(cur.clone());
        handle
            .offer(Fake::new(inc.clone()), 3, Duration::ZERO)
            .ok()
            .unwrap();
        let (out, seen) = read_all(&mut chain, &mut events, 3001).await;
        let expected: Vec<u8> = cur.iter().chain(inc.iter()).copied().collect();
        assert_eq!(out, expected);
        let (_, seq, stream_at, lead) = started(&seen[0].1);
        assert_eq!((seq, lead), (3, Duration::ZERO));
        assert_eq!(stream_at, bytes_duration(cur.len() as u64));
        assert_eq!(seen[0].0, cur.len());
    }

    #[tokio::test]
    async fn late_offer_mixes_the_rest() {
        let (mut chain, handle, mut events) = chain(secs(30.0, 1000));
        let mut out = Vec::new();
        read_to(&mut chain, &mut out, 25 * SEC, 3840).await;
        handle
            .offer(Fake::new(secs(20.0, 2000)), 2, Duration::from_secs(12))
            .ok()
            .unwrap();
        let (rest, seen) = read_all(&mut chain, &mut events, 3840).await;
        out.extend(rest);
        let (_, _, stream_at, _) = started(&seen[0].1);
        assert_eq!(stream_at, Duration::from_secs(25));
        assert_eq!(out.len(), (30 + 20 - 5) * SEC);
        assert_eq!(sample_at(&out, 30 * SAMPLE_RATE - 1), 2000);
    }

    #[tokio::test]
    async fn withdraw_before_the_mix_returns_the_reader() {
        let (mut chain, handle, mut events) = chain(secs(30.0, 1000));
        let inc = secs(2.0, 2000);
        handle
            .offer(Fake::new(inc.clone()), 1, Duration::from_secs(6))
            .ok()
            .unwrap();
        let mut reader = handle.withdraw().expect("reader back");
        let mut got = Vec::new();
        let mut buf = vec![0u8; 5000];
        loop {
            let n = reader.read(&mut buf).await.unwrap();
            if n == 0 {
                break;
            }
            got.extend_from_slice(&buf[..n]);
        }
        assert_eq!(got, inc);
        assert!(handle.withdraw().is_none());
        let (out, seen) = read_all(&mut chain, &mut events, 3840).await;
        assert_eq!(out, secs(30.0, 1000));
        assert!(seen.is_empty());
        // The slot is free again.
        let (mut chain2, handle2, _events2) = self::chain(secs(1.0, 1));
        assert!(
            handle2
                .offer(Fake::new(secs(1.0, 2)), 1, Duration::ZERO)
                .is_ok()
        );
        let _ = &mut chain2;
    }

    #[tokio::test]
    async fn withdraw_during_the_mix_cuts_the_incoming() {
        let (mut chain, handle, mut events) = chain(secs(30.0, 1000));
        handle
            .offer(Fake::new(secs(20.0, 2000)), 1, Duration::from_secs(6))
            .ok()
            .unwrap();
        let mut out = Vec::new();
        read_to(&mut chain, &mut out, 25 * SEC, 3840).await;
        assert!(events.try_recv().is_ok());
        assert!(handle.withdraw().is_none());
        let (rest, _) = read_all(&mut chain, &mut events, 3840).await;
        out.extend(rest);
        assert_eq!(out.len(), 30 * SEC);
        assert_eq!(sample_at(&out, 30 * SAMPLE_RATE - 1), 1000);
        // Nothing more can be offered: the chain has ended.
        assert!(
            handle
                .offer(Fake::new(secs(1.0, 2)), 2, Duration::ZERO)
                .is_err()
        );
    }

    #[tokio::test]
    async fn second_offer_is_rejected() {
        let (_chain, handle, _events) = chain(secs(10.0, 1000));
        assert!(
            handle
                .offer(Fake::new(secs(1.0, 2)), 1, Duration::ZERO)
                .is_ok()
        );
        assert!(
            handle
                .offer(Fake::new(secs(1.0, 3)), 2, Duration::ZERO)
                .is_err()
        );
    }

    #[tokio::test]
    async fn chaining_twice_works() {
        let fade = Duration::from_secs(3);
        let (mut chain, handle, mut events) = chain(secs(20.0, 1000));
        handle
            .offer(Fake::new(secs(20.0, 2000)), 1, fade)
            .ok()
            .unwrap();
        let mut out = Vec::new();
        let mut all = Vec::new();
        read_to(&mut chain, &mut out, 17 * SEC + 5 * SEC, 3840).await;
        while let Ok(ev) = events.try_recv() {
            all.push(ev);
        }
        handle
            .offer(Fake::new(secs(20.0, 3000)), 2, fade)
            .ok()
            .unwrap();
        let (rest, seen) = read_all(&mut chain, &mut events, 3840).await;
        out.extend(rest);
        all.extend(seen.into_iter().map(|(_, e)| e));
        assert_eq!(all.len(), 2);
        assert_eq!(started(&all[0]).2, Duration::from_secs(17));
        assert_eq!(started(&all[1]).2, Duration::from_secs(34));
        assert_eq!(out.len(), (60 - 6) * SEC);
        assert_eq!(sample_at(&out, out.len() / 4 - 1), 3000);
    }

    #[tokio::test]
    async fn incoming_error_during_the_mix_is_an_error() {
        let (mut chain, handle, _events) = chain(secs(20.0, 1000));
        let mut bad = Fake::new(secs(2.0, 2000));
        bad.error_at_end = true;
        handle.offer(bad, 1, Duration::from_secs(6)).ok().unwrap();
        let mut buf = vec![0u8; 3840];
        let failed = loop {
            match chain.read(&mut buf).await {
                Ok(0) => break false,
                Ok(_) => {}
                Err(_) => break true,
            }
        };
        assert!(failed);
        assert_eq!(chain.read(&mut buf).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn mix_is_equal_power() {
        let (mut chain, handle, mut events) = chain(secs(20.0, 10_000));
        handle
            .offer(Fake::new(secs(20.0, 6000)), 1, Duration::from_secs(6))
            .ok()
            .unwrap();
        let (out, _) = read_all(&mut chain, &mut events, 3840).await;
        let mid = 14 * SAMPLE_RATE + 3 * SAMPLE_RATE;
        let expected = std::f32::consts::FRAC_1_SQRT_2 * 16_000.0;
        let got = f32::from(sample_at(&out, mid));
        assert!((got - expected).abs() < 5.0, "got {got}, want {expected}");
    }
}
