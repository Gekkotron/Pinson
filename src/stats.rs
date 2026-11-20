//! Listening stats: a pure aggregation over recorded plays (see `History::since`).

use std::collections::HashMap;

use chrono::{DateTime, Duration, Timelike, Utc};
use chrono_tz::Tz;
use serde::Serialize;

use crate::song::Song;

/// How many songs and artists the top lists hold.
const TOP: usize = 10;

/// Separators between credited artists. Not " x " or " and ": they sit inside band names
/// like "Simon and Garfunkel".
const SEPARATORS: [&str; 4] = [", ", " & ", " feat. ", " ft. "];

/// A rolling window of listening history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Period {
    Week,
    Month,
    Year,
    All,
}

impl Period {
    pub fn parse(s: &str) -> Option<Period> {
        match s {
            "week" => Some(Period::Week),
            "month" => Some(Period::Month),
            "year" => Some(Period::Year),
            "all" => Some(Period::All),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Period::Week => "week",
            Period::Month => "month",
            Period::Year => "year",
            Period::All => "all",
        }
    }

    /// Length of the window; `None` for all time.
    pub fn window(self) -> Option<Duration> {
        match self {
            Period::Week => Some(Duration::days(7)),
            Period::Month => Some(Duration::days(30)),
            Period::Year => Some(Duration::days(365)),
            Period::All => None,
        }
    }

    /// Start of the window ending at `now`; `None` for all time.
    pub fn cutoff(self, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        self.window().map(|w| now - w)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TopSong {
    pub song: Song,
    pub plays: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TopArtist {
    pub name: String,
    pub plays: u32,
    pub artwork_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Stats {
    pub period: &'static str,
    pub plays: u32,
    pub listened_secs: u64,
    pub songs: u32,
    pub artists: u32,
    pub top_songs: Vec<TopSong>,
    pub top_artists: Vec<TopArtist>,
    pub by_hour: [u32; 24],
}

/// Splits a credit like "Daft Punk, Pharrell & Nile feat. X" into its artists.
pub fn split_artists(credit: &str) -> Vec<&str> {
    // ASCII lowercasing keeps byte offsets, so matches in `lower` index into `credit`.
    let lower = credit.to_ascii_lowercase();
    let mut out = Vec::new();
    let mut start = 0;
    while let Some((at, len)) = SEPARATORS
        .iter()
        .filter_map(|sep| lower[start..].find(sep).map(|i| (start + i, sep.len())))
        .min_by_key(|(i, _)| *i)
    {
        out.push(&credit[start..at]);
        start = at + len;
    }
    out.push(&credit[start..]);
    out.into_iter()
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .collect()
}

struct SongAgg<'a> {
    plays: u32,
    /// Rank of the most recent play (0 = newest).
    latest: usize,
    song: &'a Song,
}

#[derive(Default)]
struct ArtistAgg<'a> {
    plays: u32,
    latest: usize,
    name: &'a str,
    /// Per song: (plays by this artist, rank of its most recent play, its artwork).
    songs: HashMap<&'a str, (u32, usize, Option<&'a str>)>,
}

/// Aggregates `plays` (`(played_at RFC 3339, song)`, any order) with hours in `tz`.
pub fn compute(plays: &[(String, Song)], tz: Tz, period: Period) -> Stats {
    let mut rows: Vec<(Option<DateTime<Utc>>, &Song)> = plays
        .iter()
        .map(|(at, song)| {
            let at = DateTime::parse_from_rfc3339(at)
                .ok()
                .map(|d| d.with_timezone(&Utc));
            (at, song)
        })
        .collect();
    // Newest first; a stable sort keeps the input order for equal or unparsable times.
    rows.sort_by_key(|r| std::cmp::Reverse(r.0));

    let mut listened_secs = 0u64;
    let mut by_hour = [0u32; 24];
    let mut songs: HashMap<&str, SongAgg> = HashMap::new();
    let mut artists: HashMap<String, ArtistAgg> = HashMap::new();

    for (rank, (at, song)) in rows.iter().enumerate() {
        listened_secs += u64::from(song.duration_secs.unwrap_or(0));
        if let Some(at) = at {
            by_hour[at.with_timezone(&tz).hour() as usize] += 1;
        }
        let agg = songs.entry(&song.video_id).or_insert(SongAgg {
            plays: 0,
            latest: rank,
            song,
        });
        agg.plays += 1;

        let mut seen: Vec<String> = Vec::new();
        for name in split_artists(&song.artists) {
            let key = name.to_lowercase();
            if seen.contains(&key) {
                continue;
            }
            let a = artists.entry(key.clone()).or_insert_with(|| ArtistAgg {
                latest: rank,
                name,
                ..ArtistAgg::default()
            });
            a.plays += 1;
            let s = a
                .songs
                .entry(&song.video_id)
                .or_insert((0, rank, song.artwork_url.as_deref()));
            s.0 += 1;
            seen.push(key);
        }
    }

    let mut top_songs: Vec<&SongAgg> = songs.values().collect();
    top_songs.sort_by_key(|s| (std::cmp::Reverse(s.plays), s.latest));
    let top_songs = top_songs
        .into_iter()
        .take(TOP)
        .map(|s| TopSong {
            song: s.song.clone(),
            plays: s.plays,
        })
        .collect();

    let mut top_artists: Vec<&ArtistAgg> = artists.values().collect();
    // Artists credited on the same play tie on recency too; fall back to the name.
    top_artists.sort_by_key(|a| (std::cmp::Reverse(a.plays), a.latest, a.name.to_lowercase()));
    let top_artists = top_artists
        .into_iter()
        .take(TOP)
        .map(|a| TopArtist {
            name: a.name.to_string(),
            plays: a.plays,
            artwork_url: a
                .songs
                .values()
                .min_by_key(|(n, latest, _)| (std::cmp::Reverse(*n), *latest))
                .and_then(|(_, _, art)| art.map(str::to_string)),
        })
        .collect();

    Stats {
        period: period.as_str(),
        plays: rows.len() as u32,
        listened_secs,
        songs: songs.len() as u32,
        artists: artists.len() as u32,
        top_songs,
        top_artists,
        by_hour,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn song(id: &str, artists: &str, secs: Option<u32>, art: Option<&str>) -> Song {
        Song {
            video_id: id.into(),
            title: format!("Title {id}"),
            artists: artists.into(),
            album: None,
            duration_secs: secs,
            artwork_url: art.map(str::to_string),
        }
    }

    /// Plays given newest first, one minute apart ending 2026-11-20 12:00 UTC.
    fn plays(songs: Vec<Song>) -> Vec<(String, Song)> {
        let end = DateTime::parse_from_rfc3339("2026-11-20T12:00:00+00:00")
            .unwrap()
            .with_timezone(&Utc);
        songs
            .into_iter()
            .enumerate()
            .map(|(i, s)| ((end - Duration::minutes(i as i64)).to_rfc3339(), s))
            .collect()
    }

    fn names(s: &Stats) -> Vec<(&str, u32)> {
        s.top_artists
            .iter()
            .map(|a| (a.name.as_str(), a.plays))
            .collect()
    }

    #[test]
    fn period_parses_and_windows() {
        assert_eq!(Period::parse("week"), Some(Period::Week));
        assert_eq!(Period::parse("month"), Some(Period::Month));
        assert_eq!(Period::parse("year"), Some(Period::Year));
        assert_eq!(Period::parse("all"), Some(Period::All));
        assert_eq!(Period::parse("decade"), None);
        assert_eq!(Period::Week.window(), Some(Duration::days(7)));
        assert_eq!(Period::Month.window(), Some(Duration::days(30)));
        assert_eq!(Period::Year.window(), Some(Duration::days(365)));
        assert_eq!(Period::All.window(), None);
        assert_eq!(Period::Year.as_str(), "year");
    }

    #[test]
    fn empty_input() {
        let s = compute(&[], Tz::UTC, Period::Month);
        assert_eq!(s.period, "month");
        assert_eq!((s.plays, s.listened_secs, s.songs, s.artists), (0, 0, 0, 0));
        assert!(s.top_songs.is_empty() && s.top_artists.is_empty());
        assert_eq!(s.by_hour, [0; 24]);
    }

    #[test]
    fn counts_plays_time_songs_and_artists() {
        let p = plays(vec![
            song("A", "X", Some(100), None),
            song("B", "Y", None, None),
            song("A", "X", Some(100), None),
            song("C", "x", Some(50), None),
        ]);
        let s = compute(&p, Tz::UTC, Period::All);
        assert_eq!(s.plays, 4);
        assert_eq!(s.listened_secs, 250);
        assert_eq!(s.songs, 3);
        assert_eq!(s.artists, 2);
        assert_eq!(s.by_hour[12], 1);
        assert_eq!(s.by_hour[11], 3);
    }

    #[test]
    fn top_songs_ties_most_recent_first_and_capped() {
        let mut v = vec![song("A", "X", None, None)];
        v.push(song("B", "X", None, None));
        v.push(song("C", "X", None, None));
        v.push(song("C", "X", None, None));
        v.push(song("A", "X", None, None));
        for i in 0..12 {
            v.push(song(&format!("o{i}"), "X", None, None));
        }
        let s = compute(&plays(v), Tz::UTC, Period::All);
        let ids: Vec<(&str, u32)> = s
            .top_songs
            .iter()
            .map(|t| (t.song.video_id.as_str(), t.plays))
            .collect();
        assert_eq!(s.top_songs.len(), 10);
        // A and C both 2 plays; A played most recently. Then singles, newest first.
        assert_eq!(&ids[..4], [("A", 2), ("C", 2), ("B", 1), ("o0", 1)]);
    }

    #[test]
    fn top_song_metadata_from_most_recent_play() {
        let mut newer = song("A", "X", Some(1), None);
        newer.title = "New title".into();
        let mut older = song("A", "X", Some(1), None);
        older.title = "Old title".into();
        let s = compute(&plays(vec![newer, older]), Tz::UTC, Period::All);
        assert_eq!(s.top_songs[0].song.title, "New title");
    }

    #[test]
    fn input_order_does_not_matter() {
        let p = plays(vec![song("A", "X", None, None), song("B", "Y", None, None)]);
        let mut rev = p.clone();
        rev.reverse();
        assert_eq!(
            compute(&p, Tz::UTC, Period::All),
            compute(&rev, Tz::UTC, Period::All)
        );
    }

    #[test]
    fn splits_credits_on_the_listed_separators_only() {
        assert_eq!(
            split_artists("Simon and Garfunkel"),
            ["Simon and Garfunkel"]
        );
        assert_eq!(
            split_artists("Daft Punk & Pharrell"),
            ["Daft Punk", "Pharrell"]
        );
        assert_eq!(split_artists("A, B & C"), ["A", "B", "C"]);
        assert_eq!(split_artists("A feat. B"), ["A", "B"]);
        assert_eq!(split_artists("A Ft. B ft. C"), ["A", "B", "C"]);
        assert_eq!(split_artists("Mike x Dan"), ["Mike x Dan"]);
        assert_eq!(
            split_artists("Tyler, The Creator"),
            ["Tyler", "The Creator"]
        );
        assert_eq!(split_artists(""), Vec::<&str>::new());
    }

    #[test]
    fn each_credited_artist_counts_once_per_play() {
        let p = plays(vec![
            song("A", "Daft Punk & Pharrell", None, None),
            song("B", "Daft Punk", None, None),
            song("C", "Simon and Garfunkel", None, None),
            song("D", "Pharrell, pharrell", None, None),
        ]);
        let s = compute(&p, Tz::UTC, Period::All);
        assert_eq!(s.artists, 3);
        assert_eq!(
            names(&s),
            [
                ("Daft Punk", 2),
                ("Pharrell", 2),
                ("Simon and Garfunkel", 1)
            ]
        );
    }

    #[test]
    fn artists_group_case_insensitively_with_latest_spelling() {
        let p = plays(vec![
            song("A", "DAFT PUNK", None, None),
            song("B", "daft punk", None, None),
            song("C", "Daft Punk", None, None),
        ]);
        let s = compute(&p, Tz::UTC, Period::All);
        assert_eq!(names(&s), [("DAFT PUNK", 3)]);
        assert_eq!(s.artists, 1);
    }

    #[test]
    fn artist_ties_most_recent_first() {
        let p = plays(vec![
            song("A", "New", None, None),
            song("B", "Old", None, None),
        ]);
        assert_eq!(
            names(&compute(&p, Tz::UTC, Period::All)),
            [("New", 1), ("Old", 1)]
        );
    }

    #[test]
    fn artist_artwork_from_most_played_song_ties_most_recent() {
        let p = plays(vec![
            song("A", "X", None, Some("a.jpg")),
            song("B", "X", None, Some("b.jpg")),
            song("B", "X", None, Some("b.jpg")),
            song("C", "X", None, Some("c.jpg")),
            song("C", "X", None, Some("c.jpg")),
        ]);
        let s = compute(&p, Tz::UTC, Period::All);
        // B and C tie on 2 plays; B was played more recently.
        assert_eq!(s.top_artists[0].artwork_url.as_deref(), Some("b.jpg"));
        let p = plays(vec![
            song("A", "X", None, Some("a.jpg")),
            song("B", "X", None, Some("b.jpg")),
            song("B", "X", None, Some("b.jpg")),
        ]);
        let s = compute(&p, Tz::UTC, Period::All);
        assert_eq!(s.top_artists[0].artwork_url.as_deref(), Some("b.jpg"));
        let p = plays(vec![song("A", "X", None, None)]);
        assert_eq!(
            compute(&p, Tz::UTC, Period::All).top_artists[0].artwork_url,
            None
        );
    }

    #[test]
    fn top_artists_capped_at_ten() {
        let v = (0..15)
            .map(|i| song(&format!("s{i}"), &format!("Artist {i}"), None, None))
            .collect();
        let s = compute(&plays(v), Tz::UTC, Period::All);
        assert_eq!(s.top_artists.len(), 10);
        assert_eq!(s.artists, 15);
    }

    #[test]
    fn by_hour_uses_local_time_across_dst() {
        let paris: Tz = "Europe/Paris".parse().unwrap();
        let at = |t: &str| (t.to_string(), song("A", "X", None, None));
        // Paris is UTC+1 on 2026-03-28 and UTC+2 on 2026-03-30 (DST began on the 29th).
        let p = vec![
            at("2026-03-28T22:30:00+00:00"),
            at("2026-03-30T22:30:00+00:00"),
            at("2026-03-29T00:30:00+00:00"),
            at("2026-03-29T01:30:00+00:00"),
        ];
        let s = compute(&p, paris, Period::All);
        assert_eq!(s.by_hour[23], 1);
        assert_eq!(s.by_hour[0], 1);
        assert_eq!(s.by_hour[1], 1);
        // 01:30 UTC on the 29th is 03:30 local (02:xx does not exist that night).
        assert_eq!(s.by_hour[3], 1);
        assert_eq!(s.by_hour.iter().sum::<u32>(), 4);
        let utc = compute(&p, Tz::UTC, Period::All);
        assert_eq!(utc.by_hour[22], 2);
    }

    #[test]
    fn serializes_camel_case() {
        let p = plays(vec![song("A", "X", Some(60), Some("a.jpg"))]);
        let v = serde_json::to_value(compute(&p, Tz::UTC, Period::Week)).unwrap();
        for key in [
            "period",
            "plays",
            "listenedSecs",
            "songs",
            "artists",
            "topSongs",
            "topArtists",
            "byHour",
        ] {
            assert!(v.get(key).is_some(), "missing {key}");
        }
        assert_eq!(v["period"], "week");
        assert_eq!(v["topArtists"][0]["artworkUrl"], "a.jpg");
        assert_eq!(v["topSongs"][0]["song"]["videoId"], "A");
        assert_eq!(v["byHour"].as_array().unwrap().len(), 24);
    }
}
