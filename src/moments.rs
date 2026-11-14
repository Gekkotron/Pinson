//! "Right now": one featured YouTube Music playlist per mood that fits the
//! time of day, the same all day and (usually) new each day.

use std::sync::Arc;
use std::time::Duration;

use futures_util::future::join_all;
use serde::Serialize;

use crate::catalog::CollectionSummary;
use crate::moods::{MoodCategory, Moods};

/// How long one category's playlists may take before it is left out.
pub const MOMENT_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Slot {
    Morning,
    Afternoon,
    Evening,
    Night,
}

/// The part of the day for `hour` (0-23).
pub fn slot_for_hour(hour: u8) -> Slot {
    match hour {
        6..=10 => Slot::Morning,
        11..=16 => Slot::Afternoon,
        17..=21 => Slot::Evening,
        _ => Slot::Night,
    }
}

/// The YouTube Music categories (English titles) shown in `slot`, in order.
pub fn slot_categories(slot: Slot) -> &'static [&'static str] {
    match slot {
        Slot::Morning => &["Energize", "Feel good", "Pop", "Commute"],
        Slot::Afternoon => &["Focus", "Pop", "Rock", "Hip-hop"],
        Slot::Evening => &["Party", "Dance & electronic", "French rap", "R&B & soul"],
        Slot::Night => &["Chill", "Jazz", "Sleep", "Indie & alternative"],
    }
}

/// Which of `len` (> 0) playlists `category` shows on `day`. 64-bit FNV-1a,
/// so the pick survives restarts and rebuilds (unlike `DefaultHasher`).
pub fn daily_index(day: &str, category: &str, len: usize) -> usize {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in day
        .bytes()
        .chain(std::iter::once(b'|'))
        .chain(category.bytes())
    {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    (h % len as u64) as usize
}

#[derive(Serialize)]
pub struct Moments {
    pub slot: Slot,
    pub playlists: Vec<CollectionSummary>,
}

/// The playlists to show at `hour` on `day`. A category that is missing,
/// fails, times out or is empty is left out.
pub async fn moments(moods: Arc<dyn Moods>, day: &str, hour: u8) -> Moments {
    let slot = slot_for_hour(hour);
    let categories = match moods.categories().await {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!("moods unavailable: {e}");
            return Moments {
                slot,
                playlists: vec![],
            };
        }
    };
    let found: Vec<&MoodCategory> = slot_categories(slot)
        .iter()
        .filter_map(|want| {
            categories
                .iter()
                .find(|c| c.title.trim().eq_ignore_ascii_case(want))
        })
        .collect();
    let fetched = join_all(found.iter().map(|c| {
        let moods = moods.clone();
        async move {
            match tokio::time::timeout(MOMENT_TIMEOUT, moods.playlists(&c.params)).await {
                Ok(Ok(list)) => list,
                Ok(Err(e)) => {
                    tracing::debug!("moods {} failed: {e}", c.title);
                    vec![]
                }
                Err(_) => {
                    tracing::debug!("moods {} timed out", c.title);
                    vec![]
                }
            }
        }
    }))
    .await;

    let mut picked: Vec<CollectionSummary> = Vec::new();
    for (category, list) in found.iter().zip(fetched) {
        if list.is_empty() {
            continue;
        }
        let start = daily_index(day, &category.title, list.len());
        let choice = (0..list.len())
            .map(|step| &list[(start + step) % list.len()])
            .find(|p| picked.iter().all(|shown| shown.id != p.id));
        if let Some(p) = choice {
            picked.push(CollectionSummary {
                subtitle: Some(category.title.clone()),
                ..p.clone()
            });
        }
    }
    Moments {
        slot,
        playlists: picked,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::CollectionKind;
    use crate::moods::MoodCategory;
    use crate::moods::fake::FakeMoods;

    fn cat(title: &str) -> MoodCategory {
        MoodCategory {
            title: title.into(),
            params: format!("p-{title}"),
        }
    }

    fn pl(id: &str) -> CollectionSummary {
        CollectionSummary {
            kind: CollectionKind::Playlist,
            id: id.into(),
            title: format!("t{id}"),
            subtitle: None,
            artwork_url: None,
        }
    }

    fn many(prefix: &str, n: usize) -> Vec<CollectionSummary> {
        (0..n).map(|i| pl(&format!("{prefix}{i}"))).collect()
    }

    fn subtitles(m: &Moments) -> Vec<&str> {
        m.playlists
            .iter()
            .map(|p| p.subtitle.as_deref().unwrap())
            .collect()
    }

    #[test]
    fn slot_boundaries() {
        for (hour, slot) in [
            (5, Slot::Night),
            (6, Slot::Morning),
            (10, Slot::Morning),
            (11, Slot::Afternoon),
            (16, Slot::Afternoon),
            (17, Slot::Evening),
            (21, Slot::Evening),
            (22, Slot::Night),
            (23, Slot::Night),
            (0, Slot::Night),
        ] {
            assert_eq!(slot_for_hour(hour), slot, "hour {hour}");
        }
    }

    #[test]
    fn daily_index_is_stable_and_changes_with_the_day() {
        assert_eq!(
            daily_index("2026-11-14", "Pop", 50),
            daily_index("2026-11-14", "Pop", 50)
        );
        let distinct: std::collections::HashSet<usize> = (1..=30)
            .map(|d| {
                let i = daily_index(&format!("2026-11-{d:02}"), "Pop", 100);
                assert!(i < 100);
                i
            })
            .collect();
        assert!(distinct.len() >= 10, "{distinct:?}");
    }

    #[test]
    fn daily_index_is_fixed_across_builds() {
        assert_eq!(daily_index("2026-11-14", "Pop", 109), 104);
    }

    #[tokio::test]
    async fn moments_pick_one_playlist_per_category_in_table_order() {
        let fake = FakeMoods::default();
        let titles = [
            "Rock",
            "R&B & soul",
            "French rap",
            "Dance & electronic",
            "Party",
        ];
        fake.set_categories(titles.iter().map(|t| cat(t)).collect());
        for t in titles {
            fake.set_playlists(&format!("p-{t}"), many(&t.replace(' ', ""), 3));
        }
        let m = moments(Arc::new(fake), "2026-11-14", 19).await;
        assert_eq!(m.slot, Slot::Evening);
        assert_eq!(m.playlists.len(), 4);
        assert_eq!(
            subtitles(&m),
            ["Party", "Dance & electronic", "French rap", "R&B & soul"]
        );
    }

    #[tokio::test]
    async fn picks_skip_a_playlist_already_shown() {
        let day = "2026-11-14";
        let fake = FakeMoods::default();
        fake.set_categories(vec![
            cat("Party"),
            cat("Dance & electronic"),
            cat("French rap"),
        ]);
        // Party has a single playlist, so it always shows "shared".
        fake.set_playlists("p-Party", vec![pl("shared")]);
        // Dance lists "shared" exactly where its daily pick lands.
        let at = daily_index(day, "Dance & electronic", 3);
        let mut dance = many("d", 3);
        dance[at] = pl("shared");
        let next = dance[(at + 1) % 3].id.clone();
        fake.set_playlists("p-Dance & electronic", dance);
        // French rap only has what is already shown.
        fake.set_playlists("p-French rap", vec![pl("shared")]);
        let m = moments(Arc::new(fake), day, 19).await;
        let ids: Vec<&str> = m.playlists.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, ["shared", next.as_str()]);
        assert_eq!(subtitles(&m), ["Party", "Dance & electronic"]);
    }

    #[tokio::test]
    async fn moments_leave_out_missing_failing_and_empty_categories() {
        let fake = FakeMoods::default();
        fake.set_categories(vec![
            cat("Dance & electronic"),
            cat("French rap"),
            cat("R&B & soul"),
        ]);
        fake.set_playlists("p-Dance & electronic", many("d", 2));
        fake.fail_playlists("p-French rap");
        fake.set_playlists("p-R&B & soul", vec![]);
        let m = moments(Arc::new(fake), "2026-11-14", 19).await;
        assert_eq!(subtitles(&m), ["Dance & electronic"]);
    }

    #[tokio::test]
    async fn moments_empty_when_categories_fail() {
        let fake = FakeMoods::default();
        fake.fail_categories();
        let m = moments(Arc::new(fake), "2026-11-14", 8).await;
        assert_eq!(m.slot, Slot::Morning);
        assert!(m.playlists.is_empty());
    }
}
