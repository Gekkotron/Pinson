//! Home mixes: the YouTube Music radio of songs played recently or favorited.

use std::collections::HashSet;
use std::sync::Arc;

use crate::catalog::{Collection, CollectionKind, CollectionSummary};
use crate::history::{History, MIX_SEED_WINDOW};
use crate::radio::{RADIO_TIMEOUT, Radio};
use crate::song::Song;

/// Most mixes shown on Home.
pub const MAX_MIXES: usize = 6;

/// Most songs in one mix.
pub const MIX_SONGS: usize = 25;

/// The first artist of `artists`, lowercase; empty when blank.
pub fn artist_key(artists: &str) -> String {
    artists
        .split([',', '&'])
        .next()
        .unwrap_or("")
        .trim()
        .to_lowercase()
}

/// Seeds for the mixes: recent distinct songs with different artists first,
/// then same-artist repeats, then favorites to fill in.
pub fn pick_seeds(history: &[Song], favorites: &[Song], max: usize) -> Vec<Song> {
    let mut seen_ids = HashSet::new();
    let mut seen_artists = HashSet::new();
    let mut out = Vec::new();
    for source in [history, favorites] {
        let mut fresh = Vec::new();
        let mut rest = Vec::new();
        for s in source {
            if !seen_ids.insert(s.video_id.clone()) {
                continue;
            }
            if seen_artists.insert(artist_key(&s.artists)) {
                fresh.push(s.clone());
            } else {
                rest.push(s.clone());
            }
        }
        out.extend(fresh);
        out.extend(rest);
    }
    out.truncate(max);
    out
}

/// "A, B, C…": the artists of `songs`, each once. Songs are compared by
/// [`artist_key`], but the whole trimmed `artists` text is shown.
pub fn mix_subtitle(songs: &[Song]) -> Option<String> {
    let mut seen = HashSet::new();
    let mut names: Vec<String> = Vec::new();
    for s in songs {
        let key = artist_key(&s.artists);
        if key.is_empty() || !seen.insert(key) {
            continue;
        }
        names.push(s.artists.trim().to_owned());
    }
    if names.is_empty() {
        return None;
    }
    let more = names.len() > 3;
    names.truncate(3);
    let mut out = names.join(", ");
    if more {
        out.push('…');
    }
    Some(out)
}

/// The mix for `seed` from its `radio`; `None` when the radio is empty.
/// `id` is the requested seed id; `seed` gives the title and cover.
pub fn build_mix(id: &str, seed: &Song, radio: Vec<Song>) -> Option<Collection> {
    let mut seen = HashSet::new();
    let mut songs: Vec<Song> = radio
        .into_iter()
        .filter(|s| seen.insert(s.video_id.clone()))
        .collect();
    songs.truncate(MIX_SONGS);
    let first = songs.first()?;
    let artwork_url = seed
        .artwork_url
        .clone()
        .or_else(|| first.artwork_url.clone());
    Some(Collection {
        summary: CollectionSummary {
            kind: CollectionKind::Mix,
            id: id.to_owned(),
            title: format!("{} radio", seed.title),
            subtitle: mix_subtitle(&songs),
            artwork_url,
        },
        songs,
    })
}

async fn recent(history: History) -> anyhow::Result<Vec<Song>> {
    tokio::task::spawn_blocking(move || history.recent(MIX_SEED_WINDOW)).await?
}

/// Up to [`MAX_MIXES`] mixes, in seed order. Radios that fail, time out or
/// are empty are left out.
pub async fn home_mixes(
    radio: Arc<dyn Radio>,
    history: History,
    favorites: Vec<Song>,
) -> anyhow::Result<Vec<Collection>> {
    let history = recent(history).await?;
    let seeds = pick_seeds(&history, &favorites, MAX_MIXES);
    let fetches = seeds.iter().map(|seed| {
        let radio = radio.clone();
        async move { tokio::time::timeout(RADIO_TIMEOUT, radio.radio(&seed.video_id)).await }
    });
    let results = futures_util::future::join_all(fetches).await;
    let mut out = Vec::new();
    for (seed, result) in seeds.iter().zip(results) {
        match result {
            Ok(Ok(songs)) => match build_mix(&seed.video_id, seed, songs) {
                Some(mix) => out.push(mix),
                None => tracing::debug!(seed = %seed.video_id, "mix skipped: empty radio"),
            },
            Ok(Err(e)) => tracing::debug!(seed = %seed.video_id, "mix skipped: {e:#}"),
            Err(_) => tracing::debug!(seed = %seed.video_id, "mix skipped: radio timed out"),
        }
    }
    Ok(out)
}

/// The mix seeded by `video_id`. `Ok(None)` means the radio is empty.
pub async fn mix(
    radio: Arc<dyn Radio>,
    history: History,
    favorites: Vec<Song>,
    video_id: &str,
) -> anyhow::Result<Option<Collection>> {
    let songs = tokio::time::timeout(RADIO_TIMEOUT, radio.radio(video_id))
        .await
        .map_err(|_| anyhow::anyhow!("radio for {video_id} timed out"))??;
    let history = recent(history).await.unwrap_or_else(|e| {
        tracing::warn!("mix: reading history failed, using favorites and the radio: {e:#}");
        Vec::new()
    });
    let seed = history
        .iter()
        .chain(favorites.iter())
        .find(|s| s.video_id == video_id)
        .or_else(|| songs.iter().find(|s| s.video_id == video_id))
        .or_else(|| songs.first())
        .cloned();
    Ok(seed.and_then(|seed| build_mix(video_id, &seed, songs)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::radio::fake::FakeRadio;
    use std::time::Duration;
    use tokio::time::Instant;

    fn song(id: &str, artist: &str) -> Song {
        Song {
            video_id: id.into(),
            title: format!("Title {id}"),
            artists: artist.into(),
            album: None,
            duration_secs: Some(60),
            artwork_url: Some(format!("https://art/{id}")),
        }
    }

    fn ids(songs: &[Song]) -> Vec<&str> {
        songs.iter().map(|s| s.video_id.as_str()).collect()
    }

    fn history_of(songs: &[Song]) -> History {
        let h = History::new(Db::in_memory());
        // recorded oldest first, so the last one is the newest
        for s in songs.iter().rev() {
            h.record(s).unwrap();
        }
        h
    }

    #[test]
    fn artist_key_takes_the_first_artist() {
        assert_eq!(artist_key("Daft Punk, Pharrell Williams"), "daft punk");
        assert_eq!(artist_key("Simon & Garfunkel"), "simon");
        assert_eq!(artist_key("  "), "");
    }

    #[test]
    fn seeds_are_distinct_songs() {
        let a = song("A", "x");
        let b = song("B", "y");
        let seeds = pick_seeds(&[a.clone(), a.clone(), a.clone(), b.clone()], &[], 6);
        assert_eq!(seeds, vec![a, b]);
    }

    #[test]
    fn seeds_prefer_new_artists() {
        let a = song("A", "x");
        let b = song("B", "x");
        let c = song("C", "y");
        let seeds = pick_seeds(&[a.clone(), b.clone(), c.clone()], &[], 6);
        assert_eq!(seeds, vec![a, c, b]);
    }

    #[test]
    fn seeds_fill_from_favorites() {
        let a = song("A", "x");
        let f1 = song("F1", "y");
        let f2 = song("F2", "z");
        let seeds = pick_seeds(
            std::slice::from_ref(&a),
            &[f1.clone(), a.clone(), f2.clone()],
            6,
        );
        assert_eq!(seeds, vec![a, f1, f2]);
    }

    #[test]
    fn seeds_keep_history_before_favorites() {
        let hist: Vec<Song> = (0..6).map(|i| song(&format!("H{i}"), "x")).collect();
        let seeds = pick_seeds(&hist, &[song("F", "y")], 6);
        assert_eq!(seeds, hist);
    }

    #[test]
    fn seeds_empty_without_history_or_favorites() {
        assert!(pick_seeds(&[], &[], 6).is_empty());
    }

    #[test]
    fn subtitle_lists_three_artists_then_ellipsis() {
        let songs: Vec<Song> = ["M83", "Phoenix", "m83", "MGMT", "Air"]
            .iter()
            .enumerate()
            .map(|(i, a)| song(&i.to_string(), a))
            .collect();
        assert_eq!(mix_subtitle(&songs).as_deref(), Some("M83, Phoenix, MGMT…"));
        assert_eq!(mix_subtitle(&songs[..2]).as_deref(), Some("M83, Phoenix"));
        assert_eq!(mix_subtitle(&[]), None);
    }

    #[test]
    fn subtitle_keeps_whole_artist_names() {
        let songs = [song("1", "Simon & Garfunkel"), song("2", "M83")];
        assert_eq!(
            mix_subtitle(&songs).as_deref(),
            Some("Simon & Garfunkel, M83")
        );
        // same first artist: compared by key, shown once
        let songs = [
            song("1", " Bob Marley & The Wailers "),
            song("2", "Bob Marley"),
        ];
        assert_eq!(
            mix_subtitle(&songs).as_deref(),
            Some("Bob Marley & The Wailers")
        );
    }

    #[test]
    fn build_mix_dedupes_cuts_and_titles() {
        let mut seed = song("S", "M83");
        seed.title = "Midnight City".into();
        let mut radio: Vec<Song> = (0..29).map(|i| song(&format!("r{i}"), "M83")).collect();
        radio.insert(5, song("r0", "M83"));
        assert_eq!(radio.len(), 30);
        let mix = build_mix("S", &seed, radio).unwrap();
        assert_eq!(mix.songs.len(), MIX_SONGS);
        assert_eq!(mix.songs[0].video_id, "r0");
        assert_eq!(mix.songs[5].video_id, "r5");
        assert_eq!(mix.summary.title, "Midnight City radio");
        assert_eq!(mix.summary.kind, CollectionKind::Mix);
        assert_eq!(mix.summary.id, "S");
        assert_eq!(mix.summary.artwork_url.as_deref(), Some("https://art/S"));
    }

    #[test]
    fn build_mix_artwork_falls_back_to_first_song() {
        let mut seed = song("S", "x");
        seed.artwork_url = None;
        let mix = build_mix("S", &seed, vec![song("r0", "x"), song("r1", "x")]).unwrap();
        assert_eq!(mix.summary.artwork_url.as_deref(), Some("https://art/r0"));
    }

    #[test]
    fn build_mix_empty_radio_is_none() {
        assert!(build_mix("S", &song("S", "x"), vec![]).is_none());
    }

    fn abc() -> (FakeRadio, History) {
        let radio = FakeRadio::default();
        let hist = history_of(&[song("A", "x"), song("B", "y"), song("C", "z")]);
        for id in ["A", "B", "C"] {
            radio.set(id, vec![song(id, "q"), song(&format!("{id}2"), "q")]);
        }
        (radio, hist)
    }

    #[tokio::test(start_paused = true)]
    async fn home_mixes_keep_seed_order_and_skip_failures() {
        let (radio, hist) = abc();
        radio.fail("B");
        let mixes = home_mixes(Arc::new(radio), hist, vec![]).await.unwrap();
        let got: Vec<&str> = mixes.iter().map(|m| m.summary.id.as_str()).collect();
        assert_eq!(got, ["A", "C"]);
    }

    #[tokio::test(start_paused = true)]
    async fn home_mixes_skip_empty_radios() {
        let (radio, hist) = abc();
        radio.set("B", vec![]);
        let mixes = home_mixes(Arc::new(radio), hist, vec![]).await.unwrap();
        let got: Vec<&str> = mixes.iter().map(|m| m.summary.id.as_str()).collect();
        assert_eq!(got, ["A", "C"]);
    }

    #[tokio::test(start_paused = true)]
    async fn home_mixes_skip_a_seed_that_times_out() {
        let (radio, hist) = abc();
        // B answers late but in time, C never does: fetches run at once, so
        // the total is one RADIO_TIMEOUT. A sequential loop would take longer.
        radio.delay("B", RADIO_TIMEOUT - Duration::from_secs(1));
        radio.delay("C", RADIO_TIMEOUT + Duration::from_secs(1));
        let start = Instant::now();
        let mixes = home_mixes(Arc::new(radio), hist, vec![]).await.unwrap();
        let got: Vec<&str> = mixes.iter().map(|m| m.summary.id.as_str()).collect();
        assert_eq!(got, ["A", "B"]);
        assert_eq!(start.elapsed(), RADIO_TIMEOUT);
    }

    #[tokio::test(start_paused = true)]
    async fn home_mixes_empty_without_seeds() {
        let radio = FakeRadio::default();
        let hist = History::new(Db::in_memory());
        let mixes = home_mixes(Arc::new(radio.clone()), hist, vec![])
            .await
            .unwrap();
        assert!(mixes.is_empty());
        assert!(radio.calls().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn mix_for_unknown_seed_uses_the_radio() {
        let radio = FakeRadio::default();
        let mut z = song("Z", "x");
        z.title = "Zed".into();
        radio.set("Z", vec![z, song("y", "x")]);
        let hist = History::new(Db::in_memory());
        let m = mix(Arc::new(radio), hist, vec![], "Z")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(m.summary.title, "Zed radio");
        assert_eq!(ids(&m.songs), ["Z", "y"]);
    }

    #[tokio::test(start_paused = true)]
    async fn mix_prefers_the_history_seed_title() {
        let radio = FakeRadio::default();
        let mut real = song("A", "x");
        real.title = "Real title".into();
        let mut other = song("A", "x");
        other.title = "Other".into();
        radio.set("A", vec![other, song("b", "x")]);
        let hist = history_of(&[real]);
        let m = mix(Arc::new(radio), hist, vec![], "A")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(m.summary.title, "Real title radio");
    }

    #[tokio::test(start_paused = true)]
    async fn mix_id_is_the_requested_id_even_when_the_radio_lacks_the_seed() {
        let radio = FakeRadio::default();
        radio.set("Q", vec![song("r0", "x"), song("r1", "x")]);
        let hist = History::new(Db::in_memory());
        let m = mix(Arc::new(radio), hist, vec![], "Q")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(m.summary.id, "Q");
        assert_eq!(m.summary.title, "Title r0 radio");
    }

    #[tokio::test(start_paused = true)]
    async fn mix_opens_when_reading_history_fails() {
        let radio = FakeRadio::default();
        let mut fav = song("A", "x");
        fav.title = "Fav".into();
        radio.set("A", vec![song("r0", "x")]);
        let db = Db::in_memory();
        db.with(|c| c.execute_batch("DROP TABLE history")).unwrap();
        let m = mix(Arc::new(radio), History::new(db), vec![fav], "A")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(m.summary.title, "Fav radio");
        assert_eq!(m.summary.id, "A");
    }
}
