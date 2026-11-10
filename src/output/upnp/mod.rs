//! UPnP/DLNA renderers: device description, SOAP control and (later) the session and discovery.

pub mod control;
#[cfg(test)]
pub(crate) mod fake;
pub mod session;
pub mod xml;
