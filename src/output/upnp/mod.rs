//! UPnP/DLNA renderers: device description, SOAP control and (later) the session and discovery.

// The session and discovery tasks that use these pieces come next.
#[allow(dead_code)]
pub mod control;
#[cfg(test)]
pub(crate) mod fake;
#[allow(dead_code)]
pub mod xml;
