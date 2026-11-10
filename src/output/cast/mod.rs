//! Chromecast output: the Cast v2 protocol (framing and TLS), a session on top of it, and
//! the engine and speakers that make it a pickable output.

pub mod engine;
#[cfg(test)]
pub(crate) mod fake;
pub mod proto;
pub mod session;
pub mod tls;

use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use futures_util::future::BoxFuture;

pub use engine::CastEngine;

use self::session::CastConnector;
use self::tls::{CastLink, tls_connect};
use super::discovery::Discovery;

pub const CAST_PORT: u16 = 8009;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Connects to a Cast device named by `host:port`, or by a Cast id or name that mDNS finds.
/// The address is looked up again on every connect, so a device that moved is followed.
pub struct DiscoveredConnector {
    pub address: String,
    pub discovery: Discovery,
}

impl DiscoveredConnector {
    pub async fn addr(&self) -> io::Result<SocketAddr> {
        if self.address.contains(':') {
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host(self.address.as_str())
                .await?
                .collect();
            return addrs
                .iter()
                .find(|a| a.is_ipv4())
                .or(addrs.first())
                .copied()
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("no address for {}", self.address),
                    )
                });
        }
        self.discovery
            .cast(&self.address)
            .map(|d| d.addr)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("{} hasn't been found on the network", self.address),
                )
            })
    }
}

impl CastConnector for DiscoveredConnector {
    fn target(&self) -> String {
        self.address.clone()
    }

    fn connect(&self) -> BoxFuture<'static, io::Result<CastLink>> {
        let this = Self {
            address: self.address.clone(),
            discovery: self.discovery.clone(),
        };
        Box::pin(async move {
            let addr = this.addr().await?;
            tls_connect(addr, CONNECT_TIMEOUT).await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::discovery::{CastDevice, Found};

    fn dev(addr: &str) -> CastDevice {
        CastDevice {
            id: "0123456789abcdef0123456789abcdef".into(),
            name: "Living Room".into(),
            model: None,
            addr: addr.parse().unwrap(),
        }
    }

    fn found(addr: &str) -> Found {
        Found {
            casts: vec![dev(addr)],
            snapservers: vec![],
        }
    }

    #[tokio::test]
    async fn connector_resolves_each_attempt() {
        let (discovery, tx) = Discovery::manual();
        let c = DiscoveredConnector {
            address: "0123456789abcdef0123456789abcdef".into(),
            discovery: discovery.clone(),
        };
        assert_eq!(c.addr().await.unwrap_err().kind(), io::ErrorKind::NotFound);
        tx.send(found("10.0.0.4:8009")).unwrap();
        assert_eq!(c.addr().await.unwrap(), "10.0.0.4:8009".parse().unwrap());
        tx.send(found("10.0.0.8:8009")).unwrap();
        assert_eq!(c.addr().await.unwrap(), "10.0.0.8:8009".parse().unwrap());
        let direct = DiscoveredConnector {
            address: "127.0.0.1:18009".into(),
            discovery,
        };
        assert_eq!(
            direct.addr().await.unwrap(),
            "127.0.0.1:18009".parse().unwrap()
        );
    }
}
