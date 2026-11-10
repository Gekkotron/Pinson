//! Playing in a browser tab: the MP3 stream a tab's `<audio>` element reads.

pub mod stream;

pub use stream::{BYTES_PER_MS, Encoder, FRAME_BYTES, FfmpegMp3, Stream, StreamBuf, byte_offset};
