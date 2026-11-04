//! The `Song` type shared by search, the queue and the API.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Song {
    pub video_id: String,
    pub title: String,
    pub artists: String,
    pub album: Option<String>,
    pub duration_secs: Option<u32>,
    pub artwork_url: Option<String>,
}

/// Parses "m:ss" or "h:mm:ss" into seconds.
pub fn parse_duration(s: &str) -> Option<u32> {
    let parts: Vec<&str> = s.trim().split(':').collect();
    if !(2..=3).contains(&parts.len()) {
        return None;
    }
    parts.iter().try_fold(0u32, |acc, p| {
        if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        acc.checked_mul(60)?.checked_add(p.parse().ok()?)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_duration_minutes() {
        assert_eq!(parse_duration("3:45"), Some(225));
    }

    #[test]
    fn parse_duration_hours() {
        assert_eq!(parse_duration("1:02:03"), Some(3723));
    }

    #[test]
    fn parse_duration_garbage() {
        assert_eq!(parse_duration(""), None);
        assert_eq!(parse_duration("abc"), None);
        assert_eq!(parse_duration("3:xx"), None);
    }

    #[test]
    fn song_serializes_camel_case() {
        let song = Song {
            video_id: "abc123".into(),
            title: "One More Time".into(),
            artists: "Daft Punk".into(),
            album: Some("Discovery".into()),
            duration_secs: Some(225),
            artwork_url: None,
        };
        let v = serde_json::to_value(&song).unwrap();
        assert_eq!(v["videoId"], "abc123");
        assert_eq!(v["durationSecs"], 225);
    }
}
