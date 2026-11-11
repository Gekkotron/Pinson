//! RAOP wire formats and crypto helpers.

use std::sync::LazyLock;
use std::time::{SystemTime, UNIX_EPOCH};

use aws_lc_rs::cipher::{AES_128, EncryptingKey, EncryptionContext, UnboundCipherKey};
use aws_lc_rs::rsa::{OAEP_SHA1_MGF1SHA1, OaepPublicEncryptingKey, PublicEncryptingKey};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
use rand::RngCore;

pub const FRAMES_PER_PACKET: usize = 352;
pub const SAMPLE_RATE: u32 = 44_100;

const NTP_UNIX_OFFSET: u64 = 2_208_988_800;

/// The AirPort Express RSA public key (SubjectPublicKeyInfo, base64). It is
/// public and widely published; every RAOP receiver accepts it.
const APPLE_RAOP_KEY_B64: &str = "MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEA59dE8qLieItsH1WgjrcFRKj6eUWqi+bGLOX1HL3U3GhC/j0Qg90u3sG/1CUtwC5vOYvfDmFI6oSFXi5ELabWJmT2dKHzBJKa3k9ok+8t9ucRqMd6DZHJ2YCCLlDRKSKv6kDqnw4UwPdpOMXziC/AMj3Z/lUVX1G7WSHCAWKf1zNS1eLvqr+boEjXuBOitnZ/bDzPHrTOZz0Dew0uowxf/+sG+NCK3eQJVxqcaJ/vEHKIVd2M+5qL71yJQ+87X6oV3eaYvt3zWZYD6z5vYTcrtij2VZ9Zmni/UAaHqn9JdsBWLUEpVviYnhimNVvYFZeCXg/IdTQ+x4IRdiXNv5hEewIDAQAB";

static APPLE_RAOP_KEY_DER: LazyLock<Vec<u8>> = LazyLock::new(|| {
    STANDARD
        .decode(APPLE_RAOP_KEY_B64)
        .expect("valid base64 constant")
});

/// An NTP timestamp: seconds since 1900 and a 32-bit binary fraction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ntp {
    pub secs: u32,
    pub frac: u32,
}

impl Ntp {
    pub fn from_system(t: SystemTime) -> Ntp {
        let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
        Ntp {
            secs: (d.as_secs() + NTP_UNIX_OFFSET) as u32,
            frac: ((u64::from(d.subsec_nanos()) << 32) / 1_000_000_000) as u32,
        }
    }

    pub fn now() -> Ntp {
        Ntp::from_system(SystemTime::now())
    }

    pub fn to_bytes(self) -> [u8; 8] {
        let mut b = [0u8; 8];
        b[..4].copy_from_slice(&self.secs.to_be_bytes());
        b[4..].copy_from_slice(&self.frac.to_be_bytes());
        b
    }
}

struct BitWriter {
    out: Vec<u8>,
    acc: u64,
    bits: u32,
}

impl BitWriter {
    fn put(&mut self, value: u32, n: u32) {
        self.acc = (self.acc << n) | u64::from(value & ((1u64 << n) - 1) as u32);
        self.bits += n;
        while self.bits >= 8 {
            self.bits -= 8;
            self.out.push((self.acc >> self.bits) as u8);
        }
        self.acc &= (1u64 << self.bits) - 1;
    }
}

/// One uncompressed ALAC frame from interleaved stereo samples.
pub fn alac_raw(frames: &[i16]) -> Vec<u8> {
    let mut w = BitWriter {
        out: Vec::with_capacity(frames.len() * 2 + 8),
        acc: 0,
        bits: 0,
    };
    w.put(1, 3); // stereo
    w.put(0, 4);
    w.put(0, 12);
    w.put(1, 1); // has size
    w.put(0, 2);
    w.put(1, 1); // not compressed
    w.put((frames.len() / 2) as u32, 32);
    for &s in frames {
        w.put(u32::from(s as u16), 16);
    }
    w.put(7, 3); // END tag
    if w.bits > 0 {
        let pad = 8 - w.bits;
        w.put(0, pad);
    }
    w.out
}

/// Inverse of [`alac_raw`], for tests and the fake receiver.
#[cfg(test)]
pub(crate) fn alac_raw_decode(b: &[u8]) -> Option<Vec<i16>> {
    let mut pos = 0usize;
    let mut get = |n: usize| -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            let byte = *b.get(pos / 8)?;
            v = (v << 1) | u32::from((byte >> (7 - pos % 8)) & 1);
            pos += 1;
        }
        Some(v)
    };
    if get(3)? != 1 || get(4)? != 0 || get(12)? != 0 || get(1)? != 1 || get(2)? != 0 {
        return None;
    }
    if get(1)? != 1 {
        return None;
    }
    let n = get(32)? as usize;
    let mut out = Vec::with_capacity(n * 2);
    for _ in 0..n * 2 {
        out.push(get(16)? as u16 as i16);
    }
    (get(3)? == 7).then_some(out)
}

pub fn audio_packet(first: bool, seq: u16, ts: u32, ssrc: u32, payload: &[u8]) -> Vec<u8> {
    let mut p = Vec::with_capacity(12 + payload.len());
    p.push(0x80);
    p.push(if first { 0xe0 } else { 0x60 });
    p.extend_from_slice(&seq.to_be_bytes());
    p.extend_from_slice(&ts.to_be_bytes());
    p.extend_from_slice(&ssrc.to_be_bytes());
    p.extend_from_slice(payload);
    p
}

pub fn sync_packet(first: bool, now_ts: u32, latency_frames: u32, ntp: Ntp) -> [u8; 20] {
    let mut p = [0u8; 20];
    p[0] = if first { 0x90 } else { 0x80 };
    p[1] = 0xd4;
    p[3] = 0x07;
    p[4..8].copy_from_slice(&now_ts.wrapping_sub(latency_frames).to_be_bytes());
    p[8..16].copy_from_slice(&ntp.to_bytes());
    p[16..20].copy_from_slice(&now_ts.to_be_bytes());
    p
}

pub fn timing_reply(req: &[u8], received: Ntp, sent: Ntp) -> Option<[u8; 32]> {
    if req.len() < 32 || req[1] & 0x7f != 0x52 {
        return None;
    }
    let mut p = [0u8; 32];
    p[..4].copy_from_slice(&[0x80, 0xd3, 0x00, 0x07]);
    p[8..16].copy_from_slice(&req[24..32]);
    p[16..24].copy_from_slice(&received.to_bytes());
    p[24..32].copy_from_slice(&sent.to_bytes());
    Some(p)
}

/// `(first_seq, count)` of a retransmit request.
pub fn parse_resend(b: &[u8]) -> Option<(u16, u16)> {
    if b.len() < 8 || b[1] & 0x7f != 0x55 {
        return None;
    }
    Some((
        u16::from_be_bytes([b[4], b[5]]),
        u16::from_be_bytes([b[6], b[7]]),
    ))
}

pub fn resend_reply(original_packet: &[u8]) -> Vec<u8> {
    let mut p = Vec::with_capacity(4 + original_packet.len());
    p.extend_from_slice(&[0x80, 0xd6, 0x00, 0x01]);
    p.extend_from_slice(original_packet);
    p
}

/// RAOP volume in dB: 0 at full, -30 at the bottom, -144 for silence.
pub fn volume_db(level: f32, muted: bool) -> f32 {
    let level = level.clamp(0.0, 1.0);
    if muted || level <= 0.0 {
        -144.0
    } else {
        -30.0 * (1.0 - level)
    }
}

/// Per-session AES-128-CBC key and IV.
pub struct Cipher {
    key: [u8; 16],
    iv: [u8; 16],
}

impl Cipher {
    pub fn random() -> Cipher {
        let mut key = [0u8; 16];
        let mut iv = [0u8; 16];
        let mut rng = rand::rng();
        rng.fill_bytes(&mut key);
        rng.fill_bytes(&mut iv);
        Cipher { key, iv }
    }

    pub fn new(key: [u8; 16], iv: [u8; 16]) -> Cipher {
        Cipher { key, iv }
    }

    /// Encrypts the leading whole 16-byte blocks, restarting from the IV
    /// each packet; the shorter tail stays clear.
    pub fn encrypt(&self, payload: &mut [u8]) {
        let whole = payload.len() - payload.len() % 16;
        if whole == 0 {
            return;
        }
        let key = UnboundCipherKey::new(&AES_128, &self.key).expect("16-byte AES key");
        let enc = EncryptingKey::cbc(key).expect("AES-CBC key");
        enc.less_safe_encrypt(
            &mut payload[..whole],
            EncryptionContext::Iv128(self.iv.into()),
        )
        .expect("whole blocks");
    }

    pub fn aesiv(&self) -> String {
        STANDARD_NO_PAD.encode(self.iv)
    }

    pub fn rsaaeskey(&self) -> String {
        STANDARD_NO_PAD.encode(wrap_key(&APPLE_RAOP_KEY_DER, &self.key))
    }
}

/// RSA-OAEP (SHA-1) wrap of `key` for a SubjectPublicKeyInfo DER public key.
pub fn wrap_key(spki_der: &[u8], key: &[u8; 16]) -> Vec<u8> {
    let public = PublicEncryptingKey::from_der(spki_der).expect("valid RSA public key");
    let oaep = OaepPublicEncryptingKey::new(public).expect("supported RSA key size");
    let mut out = vec![0u8; oaep.ciphertext_size()];
    let n = oaep
        .encrypt(&OAEP_SHA1_MGF1SHA1, key, &mut out, None)
        .expect("OAEP encrypt")
        .len();
    out.truncate(n);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_lc_rs::cipher::{AES_128, DecryptingKey, DecryptionContext, UnboundCipherKey};
    use aws_lc_rs::encoding::AsDer;
    use aws_lc_rs::rsa::{
        KeySize, OAEP_SHA1_MGF1SHA1, OaepPrivateDecryptingKey, PrivateDecryptingKey,
    };
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD_NO_PAD;
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn alac_raw_exact_bytes() {
        assert_eq!(
            alac_raw(&[0x0102, 0x0304]),
            [
                0x20, 0x00, 0x12, 0x00, 0x00, 0x00, 0x02, 0x02, 0x04, 0x06, 0x09, 0xc0
            ]
        );
        assert_eq!(
            alac_raw(&[-1, -2]),
            [
                0x20, 0x00, 0x12, 0x00, 0x00, 0x00, 0x03, 0xff, 0xff, 0xff, 0xfd, 0xc0
            ]
        );
    }

    #[test]
    fn alac_raw_full_packet_size_and_round_trip() {
        let samples: Vec<i16> = (0..704).map(|i| (i * 91 - 30_000) as i16).collect();
        let b = alac_raw(&samples);
        assert_eq!(b.len(), 1416);
        assert_eq!(alac_raw_decode(&b), Some(samples));
    }

    #[test]
    fn audio_packet_header() {
        let p = audio_packet(true, 0x0102, 0x03040506, 0x0708090a, &[0xaa, 0xbb]);
        assert_eq!(p, [0x80, 0xe0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 0xaa, 0xbb]);
        assert_eq!(audio_packet(false, 0, 0, 0, &[])[1], 0x60);
    }

    #[test]
    fn sync_packet_layout() {
        let ntp = Ntp { secs: 1, frac: 2 };
        let p = sync_packet(true, 100, 88_200, ntp);
        assert_eq!(&p[0..4], &[0x90, 0xd4, 0x00, 0x07]);
        assert_eq!(&p[4..8], &100u32.wrapping_sub(88_200).to_be_bytes());
        assert_eq!(&p[8..16], &ntp.to_bytes());
        assert_eq!(&p[16..20], &100u32.to_be_bytes());
        assert_eq!(sync_packet(false, 1, 0, ntp)[0], 0x80);
    }

    #[test]
    fn ntp_epoch() {
        let n = Ntp::from_system(UNIX_EPOCH + Duration::from_millis(1500));
        assert_eq!(n.secs, 2_208_988_801);
        assert_eq!(n.frac, 0x8000_0000);
    }

    #[test]
    fn timing_reply_echoes_reference() {
        let mut req = [0u8; 32];
        req[0] = 0x80;
        req[1] = 0xd2;
        req[24..32].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        let rx = Ntp { secs: 10, frac: 20 };
        let tx = Ntp { secs: 30, frac: 40 };
        let r = timing_reply(&req, rx, tx).unwrap();
        assert_eq!(&r[0..4], &[0x80, 0xd3, 0, 7]);
        assert_eq!(&r[4..8], &[0; 4]);
        assert_eq!(&r[8..16], &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(&r[16..24], &rx.to_bytes());
        assert_eq!(&r[24..32], &tx.to_bytes());
        req[1] = 0xd3;
        assert_eq!(timing_reply(&req, rx, tx), None);
        assert_eq!(timing_reply(&req[..31], rx, tx), None);
    }

    #[test]
    fn resend_parse_and_reply() {
        let req = [0x80, 0xd5, 0, 1, 0x01, 0x02, 0x00, 0x03];
        assert_eq!(parse_resend(&req), Some((0x0102, 3)));
        assert_eq!(parse_resend(&[0x80, 0xd6, 0, 1, 0, 0, 0, 0]), None);
        assert_eq!(parse_resend(&req[..7]), None);
        let orig = audio_packet(false, 5, 6, 7, &[9]);
        let r = resend_reply(&orig);
        assert_eq!(&r[0..4], &[0x80, 0xd6, 0, 1]);
        assert_eq!(&r[4..], &orig[..]);
    }

    #[test]
    fn volume_db_mapping() {
        assert_eq!(volume_db(1.0, false), 0.0);
        assert_eq!(volume_db(0.5, false), -15.0);
        assert_eq!(volume_db(0.0, false), -144.0);
        assert_eq!(volume_db(0.8, true), -144.0);
        assert_eq!(volume_db(2.0, false), 0.0);
    }

    #[test]
    fn cbc_encrypts_whole_blocks_only() {
        let key = [7u8; 16];
        let iv = [9u8; 16];
        let clear: Vec<u8> = (0..1416).map(|i| (i % 251) as u8).collect();
        let mut buf = clear.clone();
        Cipher::new(key, iv).encrypt(&mut buf);
        assert_ne!(buf[..1408], clear[..1408]);
        assert_eq!(buf[1408..], clear[1408..]);
        let dk = DecryptingKey::cbc(UnboundCipherKey::new(&AES_128, &key).unwrap()).unwrap();
        let mut head = buf[..1408].to_vec();
        let out = dk
            .decrypt(&mut head, DecryptionContext::Iv128(iv.into()))
            .unwrap();
        assert_eq!(out, &clear[..1408]);
    }

    #[test]
    fn wrap_key_round_trip() {
        let sk = PrivateDecryptingKey::generate(KeySize::Rsa2048).unwrap();
        let der = sk.public_key().as_der().unwrap();
        let key = [0x42u8; 16];
        let wrapped = wrap_key(der.as_ref(), &key);
        assert_eq!(wrapped.len(), 256);
        let dk = OaepPrivateDecryptingKey::new(sk).unwrap();
        let mut out = vec![0u8; dk.min_output_size()];
        let plain = dk
            .decrypt(&OAEP_SHA1_MGF1SHA1, &wrapped, &mut out, None)
            .unwrap();
        assert_eq!(plain, &key);
    }

    #[test]
    fn apple_key_parses() {
        let c = Cipher::random();
        assert_eq!(STANDARD_NO_PAD.decode(c.rsaaeskey()).unwrap().len(), 256);
        assert_eq!(STANDARD_NO_PAD.decode(c.aesiv()).unwrap().len(), 16);
    }
}
