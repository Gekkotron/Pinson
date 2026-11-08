//! Relay to nidoo-sound's HTTP API: list the satellites, set volume and mute.
//!
//! nidoo-sound names a satellite after what the satellite itself reports; a rename made in
//! Nidoo lives in the hub, so the names come from the hub's snapshot when one is set.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::http::StatusCode;
use serde::{Deserialize, Serialize};

/// Port of nidoo-sound's HTTP API when only the PCM address is known.
const DEFAULT_CONTROL_PORT: u16 = 4954;

/// Port of the Nidoo hub's HTTP API when only the PCM address is known.
const DEFAULT_HUB_PORT: u16 = 8081;

/// How long the names read from the hub are reused before asking again.
const NAMES_TTL: Duration = Duration::from_secs(30);

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

/// Base URL of the Nidoo hub's HTTP API: `explicit` (trailing `/` trimmed) if given, else
/// the host of `nidoo_sound` on port 8081.
pub fn hub_url(nidoo_sound: &str, explicit: Option<&str>) -> String {
    if let Some(url) = explicit {
        return url.trim_end_matches('/').to_owned();
    }
    let host = nidoo_sound
        .rsplit_once(':')
        .map_or(nidoo_sound, |(host, _)| host);
    format!("http://{host}:{DEFAULT_HUB_PORT}")
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
    hub: Option<HubNames>,
}

/// The speaker names set in Nidoo, read from the hub's snapshot and kept for [`NAMES_TTL`].
struct HubNames {
    base: String,
    cache: Mutex<Option<(Instant, HashMap<String, String>)>>,
}

#[derive(Deserialize)]
struct Snapshot {
    #[serde(default)]
    entities: Vec<SnapshotEntity>,
}

#[derive(Deserialize)]
struct SnapshotEntity {
    id: String,
    #[serde(default)]
    ui: Option<UiOverride>,
}

#[derive(Deserialize)]
struct UiOverride {
    #[serde(default)]
    name: Option<String>,
}

/// Speaker id -> the name given in Nidoo. A speaker is renamed through its main entity,
/// `nidoo_sound_<id>_volume`; its `ui.name` is set only when someone renamed it.
fn names_from_snapshot(snapshot: Snapshot) -> HashMap<String, String> {
    snapshot
        .entities
        .into_iter()
        .filter_map(|e| {
            let id = e.id.strip_prefix("nidoo_sound_")?.strip_suffix("_volume")?;
            let name = e.ui?.name?.trim().to_owned();
            (!name.is_empty()).then(|| (id.to_owned(), name))
        })
        .collect()
}

/// Puts the Nidoo names on `speakers` and sorts them by name, then id, like nidoo-sound does.
fn rename(speakers: &mut [Speaker], names: &HashMap<String, String>) {
    for sp in speakers.iter_mut() {
        if let Some(name) = names.get(&sp.id) {
            sp.name.clone_from(name);
        }
    }
    speakers.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
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
            hub: None,
        }
    }

    /// Names the speakers as the Nidoo hub at `base` does. A hub that can't be read leaves
    /// nidoo-sound's names.
    pub fn with_hub(mut self, base: String) -> Self {
        self.hub = Some(HubNames {
            base: base.trim_end_matches('/').to_owned(),
            cache: Mutex::new(None),
        });
        self
    }

    pub async fn list(&self) -> Result<Vec<Speaker>, SpeakerError> {
        let (speakers, names) = tokio::join!(self.players(), self.hub_names());
        let mut speakers = speakers?;
        rename(&mut speakers, &names);
        Ok(speakers)
    }

    /// The names set in Nidoo; the last ones read (or none) while the hub can't be read.
    async fn hub_names(&self) -> HashMap<String, String> {
        let Some(hub) = &self.hub else {
            return HashMap::new();
        };
        if let Some((at, names)) = &*hub.cache.lock().unwrap()
            && at.elapsed() < NAMES_TTL
        {
            return names.clone();
        }
        let fetched = async {
            let res = self
                .http
                .get(format!("{}/api/v1/snapshot", hub.base))
                .send()
                .await?
                .error_for_status()?;
            res.json::<Snapshot>().await
        }
        .await;
        let mut cache = hub.cache.lock().unwrap();
        let names = match fetched {
            Ok(snapshot) => names_from_snapshot(snapshot),
            Err(e) => {
                tracing::warn!("can't read speaker names from the Nidoo hub: {e}");
                cache.as_ref().map(|(_, n)| n.clone()).unwrap_or_default()
            }
        };
        // A failure is cached too, so a hub that is down doesn't slow every poll.
        *cache = Some((Instant::now(), names.clone()));
        names
    }

    async fn players(&self) -> Result<Vec<Speaker>, SpeakerError> {
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

    #[test]
    fn hub_url_derives_and_overrides() {
        assert_eq!(
            hub_url("192.168.1.91:4953", None),
            "http://192.168.1.91:8081"
        );
        assert_eq!(hub_url("nidoo.local", None), "http://nidoo.local:8081");
        assert_eq!(hub_url("x:1", Some("http://h:9/")), "http://h:9");
    }

    fn speaker(id: &str, name: &str) -> Speaker {
        Speaker {
            id: id.into(),
            name: name.into(),
            volume: 50,
            muted: false,
            connected: true,
        }
    }

    #[test]
    fn snapshot_names_come_from_the_volume_entity_override() {
        let snapshot: Snapshot = serde_json::from_value(serde_json::json!({"entities": [
            {"id": "nidoo_sound_aaa_volume", "ui": {"name": " Salon "}},
            {"id": "nidoo_sound_aaa_mute", "ui": {"name": "Mute"}},
            {"id": "nidoo_sound_bbb_volume", "name": "Office Music volume"},
            {"id": "nidoo_sound_ccc_volume", "ui": {"area": "living"}},
            {"id": "nidoo_sound_ddd_volume", "ui": {"name": ""}},
            {"id": "light_x_volume", "ui": {"name": "Lamp"}}
        ]}))
        .unwrap();
        let names = names_from_snapshot(snapshot);
        assert_eq!(
            names,
            HashMap::from([("aaa".to_owned(), "Salon".to_owned())])
        );
    }

    #[test]
    fn rename_replaces_known_names_and_resorts() {
        let mut list = vec![speaker("aaa", "Alpha"), speaker("zzz", "Nidoo satellite")];
        let names = HashMap::from([("zzz".to_owned(), "Bedroom".to_owned())]);
        rename(&mut list, &names);
        let shown: Vec<_> = list.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(shown, ["Alpha", "Bedroom"]);
    }
}
