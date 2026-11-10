//! Driving a UPnP renderer: the `RendererControl` trait and its SOAP-over-HTTP implementation.

use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;

use super::xml::{DeviceDesc, SoapError, envelope, parse_response, parse_time};
use crate::output::renderer::Volume;

const AVT: &str = "urn:schemas-upnp-org:service:AVTransport:1";
const RCS: &str = "urn:schemas-upnp-org:service:RenderingControl:1";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportState {
    Playing,
    Paused,
    Stopped,
    NoMedia,
    Transitioning,
    Other,
}

impl TransportState {
    pub fn parse(s: &str) -> Self {
        match s.trim() {
            "PLAYING" => Self::Playing,
            "PAUSED_PLAYBACK" | "PAUSED_RECORDING" => Self::Paused,
            "STOPPED" => Self::Stopped,
            "NO_MEDIA_PRESENT" => Self::NoMedia,
            "TRANSITIONING" => Self::Transitioning,
            _ => Self::Other,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct TransportInfo {
    pub state: TransportState,
    /// `CurrentTransportStatus` is `ERROR_OCCURRED`.
    pub error: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PositionInfo {
    pub rel_time: Option<Duration>,
    pub track_uri: String,
}

/// What a session needs from a renderer. Only `SoapError::Transport` means the device
/// can't be reached.
#[async_trait]
pub trait RendererControl: Send + Sync + 'static {
    async fn set_uri(&self, uri: &str, metadata: &str) -> Result<(), SoapError>;
    async fn play(&self) -> Result<(), SoapError>;
    async fn pause(&self) -> Result<(), SoapError>;
    async fn stop(&self) -> Result<(), SoapError>;
    async fn transport_info(&self) -> Result<TransportInfo, SoapError>;
    async fn position_info(&self) -> Result<PositionInfo, SoapError>;
    /// `None` when the device has no RenderingControl.
    async fn volume(&self) -> Result<Option<Volume>, SoapError>;
    async fn set_volume(&self, level: Option<f32>, muted: Option<bool>) -> Result<(), SoapError>;
}

pub struct HttpControl {
    desc: DeviceDesc,
    client: reqwest::Client,
}

impl HttpControl {
    pub fn new(desc: DeviceDesc, timeout: Duration) -> Self {
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(timeout)
            .build()
            .expect("reqwest client");
        Self { desc, client }
    }

    async fn call(
        &self,
        url: &str,
        service: &str,
        action: &str,
        args: &[(&str, &str)],
    ) -> Result<HashMap<String, String>, SoapError> {
        let resp = self
            .client
            .post(url)
            .header("SOAPACTION", format!("\"{service}#{action}\""))
            .header("Content-Type", "text/xml; charset=\"utf-8\"")
            .body(envelope(service, action, args))
            .send()
            .await
            .map_err(|e| SoapError::Transport(e.to_string()))?;
        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| SoapError::Transport(e.to_string()))?;
        match parse_response(&body, action) {
            Err(SoapError::BadReply(e)) if !status.is_success() => {
                Err(SoapError::BadReply(format!("HTTP {status}: {e}")))
            }
            other => other,
        }
    }

    async fn avt(
        &self,
        action: &str,
        args: &[(&str, &str)],
    ) -> Result<HashMap<String, String>, SoapError> {
        self.call(&self.desc.av_transport, AVT, action, args).await
    }

    async fn rcs(
        &self,
        url: &str,
        action: &str,
        args: &[(&str, &str)],
    ) -> Result<HashMap<String, String>, SoapError> {
        self.call(url, RCS, action, args).await
    }
}

#[async_trait]
impl RendererControl for HttpControl {
    async fn set_uri(&self, uri: &str, metadata: &str) -> Result<(), SoapError> {
        self.avt(
            "SetAVTransportURI",
            &[
                ("InstanceID", "0"),
                ("CurrentURI", uri),
                ("CurrentURIMetaData", metadata),
            ],
        )
        .await
        .map(drop)
    }

    async fn play(&self) -> Result<(), SoapError> {
        self.avt("Play", &[("InstanceID", "0"), ("Speed", "1")])
            .await
            .map(drop)
    }

    async fn pause(&self) -> Result<(), SoapError> {
        self.avt("Pause", &[("InstanceID", "0")]).await.map(drop)
    }

    async fn stop(&self) -> Result<(), SoapError> {
        self.avt("Stop", &[("InstanceID", "0")]).await.map(drop)
    }

    async fn transport_info(&self) -> Result<TransportInfo, SoapError> {
        let m = self.avt("GetTransportInfo", &[("InstanceID", "0")]).await?;
        let state = m
            .get("CurrentTransportState")
            .ok_or_else(|| SoapError::BadReply("no CurrentTransportState".into()))?;
        Ok(TransportInfo {
            state: TransportState::parse(state),
            error: m
                .get("CurrentTransportStatus")
                .is_some_and(|s| s == "ERROR_OCCURRED"),
        })
    }

    async fn position_info(&self) -> Result<PositionInfo, SoapError> {
        let m = self.avt("GetPositionInfo", &[("InstanceID", "0")]).await?;
        Ok(PositionInfo {
            rel_time: m.get("RelTime").and_then(|t| parse_time(t)),
            track_uri: m.get("TrackURI").cloned().unwrap_or_default(),
        })
    }

    async fn volume(&self) -> Result<Option<Volume>, SoapError> {
        let Some(url) = &self.desc.rendering_control else {
            return Ok(None);
        };
        let args = [("InstanceID", "0"), ("Channel", "Master")];
        let v = self.rcs(url, "GetVolume", &args).await?;
        let level: f32 = v
            .get("CurrentVolume")
            .and_then(|v| v.trim().parse().ok())
            .ok_or_else(|| SoapError::BadReply("no CurrentVolume".into()))?;
        let m = self.rcs(url, "GetMute", &args).await?;
        let muted = m
            .get("CurrentMute")
            .is_some_and(|m| matches!(m.trim(), "1" | "true" | "True"));
        Ok(Some(Volume {
            level: (level / 100.0).clamp(0.0, 1.0),
            muted,
        }))
    }

    async fn set_volume(&self, level: Option<f32>, muted: Option<bool>) -> Result<(), SoapError> {
        let Some(url) = &self.desc.rendering_control else {
            return Err(SoapError::Fault {
                code: None,
                description: "no volume control".into(),
            });
        };
        if let Some(l) = level {
            let v = ((l * 100.0).round() as i64).clamp(0, 100).to_string();
            self.rcs(
                url,
                "SetVolume",
                &[
                    ("InstanceID", "0"),
                    ("Channel", "Master"),
                    ("DesiredVolume", &v),
                ],
            )
            .await?;
        }
        if let Some(m) = muted {
            let v = if m { "1" } else { "0" };
            self.rcs(
                url,
                "SetMute",
                &[
                    ("InstanceID", "0"),
                    ("Channel", "Master"),
                    ("DesiredMute", v),
                ],
            )
            .await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::Router;
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::post;

    use super::*;

    type Log = Arc<Mutex<Vec<(String, String)>>>;

    fn reply(action: &str, fields: &[(&str, &str)]) -> String {
        let f: String = fields
            .iter()
            .map(|(k, v)| format!("<{k}>{v}</{k}>"))
            .collect();
        format!(
            "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body>\
<u:{action}Response xmlns:u=\"urn:x\">{f}</u:{action}Response></s:Body></s:Envelope>"
        )
    }

    /// A renderer on 127.0.0.1 answering each action with `canned(action)`; records
    /// (SOAPACTION, body).
    async fn serve(canned: fn(&str) -> (u16, String)) -> (HttpControl, Log) {
        let log: Log = Default::default();
        let l = log.clone();
        let app = Router::new().route(
            "/ctl",
            post(move |h: HeaderMap, body: String| {
                let l = l.clone();
                async move {
                    let sa = h["soapaction"].to_str().unwrap().to_string();
                    let action = sa.trim_matches('"').split('#').nth(1).unwrap().to_string();
                    l.lock().unwrap().push((sa, body));
                    let (code, xml) = canned(&action);
                    (StatusCode::from_u16(code).unwrap(), xml)
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (
            HttpControl::new(desc(&format!("http://{addr}/ctl")), Duration::from_secs(3)),
            log,
        )
    }

    fn desc(url: &str) -> DeviceDesc {
        DeviceDesc {
            udn: "u".into(),
            name: "n".into(),
            manufacturer: None,
            model: None,
            location: url.into(),
            av_transport: url.into(),
            rendering_control: Some(url.into()),
        }
    }

    #[tokio::test]
    async fn http_control_sends_soap_action_and_args() {
        let (c, log) = serve(|a| (200, reply(a, &[]))).await;
        c.set_uri("http://h/s.mp3?a=1&b=2", "<DIDL/>")
            .await
            .unwrap();
        c.play().await.unwrap();
        let log = log.lock().unwrap();
        assert_eq!(
            log[0].0,
            "\"urn:schemas-upnp-org:service:AVTransport:1#SetAVTransportURI\""
        );
        assert!(
            log[0]
                .1
                .contains("<CurrentURI>http://h/s.mp3?a=1&amp;b=2</CurrentURI>")
        );
        assert!(log[0].1.contains("<InstanceID>0</InstanceID>"));
        assert!(
            log[0]
                .1
                .contains("<CurrentURIMetaData>&lt;DIDL/&gt;</CurrentURIMetaData>")
        );
        assert!(log[1].1.contains("<Speed>1</Speed>"));
    }

    #[tokio::test]
    async fn http_control_reads_transport_and_position() {
        let (c, _) = serve(|a| match a {
            "GetTransportInfo" => (
                200,
                reply(
                    a,
                    &[
                        ("CurrentTransportState", "PAUSED_PLAYBACK"),
                        ("CurrentTransportStatus", "ERROR_OCCURRED"),
                    ],
                ),
            ),
            _ => (
                200,
                reply(a, &[("RelTime", "0:01:02"), ("TrackURI", "http://h/s.mp3")]),
            ),
        })
        .await;
        assert_eq!(
            c.transport_info().await.unwrap(),
            TransportInfo {
                state: TransportState::Paused,
                error: true
            }
        );
        assert_eq!(
            c.position_info().await.unwrap(),
            PositionInfo {
                rel_time: Some(Duration::from_secs(62)),
                track_uri: "http://h/s.mp3".into()
            }
        );
    }

    #[tokio::test]
    async fn http_control_volume_round_trip() {
        let (c, log) = serve(|a| match a {
            "GetVolume" => (200, reply(a, &[("CurrentVolume", "35")])),
            "GetMute" => (200, reply(a, &[("CurrentMute", "1")])),
            _ => (200, reply(a, &[])),
        })
        .await;
        assert_eq!(
            c.volume().await.unwrap(),
            Some(Volume {
                level: 0.35,
                muted: true
            })
        );
        c.set_volume(Some(0.5), None).await.unwrap();
        c.set_volume(None, Some(false)).await.unwrap();
        let log = log.lock().unwrap();
        assert!(log[2].1.contains("<DesiredVolume>50</DesiredVolume>"));
        assert!(log[2].1.contains("<Channel>Master</Channel>"));
        assert!(log[3].1.contains("<DesiredMute>0</DesiredMute>"));
    }

    #[tokio::test]
    async fn http_control_no_rendering_control() {
        let mut d = desc("http://127.0.0.1:9/x");
        d.rendering_control = None;
        let c = HttpControl::new(d, Duration::from_secs(1));
        assert_eq!(c.volume().await, Ok(None));
        assert_eq!(
            c.set_volume(Some(0.1), None).await,
            Err(SoapError::Fault {
                code: None,
                description: "no volume control".into()
            })
        );
    }

    #[tokio::test]
    async fn http_control_fault_and_unreachable() {
        let (c, _) = serve(|_| {
            (
                500,
                "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body><s:Fault>\
<detail><UPnPError xmlns=\"urn:schemas-upnp-org:control-1-0\"><errorCode>701</errorCode>\
<errorDescription>Transition not available</errorDescription></UPnPError></detail></s:Fault></s:Body></s:Envelope>"
                    .into(),
            )
        })
        .await;
        assert_eq!(
            c.pause().await,
            Err(SoapError::Fault {
                code: Some(701),
                description: "Transition not available".into()
            })
        );
        // A port nobody listens on.
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let dead = HttpControl::new(
            desc(&format!("http://127.0.0.1:{port}/ctl")),
            Duration::from_secs(1),
        );
        assert!(matches!(dead.play().await, Err(SoapError::Transport(_))));
    }
}
