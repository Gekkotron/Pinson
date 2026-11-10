//! UPnP/DLNA output: device description, SOAP control, a session on top of them, and the
//! engine that makes a renderer a pickable output.

pub mod control;
pub mod engine;
#[cfg(test)]
pub(crate) mod fake;
pub mod session;
pub mod xml;

use std::io;
use std::net::{IpAddr, UdpSocket};
use std::time::Duration;

use futures_util::future::BoxFuture;
use reqwest::Url;

pub use engine::UpnpEngine;

use self::control::HttpControl;
use self::session::{UpnpConnector, UpnpLink};
use self::xml::{DeviceDesc, parse_description};
use super::discovery::Discovery;

const IO_TIMEOUT: Duration = Duration::from_secs(3);

/// Connects to a renderer named by description URL, UDN or friendly name. The renderer is
/// resolved again on every connect, so a device that moved is followed.
pub struct DiscoveredRenderer {
    pub address: String,
    pub discovery: Discovery,
}

fn not_found(address: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        format!("{address} hasn't been found on the network"),
    )
}

/// The address of this machine facing `url`'s host. A connected UDP socket sends nothing.
fn local_ip_for(url: &str) -> io::Result<IpAddr> {
    let url = Url::parse(url).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let host = url
        .host_str()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no host in control URL"))?;
    let port = url.port_or_known_default().unwrap_or(80);
    let socket = UdpSocket::bind("0.0.0.0:0")?;
    socket.connect((host, port))?;
    Ok(socket.local_addr()?.ip())
}

impl DiscoveredRenderer {
    async fn describe(&self) -> io::Result<DeviceDesc> {
        let address = self.address.as_str();
        if address.starts_with("http://") || address.starts_with("https://") {
            let client = reqwest::Client::builder()
                .no_proxy()
                .timeout(IO_TIMEOUT)
                .build()
                .map_err(io::Error::other)?;
            let xml = client
                .get(address)
                .send()
                .await
                .and_then(|r| r.error_for_status())
                .map_err(io::Error::other)?
                .text()
                .await
                .map_err(io::Error::other)?;
            return parse_description(&xml, address)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e));
        }
        self.discovery
            .renderer(address)
            .ok_or_else(|| not_found(address))
    }
}

impl UpnpConnector for DiscoveredRenderer {
    fn target(&self) -> String {
        self.address.clone()
    }

    fn connect(&self) -> BoxFuture<'static, io::Result<UpnpLink>> {
        let this = Self {
            address: self.address.clone(),
            discovery: self.discovery.clone(),
        };
        Box::pin(async move {
            let desc = this.describe().await?;
            let local_ip = local_ip_for(&desc.av_transport)?;
            let sonos = desc.sonos();
            Ok(UpnpLink {
                control: std::sync::Arc::new(HttpControl::new(
                    desc,
                    IO_TIMEOUT,
                    session::LOAD_CALL_TIMEOUT,
                )),
                local_ip,
                sonos,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::discovery::Found;

    const TV: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/upnp/tv.xml"
    ));

    #[tokio::test]
    async fn connector_resolves_each_attempt() {
        let (discovery, tx) = Discovery::manual();
        let c = DiscoveredRenderer {
            address: "Kitchen".into(),
            discovery,
        };
        let err = c.describe().await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert_eq!(err.to_string(), "Kitchen hasn't been found on the network");
        let desc = parse_description(TV, "http://127.0.0.1:49152/d.xml").unwrap();
        let c = DiscoveredRenderer {
            address: desc.name.clone(),
            ..c
        };
        tx.send(Found {
            renderers: vec![desc.clone()],
            ..Found::default()
        })
        .unwrap();
        assert_eq!(c.describe().await.unwrap(), desc);
    }

    #[test]
    fn local_ip_faces_the_host() {
        assert_eq!(
            local_ip_for("http://127.0.0.1:49152/ctl").unwrap(),
            "127.0.0.1".parse::<IpAddr>().unwrap()
        );
    }
}
