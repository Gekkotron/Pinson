//! TLS to a Chromecast on port 8009. Devices use self-signed certificates, so any
//! certificate is accepted; handshake signatures are still verified.

use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms, aws_lc_rs};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

/// A byte stream to the device (TLS in production, a pipe in tests).
pub trait CastIo: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> CastIo for T {}

/// An open connection and the local address the device sees us at.
pub struct CastLink {
    pub io: Box<dyn CastIo>,
    pub local_ip: IpAddr,
}

#[derive(Debug)]
struct AcceptAnyCert(WebPkiSupportedAlgorithms);

impl ServerCertVerifier for AcceptAnyCert {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.0)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.0)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.supported_schemes()
    }
}

fn client_config() -> io::Result<ClientConfig> {
    let provider: Arc<CryptoProvider> = Arc::new(aws_lc_rs::default_provider());
    let algs = provider.signature_verification_algorithms;
    ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS12, &rustls::version::TLS13])
        .map_err(io::Error::other)
        .map(|b| {
            b.dangerous()
                .with_custom_certificate_verifier(Arc::new(AcceptAnyCert(algs)))
                .with_no_client_auth()
        })
}

/// Connects to `addr` and completes the TLS handshake, all within `timeout`.
pub async fn tls_connect(addr: SocketAddr, timeout: Duration) -> io::Result<CastLink> {
    let config = Arc::new(client_config()?);
    let connect = async move {
        let tcp = TcpStream::connect(addr).await?;
        tcp.set_nodelay(true)?;
        let local_ip = tcp.local_addr()?.ip();
        let tls = TlsConnector::from(config)
            .connect(ServerName::IpAddress(addr.ip().into()), tcp)
            .await?;
        Ok(CastLink {
            io: Box::new(tls),
            local_ip,
        })
    };
    tokio::time::timeout(timeout, connect)
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "Cast connect timed out"))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn tls_connect_refused_errors() {
        let port = {
            let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
            l.local_addr().unwrap().port()
        };
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        let res = tls_connect(addr, Duration::from_secs(5)).await;
        assert!(res.is_err());
    }

    #[tokio::test]
    async fn tls_connect_times_out() {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let _hold = tokio::spawn(async move {
            let _conn = l.accept().await;
            std::future::pending::<()>().await;
        });
        let err = match tls_connect(addr, Duration::from_millis(200)).await {
            Ok(_) => panic!("expected a timeout"),
            Err(e) => e,
        };
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
    }
}
