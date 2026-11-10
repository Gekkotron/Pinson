//! Reading and writing the XML of UPnP: device descriptions, SOAP envelopes, DIDL-Lite.
//! No I/O here.

use std::collections::HashMap;
use std::time::Duration;

use reqwest::Url;

use crate::output::renderer::SongMeta;

/// What Pinson needs to know about a MediaRenderer.
#[derive(Clone, Debug, PartialEq)]
pub struct DeviceDesc {
    /// The MediaRenderer device's UDN, without the `uuid:` prefix.
    pub udn: String,
    pub name: String,
    pub manufacturer: Option<String>,
    pub model: Option<String>,
    /// The description URL it was read from.
    pub location: String,
    /// Absolute control URLs.
    pub av_transport: String,
    pub rendering_control: Option<String>,
}

impl DeviceDesc {
    pub fn sonos(&self) -> bool {
        self.manufacturer
            .as_deref()
            .is_some_and(|m| m.to_lowercase().contains("sonos"))
    }
}

type Node<'a, 'i> = roxmltree::Node<'a, 'i>;

fn child<'a, 'i>(n: Node<'a, 'i>, name: &str) -> Option<Node<'a, 'i>> {
    n.children()
        .find(|c| c.is_element() && c.tag_name().name() == name)
}

fn text(n: Node, name: &str) -> Option<String> {
    child(n, name)
        .and_then(|c| c.text())
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
}

/// The control URL of the first service whose type contains `needle`, as written.
fn control_url(device: Node, needle: &str) -> Option<String> {
    child(device, "serviceList")?
        .children()
        .filter(|c| c.is_element() && c.tag_name().name() == "service")
        .find(|s| text(*s, "serviceType").is_some_and(|t| t.contains(needle)))
        .and_then(|s| text(s, "controlURL"))
}

/// The first MediaRenderer with an AVTransport service: the root first, then the embedded
/// devices depth-first.
fn find_renderer<'a, 'i>(device: Node<'a, 'i>) -> Option<Node<'a, 'i>> {
    let is_renderer = text(device, "deviceType").is_some_and(|t| t.contains(":MediaRenderer:"));
    if is_renderer && control_url(device, ":AVTransport:").is_some() {
        return Some(device);
    }
    child(device, "deviceList")?
        .children()
        .filter(|c| c.is_element() && c.tag_name().name() == "device")
        .find_map(find_renderer)
}

pub fn parse_description(xml: &str, location: &str) -> Result<DeviceDesc, String> {
    let doc = roxmltree::Document::parse(xml).map_err(|e| format!("bad description: {e}"))?;
    let root = doc.root_element();
    let base = text(root, "URLBase").unwrap_or_else(|| location.to_string());
    let base = Url::parse(&base)
        .or_else(|_| Url::parse(location))
        .map_err(|e| format!("bad base URL: {e}"))?;
    let device = child(root, "device")
        .and_then(find_renderer)
        .ok_or_else(|| "no MediaRenderer with AVTransport".to_string())?;
    let resolve = |u: String| {
        base.join(&u)
            .map(|u| u.to_string())
            .map_err(|e| format!("bad control URL {u}: {e}"))
    };
    let av_transport = resolve(control_url(device, ":AVTransport:").expect("checked"))?;
    let rendering_control = control_url(device, ":RenderingControl:")
        .map(resolve)
        .transpose()?;
    let udn = text(device, "UDN").ok_or("no UDN")?;
    let udn = udn.strip_prefix("uuid:").unwrap_or(&udn).to_string();
    Ok(DeviceDesc {
        name: text(device, "friendlyName").unwrap_or_else(|| "UPnP renderer".into()),
        manufacturer: text(device, "manufacturer"),
        model: text(device, "modelName"),
        udn,
        location: location.to_string(),
        av_transport,
        rendering_control,
    })
}

/// Escapes text for XML content and attributes.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

/// A SOAP request body for `action` of `service` (a full service type URN).
pub fn envelope(service: &str, action: &str, args: &[(&str, &str)]) -> String {
    let mut body = String::new();
    for (k, v) in args {
        body.push_str(&format!("<{k}>{}</{k}>", escape(v)));
    }
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\">\
<s:Body><u:{action} xmlns:u=\"{service}\">{body}</u:{action}></s:Body></s:Envelope>"
    )
}

#[derive(Debug, Clone, PartialEq)]
pub enum SoapError {
    /// The device answered with a UPnP error.
    Fault {
        code: Option<u32>,
        description: String,
    },
    /// The device could not be reached (connection, timeout).
    Transport(String),
    /// The device answered with something that is not the expected SOAP.
    BadReply(String),
}

impl std::fmt::Display for SoapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SoapError::Fault { code, description } => match code {
                Some(c) => write!(f, "UPnP error {c}: {description}"),
                None => write!(f, "UPnP error: {description}"),
            },
            SoapError::Transport(e) => write!(f, "can't reach the renderer: {e}"),
            SoapError::BadReply(e) => write!(f, "bad reply: {e}"),
        }
    }
}

impl std::error::Error for SoapError {}

/// The fields of `<{action}Response>`, or the fault the device returned.
pub fn parse_response(xml: &str, action: &str) -> Result<HashMap<String, String>, SoapError> {
    let doc = roxmltree::Document::parse(xml).map_err(|e| SoapError::BadReply(e.to_string()))?;
    if let Some(err) = doc
        .descendants()
        .find(|n| n.is_element() && n.tag_name().name() == "UPnPError")
    {
        let code = text(err, "errorCode").and_then(|c| c.parse().ok());
        let description = text(err, "errorDescription").unwrap_or_default();
        return Err(SoapError::Fault { code, description });
    }
    let name = format!("{action}Response");
    let resp = doc
        .descendants()
        .find(|n| n.is_element() && n.tag_name().name() == name)
        .ok_or_else(|| SoapError::BadReply(format!("no {name}")))?;
    Ok(resp
        .children()
        .filter(|c| c.is_element())
        .map(|c| {
            (
                c.tag_name().name().to_string(),
                c.text().unwrap_or("").trim().to_string(),
            )
        })
        .collect())
}

/// `H+:MM:SS` with an optional `.fraction`. Empty and `NOT_IMPLEMENTED` are `None`.
pub fn parse_time(s: &str) -> Option<Duration> {
    let mut parts = s.trim().split(':');
    let h: u64 = parts.next()?.parse().ok()?;
    let m: u64 = parts.next()?.parse().ok()?;
    let sec: f64 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || m >= 60 || !(0.0..60.0).contains(&sec) {
        return None;
    }
    Some(Duration::from_secs_f64((h * 3600 + m * 60) as f64 + sec))
}

/// `H:MM:SS.mmm`.
pub fn format_time(d: Duration) -> String {
    let ms = d.as_millis();
    let (h, m, s, ms) = (ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000);
    format!("{h}:{m:02}:{s:02}.{ms:03}")
}

/// The DIDL-Lite metadata for the song at `url`.
pub fn didl(meta: Option<&SongMeta>, url: &str) -> String {
    let title = meta.map_or("Pinson", |m| m.title.as_str());
    let mut item = format!("<dc:title>{}</dc:title>", escape(title));
    if let Some(m) = meta {
        item.push_str(&format!(
            "<upnp:artist>{a}</upnp:artist><dc:creator>{a}</dc:creator>",
            a = escape(&m.artist)
        ));
        if let Some(album) = &m.album {
            item.push_str(&format!("<upnp:album>{}</upnp:album>", escape(album)));
        }
        if let Some(img) = &m.image {
            item.push_str(&format!(
                "<upnp:albumArtURI>{}</upnp:albumArtURI>",
                escape(img)
            ));
        }
    }
    item.push_str("<upnp:class>object.item.audioItem.musicTrack</upnp:class>");
    let duration = meta
        .and_then(|m| m.duration)
        .map(|d| format!(" duration=\"{}\"", format_time(d)))
        .unwrap_or_default();
    format!(
        "<DIDL-Lite xmlns=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\" \
xmlns:dc=\"http://purl.org/dc/elements/1.1/\" \
xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\">\
<item id=\"1\" parentID=\"0\" restricted=\"1\">{item}\
<res protocolInfo=\"http-get:*:audio/mpeg:DLNA.ORG_PN=MP3;DLNA.ORG_OP=00;DLNA.ORG_FLAGS=01700000000000000000000000000000\"{duration}>{}</res>\
</item></DIDL-Lite>",
        escape(url)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const TV: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/upnp/tv.xml"
    ));
    const SONOS: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/upnp/sonos.xml"
    ));

    #[test]
    fn parses_a_real_tv() {
        let d = parse_description(TV, "http://10.0.0.3:49152/description.xml").unwrap();
        assert_eq!(d.udn, "4d696e69-1234-4abc-8def-0123456789ab");
        assert_eq!(d.name, "55MLED910/12(10.0.0.3)");
        assert_eq!(d.av_transport, "http://10.0.0.3:49152/avt_control");
        assert_eq!(
            d.rendering_control.as_deref(),
            Some("http://10.0.0.3:49152/rcr_control")
        );
        assert!(!d.sonos());
    }

    #[test]
    fn parses_sonos_embedded_renderer() {
        let d =
            parse_description(SONOS, "http://10.0.0.5:1400/xml/device_description.xml").unwrap();
        assert_eq!(d.udn, "RINCON_000E58A0B1C201400_MR");
        assert_eq!(
            d.av_transport,
            "http://10.0.0.5:1400/MediaRenderer/AVTransport/Control"
        );
        assert_eq!(
            d.rendering_control.as_deref(),
            Some("http://10.0.0.5:1400/MediaRenderer/RenderingControl/Control")
        );
        assert!(d.sonos());
    }

    #[test]
    fn rejects_a_media_server() {
        let xml = r#"<root xmlns="urn:schemas-upnp-org:device-1-0"><device>
<deviceType>urn:schemas-upnp-org:device:MediaServer:1</deviceType>
<UDN>uuid:x</UDN><serviceList><service>
<serviceType>urn:schemas-upnp-org:service:ContentDirectory:1</serviceType>
<controlURL>/cd</controlURL></service></serviceList></device></root>"#;
        assert!(parse_description(xml, "http://10.0.0.9/d.xml").is_err());
    }

    #[test]
    fn envelope_escapes_args() {
        let e = envelope(
            "urn:x:service:AVTransport:1",
            "SetAVTransportURI",
            &[("InstanceID", "0"), ("CurrentURI", "http://h/a?x=1&y=<2>")],
        );
        assert!(e.contains("<u:SetAVTransportURI xmlns:u=\"urn:x:service:AVTransport:1\">"));
        assert!(e.contains("<InstanceID>0</InstanceID>"));
        assert!(e.contains("<CurrentURI>http://h/a?x=1&amp;y=&lt;2&gt;</CurrentURI>"));
        assert!(roxmltree::Document::parse(&e).is_ok());
        assert_eq!(escape("\"'"), "&quot;&apos;");
    }

    #[test]
    fn parse_response_reads_fields_and_faults() {
        let ok = r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body>
<u:GetVolumeResponse xmlns:u="urn:x"><CurrentVolume>35</CurrentVolume></u:GetVolumeResponse>
</s:Body></s:Envelope>"#;
        let m = parse_response(ok, "GetVolume").unwrap();
        assert_eq!(m["CurrentVolume"], "35");
        let fault = r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body>
<s:Fault><faultcode>s:Client</faultcode><faultstring>UPnPError</faultstring><detail>
<UPnPError xmlns="urn:schemas-upnp-org:control-1-0"><errorCode>716</errorCode>
<errorDescription>Resource not found</errorDescription></UPnPError></detail></s:Fault>
</s:Body></s:Envelope>"#;
        assert_eq!(
            parse_response(fault, "GetVolume"),
            Err(SoapError::Fault {
                code: Some(716),
                description: "Resource not found".into()
            })
        );
        assert!(matches!(
            parse_response("nope", "GetVolume"),
            Err(SoapError::BadReply(_))
        ));
    }

    #[test]
    fn parse_time_cases() {
        assert_eq!(parse_time("0:03:07"), Some(Duration::from_secs(187)));
        assert_eq!(
            parse_time("1:00:00.500"),
            Some(Duration::from_millis(3_600_500))
        );
        assert_eq!(parse_time(""), None);
        assert_eq!(parse_time("NOT_IMPLEMENTED"), None);
        assert_eq!(format_time(Duration::from_millis(3_600_500)), "1:00:00.500");
    }

    #[test]
    fn didl_escapes_and_has_duration() {
        let meta = SongMeta {
            title: "AC/DC & \"Friends\"".into(),
            artist: "A<B".into(),
            album: Some("Alb".into()),
            image: Some("http://h/i?a=1&b=2".into()),
            duration: Some(Duration::from_secs(225)),
        };
        let d = didl(Some(&meta), "http://h/song.mp3?x=1&y=2");
        assert!(d.contains("AC/DC &amp; &quot;Friends&quot;"));
        assert!(d.contains("duration=\"0:03:45.000\""));
        assert!(d.contains("<upnp:album>Alb</upnp:album>"));
        assert!(d.contains("song.mp3?x=1&amp;y=2</res>"));
        assert!(roxmltree::Document::parse(&d).is_ok());
        let bare = didl(None, "http://h/s.mp3");
        assert!(bare.contains("<dc:title>Pinson</dc:title>"));
        assert!(!bare.contains("duration="));
        assert!(roxmltree::Document::parse(&bare).is_ok());
    }
}
