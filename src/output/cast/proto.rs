//! Cast v2 wire format: a 4-byte big-endian length, then a protobuf `CastMessage`.
//!
//! Only the string-payload subset Pinson needs is implemented, by hand.

use std::io;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const NS_CONNECTION: &str = "urn:x-cast:com.google.cast.tp.connection";
pub const NS_HEARTBEAT: &str = "urn:x-cast:com.google.cast.tp.heartbeat";
pub const NS_RECEIVER: &str = "urn:x-cast:com.google.cast.receiver";
pub const NS_MEDIA: &str = "urn:x-cast:com.google.cast.media";

/// Our sender id on the Cast channel.
pub const SENDER_ID: &str = "sender-pinson";
/// The device's platform receiver, the destination of connection and receiver messages.
pub const PLATFORM_ID: &str = "receiver-0";
/// The Default Media Receiver app.
pub const APP_ID: &str = "CC1AD845";
/// Largest frame body accepted, in bytes.
pub const MAX_FRAME: usize = 64 * 1024;

/// A Cast message with a string payload (protocol version 0).
#[derive(Clone, Debug, PartialEq)]
pub struct CastMessage {
    pub source: String,
    pub destination: String,
    pub namespace: String,
    pub payload: String,
}

/// A protobuf body that does not parse.
#[derive(Debug, PartialEq, thiserror::Error)]
#[error("bad Cast message: {0}")]
pub struct ProtoError(&'static str);

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8 & 0x7f) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn put_str(out: &mut Vec<u8>, field: u8, s: &str) {
    out.push(field << 3 | 2);
    put_varint(out, s.len() as u64);
    out.extend_from_slice(s.as_bytes());
}

/// The frame for `msg`: length prefix followed by the protobuf.
pub fn encode(msg: &CastMessage) -> Vec<u8> {
    let mut body = vec![0x08, 0x00];
    put_str(&mut body, 2, &msg.source);
    put_str(&mut body, 3, &msg.destination);
    put_str(&mut body, 4, &msg.namespace);
    body.extend_from_slice(&[0x28, 0x00]);
    put_str(&mut body, 6, &msg.payload);
    let mut frame = (body.len() as u32).to_be_bytes().to_vec();
    frame.extend_from_slice(&body);
    frame
}

fn get_varint(b: &mut &[u8]) -> Result<u64, ProtoError> {
    let mut v = 0u64;
    for shift in (0..64).step_by(7) {
        let (&byte, rest) = b.split_first().ok_or(ProtoError("truncated varint"))?;
        *b = rest;
        v |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(v);
        }
    }
    Err(ProtoError("varint too long"))
}

fn get_bytes<'a>(b: &mut &'a [u8], n: usize) -> Result<&'a [u8], ProtoError> {
    if b.len() < n {
        return Err(ProtoError("truncated field"));
    }
    let (head, rest) = b.split_at(n);
    *b = rest;
    Ok(head)
}

/// Parses a protobuf body (no length prefix). Unknown fields are skipped; a binary
/// payload leaves `payload` empty.
pub fn decode(body: &[u8]) -> Result<CastMessage, ProtoError> {
    let mut msg = CastMessage {
        source: String::new(),
        destination: String::new(),
        namespace: String::new(),
        payload: String::new(),
    };
    let mut b = body;
    while !b.is_empty() {
        let tag = get_varint(&mut b)?;
        let (field, wire) = (tag >> 3, tag & 7);
        match wire {
            0 => {
                get_varint(&mut b)?;
            }
            1 => {
                get_bytes(&mut b, 8)?;
            }
            5 => {
                get_bytes(&mut b, 4)?;
            }
            2 => {
                let len = usize::try_from(get_varint(&mut b)?)
                    .map_err(|_| ProtoError("field too long"))?;
                let data = get_bytes(&mut b, len)?;
                let target = match field {
                    2 => &mut msg.source,
                    3 => &mut msg.destination,
                    4 => &mut msg.namespace,
                    6 => &mut msg.payload,
                    _ => continue,
                };
                *target = std::str::from_utf8(data)
                    .map_err(|_| ProtoError("invalid utf-8"))?
                    .to_owned();
            }
            _ => return Err(ProtoError("unsupported wire type")),
        }
    }
    Ok(msg)
}

/// Reads one frame. EOF is `UnexpectedEof`; an oversized or malformed frame is `InvalidData`.
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> io::Result<CastMessage> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len).await?;
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Cast frame of {len} bytes is over the {MAX_FRAME} limit"),
        ));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await?;
    decode(&body).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Writes `msg` as one frame and flushes.
pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, msg: &CastMessage) -> io::Result<()> {
    w.write_all(&encode(msg)).await?;
    w.flush().await
}

/// A message from us carrying a JSON payload.
pub fn json_msg(destination: &str, namespace: &str, payload: &serde_json::Value) -> CastMessage {
    CastMessage {
        source: SENDER_ID.to_owned(),
        destination: destination.to_owned(),
        namespace: namespace.to_owned(),
        payload: payload.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> CastMessage {
        CastMessage {
            source: "a".into(),
            destination: "b".into(),
            namespace: "n".into(),
            payload: "p".into(),
        }
    }

    #[test]
    fn encode_decode_round_trip() {
        let msg = CastMessage {
            source: SENDER_ID.into(),
            destination: PLATFORM_ID.into(),
            namespace: NS_RECEIVER.into(),
            payload: "{\"title\":\"Été ♪ 音楽\"}".into(),
        };
        let frame = encode(&msg);
        assert_eq!(
            u32::from_be_bytes(frame[..4].try_into().unwrap()) as usize,
            frame.len() - 4
        );
        assert_eq!(decode(&frame[4..]).unwrap(), msg);
    }

    #[test]
    fn encode_matches_protobuf_layout() {
        assert_eq!(
            encode(&sample()),
            [
                0, 0, 0, 16, 0x08, 0x00, 0x12, 1, b'a', 0x1a, 1, b'b', 0x22, 1, b'n', 0x28, 0x00,
                0x32, 1, b'p'
            ]
        );
    }

    #[test]
    fn decode_skips_unknown_fields() {
        let mut body = encode(&sample())[4..].to_vec();
        body.extend_from_slice(&[0x48, 0x96, 0x01]); // field 9 varint 150
        body.extend_from_slice(&[0x52, 2, 0xff, 0xfe]); // field 10 bytes
        body.extend_from_slice(&[0x3a, 2, 1, 2]); // field 7 binary payload
        assert_eq!(decode(&body).unwrap(), sample());
    }

    #[test]
    fn decode_rejects_truncated() {
        let body = encode(&sample())[4..].to_vec();
        assert!(decode(&body[..body.len() - 1]).is_err());
        assert!(decode(&[0x12, 5, b'a']).is_err());
    }

    #[tokio::test]
    async fn read_frame_rejects_oversized() {
        let mut data = ((MAX_FRAME + 1) as u32).to_be_bytes().to_vec();
        data.extend_from_slice(&[0; 8]);
        let err = read_frame(&mut &data[..]).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        let err = read_frame(&mut &[][..]).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[tokio::test]
    async fn frames_over_duplex() {
        let (mut a, mut b) = tokio::io::duplex(1024);
        let m2 = json_msg("x", NS_MEDIA, &serde_json::json!({"type": "PLAY"}));
        write_frame(&mut a, &sample()).await.unwrap();
        write_frame(&mut a, &m2).await.unwrap();
        assert_eq!(read_frame(&mut b).await.unwrap(), sample());
        let got = read_frame(&mut b).await.unwrap();
        assert_eq!(got, m2);
        assert_eq!(got.source, SENDER_ID);
    }
}
