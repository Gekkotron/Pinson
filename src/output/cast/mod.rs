//! Chromecast output: the Cast v2 protocol (framing and TLS) and, on top of it, a session.

#[cfg(test)]
pub(crate) mod fake;
pub mod proto;
pub mod session;
pub mod tls;
