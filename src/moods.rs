//! YouTube Music "Moods & genres": categories and their featured playlists.
//!
//! The pages are read as raw JSON: `ytmapi-rs` rejects a whole mood page
//! when one shelf (e.g. "Songs") has an item shape it does not know.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tokio::time::Instant;
use ytmapi_rs::YtMusic;
use ytmapi_rs::auth::AuthToken;
use ytmapi_rs::common::{MoodCategoryParams, Thumbnail, YoutubeID};
use ytmapi_rs::query::{GetMoodCategoriesQuery, GetMoodPlaylistsQuery};

use crate::catalog::{CollectionKind, CollectionSummary, api_playlist_id, best_artwork};
use crate::search::{Client, YtmSearcher};

/// How long fetched moods are served from memory.
pub const MOODS_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// One mood or genre ("Chill", "Pop") and the token that opens its page.
#[derive(Clone, Debug, PartialEq)]
pub struct MoodCategory {
    pub title: String,
    pub params: String,
}

#[async_trait]
pub trait Moods: Send + Sync + 'static {
    /// Every mood and genre, in YouTube Music's order.
    async fn categories(&self) -> anyhow::Result<Vec<MoodCategory>>;
    /// The featured playlists of the category with `params`, in YouTube
    /// Music's order. Ids come without the `VL` prefix.
    async fn playlists(&self, params: &str) -> anyhow::Result<Vec<CollectionSummary>>;
}

fn sections(page: &Value) -> impl Iterator<Item = &Value> {
    page.pointer(
        "/contents/singleColumnBrowseResultsRenderer/tabs/0/tabRenderer/content/sectionListRenderer/contents",
    )
    .and_then(Value::as_array)
    .into_iter()
    .flatten()
}

fn first_text(v: &Value) -> Option<&str> {
    v.pointer("/runs/0/text")?.as_str()
}

/// The categories of the "Moods & genres" page, in page order.
pub fn categories_from_page(page: &Value) -> Vec<MoodCategory> {
    sections(page)
        .filter_map(|s| s.pointer("/gridRenderer/items")?.as_array())
        .flatten()
        .filter_map(|item| {
            let b = item.get("musicNavigationButtonRenderer")?;
            let title = first_text(b.get("buttonText")?)?.trim();
            let params = b.pointer("/clickCommand/browseEndpoint/params")?.as_str()?;
            (!title.is_empty() && !params.is_empty()).then(|| MoodCategory {
                title: title.to_owned(),
                params: params.to_owned(),
            })
        })
        .collect()
}

fn playlist_from_item(item: &Value) -> Option<CollectionSummary> {
    let r = item.get("musicTwoRowItemRenderer")?;
    let browse_id = r
        .pointer("/navigationEndpoint/browseEndpoint/browseId")?
        .as_str()?;
    browse_id.starts_with("VL").then_some(())?;
    let title = first_text(r.get("title")?)?.trim();
    if title.is_empty() {
        return None;
    }
    let thumbnails: Vec<Thumbnail> = r
        .pointer("/thumbnailRenderer/musicThumbnailRenderer/thumbnail/thumbnails")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|t| serde_json::from_value(t.clone()).ok())
        .collect();
    Some(CollectionSummary {
        kind: CollectionKind::Playlist,
        id: api_playlist_id(browse_id).to_owned(),
        title: title.to_owned(),
        subtitle: None,
        artwork_url: best_artwork(&thumbnails),
    })
}

/// The playlists of the first shelf that holds any, in page order.
pub fn featured_from_page(page: &Value) -> Vec<CollectionSummary> {
    sections(page)
        .filter_map(|s| {
            let shelf = s
                .get("musicCarouselShelfRenderer")
                .or_else(|| s.get("gridRenderer"))?;
            shelf
                .get("contents")
                .or_else(|| shelf.get("items"))?
                .as_array()
        })
        .map(|items| {
            items
                .iter()
                .filter_map(playlist_from_item)
                .collect::<Vec<_>>()
        })
        .find(|playlists| !playlists.is_empty())
        .unwrap_or_default()
}

async fn categories_of<A: AuthToken>(yt: &YtMusic<A>) -> anyhow::Result<Vec<MoodCategory>> {
    let json = yt.raw_json_query(GetMoodCategoriesQuery).await?;
    Ok(categories_from_page(&serde_json::from_str(&json)?))
}

async fn playlists_of<A: AuthToken>(
    yt: &YtMusic<A>,
    params: &str,
) -> anyhow::Result<Vec<CollectionSummary>> {
    let query = GetMoodPlaylistsQuery::new(MoodCategoryParams::from_raw(params));
    let json = yt.raw_json_query(query).await?;
    Ok(featured_from_page(&serde_json::from_str(&json)?))
}

#[async_trait]
impl Moods for YtmSearcher {
    async fn categories(&self) -> anyhow::Result<Vec<MoodCategory>> {
        match self.client().await? {
            Client::Anonymous(yt) => categories_of(&yt).await,
            Client::Browser(yt) => categories_of(&yt).await,
        }
    }

    async fn playlists(&self, params: &str) -> anyhow::Result<Vec<CollectionSummary>> {
        match self.client().await? {
            Client::Anonymous(yt) => playlists_of(&yt, params).await,
            Client::Browser(yt) => playlists_of(&yt, params).await,
        }
    }
}

/// Remembers the categories and each category's playlists for `ttl`.
/// Failures are not remembered.
pub struct MoodsCache {
    inner: Arc<dyn Moods>,
    ttl: Duration,
    categories: Mutex<Option<(Instant, Vec<MoodCategory>)>>,
    playlists: Mutex<HashMap<String, (Instant, Vec<CollectionSummary>)>>,
}

impl MoodsCache {
    pub fn new(inner: Arc<dyn Moods>, ttl: Duration) -> MoodsCache {
        MoodsCache {
            inner,
            ttl,
            categories: Mutex::new(None),
            playlists: Mutex::new(HashMap::new()),
        }
    }
}

#[async_trait]
impl Moods for MoodsCache {
    async fn categories(&self) -> anyhow::Result<Vec<MoodCategory>> {
        if let Some((at, found)) = &*self.categories.lock().unwrap()
            && at.elapsed() < self.ttl
        {
            return Ok(found.clone());
        }
        let found = self.inner.categories().await?;
        *self.categories.lock().unwrap() = Some((Instant::now(), found.clone()));
        Ok(found)
    }

    async fn playlists(&self, params: &str) -> anyhow::Result<Vec<CollectionSummary>> {
        if let Some((at, found)) = self.playlists.lock().unwrap().get(params)
            && at.elapsed() < self.ttl
        {
            return Ok(found.clone());
        }
        let found = self.inner.playlists(params).await?;
        let mut map = self.playlists.lock().unwrap();
        map.retain(|_, (at, _)| at.elapsed() < self.ttl);
        map.insert(params.to_owned(), (Instant::now(), found.clone()));
        Ok(found)
    }
}

/// A scriptable moods source for tests.
pub mod fake {
    use std::collections::HashSet;

    use super::*;

    #[derive(Default)]
    struct State {
        categories: Vec<MoodCategory>,
        categories_failing: bool,
        playlists: HashMap<String, Vec<CollectionSummary>>,
        playlists_failing: HashSet<String>,
        calls: Vec<String>,
    }

    /// Answers from what the setters were given; params never set give no
    /// playlists.
    #[derive(Clone, Default)]
    pub struct FakeMoods {
        state: Arc<Mutex<State>>,
    }

    impl FakeMoods {
        pub fn set_categories(&self, categories: Vec<MoodCategory>) {
            let mut s = self.state.lock().unwrap();
            s.categories_failing = false;
            s.categories = categories;
        }

        pub fn fail_categories(&self) {
            self.state.lock().unwrap().categories_failing = true;
        }

        pub fn set_playlists(&self, params: &str, playlists: Vec<CollectionSummary>) {
            let mut s = self.state.lock().unwrap();
            s.playlists_failing.remove(params);
            s.playlists.insert(params.to_owned(), playlists);
        }

        pub fn fail_playlists(&self, params: &str) {
            self.state
                .lock()
                .unwrap()
                .playlists_failing
                .insert(params.to_owned());
        }

        /// Every call, in order: `"categories"` or `"playlists:<params>"`.
        pub fn calls(&self) -> Vec<String> {
            self.state.lock().unwrap().calls.clone()
        }
    }

    #[async_trait]
    impl Moods for FakeMoods {
        async fn categories(&self) -> anyhow::Result<Vec<MoodCategory>> {
            let mut s = self.state.lock().unwrap();
            s.calls.push("categories".to_owned());
            if s.categories_failing {
                anyhow::bail!("categories failed");
            }
            Ok(s.categories.clone())
        }

        async fn playlists(&self, params: &str) -> anyhow::Result<Vec<CollectionSummary>> {
            let mut s = self.state.lock().unwrap();
            s.calls.push(format!("playlists:{params}"));
            if s.playlists_failing.contains(params) {
                anyhow::bail!("playlists for {params} failed");
            }
            Ok(s.playlists.get(params).cloned().unwrap_or_default())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn page(sections: Vec<serde_json::Value>) -> serde_json::Value {
        json!({"contents": {"singleColumnBrowseResultsRenderer": {"tabs": [{"tabRenderer": {
            "content": {"sectionListRenderer": {"contents": sections}}}}]}}})
    }

    fn grid(title: &str, buttons: &[(&str, &str)]) -> serde_json::Value {
        let items: Vec<_> = buttons
            .iter()
            .map(|(t, p)| {
                json!({"musicNavigationButtonRenderer": {
                    "buttonText": {"runs": [{"text": t}]},
                    "clickCommand": {"browseEndpoint": {"params": p}}}})
            })
            .collect();
        json!({"gridRenderer": {
            "header": {"gridHeaderRenderer": {"title": {"runs": [{"text": title}]}}},
            "items": items}})
    }

    fn playlist_item(browse_id: &str, title: Option<&str>) -> serde_json::Value {
        let mut r = json!({
            "navigationEndpoint": {"browseEndpoint": {"browseId": browse_id}},
            "thumbnailRenderer": {"musicThumbnailRenderer": {"thumbnail": {"thumbnails": [
                {"url": "https://lh3.googleusercontent.com/a=w60-h60", "width": 60, "height": 60},
                {"url": "https://lh3.googleusercontent.com/a=w226-h226", "width": 226, "height": 226}
            ]}}}
        });
        if let Some(t) = title {
            r["title"] = json!({"runs": [{"text": t}]});
        }
        json!({"musicTwoRowItemRenderer": r})
    }

    fn shelf(items: Vec<serde_json::Value>) -> serde_json::Value {
        json!({"musicCarouselShelfRenderer": {"contents": items}})
    }

    fn cat(title: &str, params: &str) -> MoodCategory {
        MoodCategory {
            title: title.into(),
            params: params.into(),
        }
    }

    #[test]
    fn categories_read_moods_and_genres_sections() {
        let p = page(vec![
            grid("Moods & moments", &[("Chill", "p-chill")]),
            grid("Genres", &[("Pop", "p-pop"), ("French rap", "p-fr")]),
        ]);
        assert_eq!(
            categories_from_page(&p),
            vec![
                cat("Chill", "p-chill"),
                cat("Pop", "p-pop"),
                cat("French rap", "p-fr")
            ]
        );
    }

    #[test]
    fn categories_of_an_unknown_page_are_empty() {
        assert!(categories_from_page(&json!({})).is_empty());
    }

    #[test]
    fn featured_reads_the_first_playlist_shelf() {
        let p = page(vec![
            shelf(vec![
                playlist_item("VLRDCLAK5uy_a", Some("Pop's Biggest Hits")),
                playlist_item("VLRDCLAK5uy_b", Some("Other")),
            ]),
            shelf(vec![playlist_item("VLPLzzz", Some("Community"))]),
        ]);
        let got = featured_from_page(&p);
        assert_eq!(
            got.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            ["RDCLAK5uy_a", "RDCLAK5uy_b"]
        );
        assert_eq!(got[0].kind, CollectionKind::Playlist);
        assert_eq!(got[0].title, "Pop's Biggest Hits");
        assert_eq!(got[0].subtitle, None);
        assert_eq!(
            got[0].artwork_url.as_deref(),
            Some("https://lh3.googleusercontent.com/a=w1200-h1200")
        );
    }

    #[test]
    fn featured_skips_non_playlist_shelves_and_items() {
        let song = json!({"musicResponsiveListItemRenderer": {"flexColumns": []}});
        let video = json!({"musicTwoRowItemRenderer": {
            "title": {"runs": [{"text": "Clip"}]},
            "navigationEndpoint": {"watchEndpoint": {"videoId": "abc"}}}});
        let p = page(vec![
            shelf(vec![song]),
            shelf(vec![video]),
            shelf(vec![
                playlist_item("VLRDCLAK5uy_x", None),
                playlist_item("VLRDCLAK5uy_good", Some("Good")),
            ]),
        ]);
        let got = featured_from_page(&p);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, "RDCLAK5uy_good");
    }

    use super::fake::FakeMoods;

    fn pl(id: &str) -> CollectionSummary {
        CollectionSummary {
            kind: CollectionKind::Playlist,
            id: id.into(),
            title: format!("t{id}"),
            subtitle: None,
            artwork_url: None,
        }
    }

    #[tokio::test]
    async fn cache_serves_categories_and_playlists_from_memory() {
        let fake = FakeMoods::default();
        fake.set_categories(vec![cat("Pop", "p1")]);
        fake.set_playlists("p1", vec![pl("a")]);
        let cache = MoodsCache::new(Arc::new(fake.clone()), MOODS_TTL);
        for _ in 0..2 {
            assert_eq!(cache.categories().await.unwrap(), vec![cat("Pop", "p1")]);
            assert_eq!(cache.playlists("p1").await.unwrap(), vec![pl("a")]);
        }
        assert_eq!(fake.calls(), ["categories", "playlists:p1"]);
    }

    #[tokio::test]
    async fn cache_does_not_remember_failures() {
        let fake = FakeMoods::default();
        fake.fail_categories();
        let cache = MoodsCache::new(Arc::new(fake.clone()), MOODS_TTL);
        assert!(cache.categories().await.is_err());
        fake.set_categories(vec![cat("Pop", "p1")]);
        assert_eq!(cache.categories().await.unwrap(), vec![cat("Pop", "p1")]);
        assert_eq!(fake.calls(), ["categories", "categories"]);
    }

    #[tokio::test(start_paused = true)]
    async fn cache_refetches_after_ttl() {
        let fake = FakeMoods::default();
        let ttl = Duration::from_secs(3600);
        let cache = MoodsCache::new(Arc::new(fake.clone()), ttl);
        cache.categories().await.unwrap();
        tokio::time::sleep(ttl + Duration::from_secs(1)).await;
        cache.categories().await.unwrap();
        assert_eq!(fake.calls(), ["categories", "categories"]);
    }

    #[tokio::test]
    #[ignore = "hits YouTube Music"]
    async fn live_moods_have_pop_playlists() {
        let searcher = YtmSearcher::new().await.expect("connect to YouTube Music");
        let categories = searcher.categories().await.expect("categories");
        let pop = categories
            .iter()
            .find(|c| c.title == "Pop")
            .unwrap_or_else(|| panic!("no Pop in {categories:?}"));
        let playlists = searcher.playlists(&pop.params).await.expect("playlists");
        assert!(!playlists.is_empty());
        assert!(
            playlists.iter().all(|p| !p.id.starts_with("VL")),
            "{playlists:?}"
        );
    }
}
