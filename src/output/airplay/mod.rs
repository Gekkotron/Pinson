//! AirPlay 1 (RAOP) output: packets, crypto, resampling, RTSP and the sender.

pub mod packet;
pub mod resample;
pub mod rtsp;
pub mod sink;

#[cfg(test)]
pub(crate) mod fake;
