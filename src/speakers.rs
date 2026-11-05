//! Relay to nidoo-sound's HTTP API: list the satellites, set volume and mute.

use std::time::Duration;

use axum::http::StatusCode;
use serde::{Deserialize, Serialize};

/// Port of nidoo-sound's HTTP API when only the PCM address is known.
const DEFAULT_CONTROL_PORT: u16 = 4954;

/// Base URL of nidoo-sound's HTTP API: `explicit` (trailing `/` trimmed) if
/// given, else the host of `nidoo_sound` on port 4954.
pub fn control_url(nidoo_sound: &str, explicit: Option<&str>) -> String {
    if let Some(url) = explicit {
        return url.trim_end_matches('/').to_owned();
    }
    let host = nidoo_sound
        .rsplit_once(':')
        .map_or(nidoo_sound, |(host, _)| host);
    format!("http://{host}:{DEFAULT_CONTROL_PORT}")
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Speaker {
    pub id: String,
    pub name: String,
    pub volume: u8,
    pub muted: bool,
    pub connected: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpeakerChange {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub muted: Option<bool>,
}

#[derive(Debug)]
pub enum SpeakerError {
    /// nidoo-sound refused the request (400 or 404) with this message.
    Rejected(StatusCode, String),
    /// Connection error, timeout, 5xx or an unreadable answer.
    Unreachable,
}

pub struct SpeakerControl {
    base: String,
    http: reqwest::Client,
}

impl SpeakerControl {
    pub fn new(base: String, timeout: Duration) -> Self {
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .expect("build http client");
        Self {
            base: base.trim_end_matches('/').to_owned(),
            http,
        }
    }

    pub async fn list(&self) -> Result<Vec<Speaker>, SpeakerError> {
        let res = self
            .http
            .get(format!("{}/players", self.base))
            .send()
            .await
            .map_err(unreachable)?;
        if !res.status().is_success() {
            return Err(self.failure(res).await);
        }
        let bytes = res.bytes().await.map_err(unreachable)?;
        serde_json::from_slice(&bytes).map_err(|e| {
            tracing::warn!("nidoo-sound sent an unreadable speaker list: {e}");
            SpeakerError::Unreachable
        })
    }

    pub async fn set(&self, id: &str, change: &SpeakerChange) -> Result<(), SpeakerError> {
        let body = serde_json::to_vec(change).expect("change serializes");
        let res = self
            .http
            .put(format!("{}/players/{id}", self.base))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .await
            .map_err(unreachable)?;
        if res.status().is_success() {
            Ok(())
        } else {
            Err(self.failure(res).await)
        }
    }

    async fn failure(&self, res: reqwest::Response) -> SpeakerError {
        let status = res.status();
        if status != reqwest::StatusCode::BAD_REQUEST && status != reqwest::StatusCode::NOT_FOUND {
            tracing::warn!("nidoo-sound answered {status}");
            return SpeakerError::Unreachable;
        }
        let bytes = res.bytes().await.unwrap_or_default();
        let message = serde_json::from_slice::<serde_json::Value>(&bytes)
            .ok()
            .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_owned))
            .unwrap_or_else(|| status.to_string());
        let status = if status == reqwest::StatusCode::NOT_FOUND {
            StatusCode::NOT_FOUND
        } else {
            StatusCode::BAD_REQUEST
        };
        SpeakerError::Rejected(status, message)
    }
}

fn unreachable(e: reqwest::Error) -> SpeakerError {
    tracing::warn!("can't reach nidoo-sound: {e}");
    SpeakerError::Unreachable
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_url_derives_and_overrides() {
        assert_eq!(
            control_url("192.168.1.91:4953", None),
            "http://192.168.1.91:4954"
        );
        assert_eq!(control_url("nidoo.local", None), "http://nidoo.local:4954");
        assert_eq!(control_url("x:1", Some("http://h:9/")), "http://h:9");
    }
}
