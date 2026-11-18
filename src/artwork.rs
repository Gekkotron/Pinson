//! Sharp cover art URLs and a restricted image proxy (see `docs/api.md`).

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Duration;

use axum::body::Bytes;
use reqwest::Url;

/// Largest image the proxy will relay.
const MAX_BYTES: usize = 5 * 1024 * 1024;
/// How many recently fetched images are kept in memory.
const CACHE_SIZE: usize = 64;
const HOSTS: [&str; 4] = [
    "lh3.googleusercontent.com",
    "yt3.googleusercontent.com",
    "yt3.ggpht.com",
    "i.ytimg.com",
];

/// Rewrites a `lh3`/`yt3.googleusercontent.com` size suffix (`=w544-h544-…`) to 1200x1200.
/// Any other URL is returned unchanged.
pub fn upscale(url: &str) -> String {
    sized(url, 1200)
}

/// Like [`upscale`], but to `px` x `px`.
pub fn sized(url: &str, px: u32) -> String {
    let Ok(parsed) = Url::parse(url) else {
        return url.to_owned();
    };
    if !matches!(
        parsed.host_str(),
        Some("lh3.googleusercontent.com" | "yt3.googleusercontent.com")
    ) {
        return url.to_owned();
    }
    let Some(eq) = url.rfind('=') else {
        return url.to_owned();
    };
    let (head, params) = url.split_at(eq + 1);
    match strip_size(params) {
        Some(rest) => format!("{head}w{px}-h{px}{rest}"),
        None => url.to_owned(),
    }
}

/// `w<digits>-h<digits><rest>` -> `<rest>`.
fn strip_size(params: &str) -> Option<&str> {
    let rest = params.strip_prefix('w')?;
    let rest = rest.trim_start_matches(|c: char| c.is_ascii_digit());
    let rest = rest.strip_prefix("-h")?;
    let after = rest.trim_start_matches(|c: char| c.is_ascii_digit());
    // At least one digit must follow `-h`.
    (after.len() < rest.len()).then_some(after)
}

#[derive(Clone, Debug)]
pub struct Artwork {
    pub content_type: String,
    pub bytes: Bytes,
}

#[derive(Debug, thiserror::Error)]
pub enum ArtworkError {
    #[error("url is not an allowed image host")]
    NotAllowed,
    #[error("artwork fetch failed: {0}")]
    Upstream(String),
    #[error("artwork is not an image")]
    NotImage,
    #[error("artwork is larger than 5 MB")]
    TooLarge,
    #[error("artwork could not be decoded or resized")]
    InvalidImage,
}

pub struct ArtworkProxy {
    allowed_hosts: Vec<String>,
    client: reqwest::Client,
    cache: Mutex<VecDeque<(String, Artwork)>>,
}

impl ArtworkProxy {
    pub fn new(allowed_hosts: Vec<String>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            // A redirect could leave the allow-list.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("build http client");
        Self {
            allowed_hosts,
            client,
            cache: Mutex::new(VecDeque::new()),
        }
    }

    /// The hosts YouTube Music serves covers from.
    pub fn youtube() -> Self {
        Self::new(HOSTS.iter().map(|h| (*h).to_owned()).collect())
    }

    /// Whether [`fetch`](Self::fetch) would accept `url` (an `http(s)` URL on an allowed host).
    pub fn allows(&self, url: &str) -> bool {
        self.parse_allowed(url).is_ok()
    }

    fn parse_allowed(&self, url: &str) -> Result<Url, ArtworkError> {
        let parsed = Url::parse(url).map_err(|_| ArtworkError::NotAllowed)?;
        let allowed = matches!(parsed.scheme(), "http" | "https")
            && parsed
                .host_str()
                .is_some_and(|h| self.allowed_hosts.iter().any(|a| a == h));
        if allowed {
            Ok(parsed)
        } else {
            Err(ArtworkError::NotAllowed)
        }
    }

    pub async fn fetch(&self, url: &str) -> Result<Artwork, ArtworkError> {
        let parsed = self.parse_allowed(url)?;
        if let Some(hit) = self.cached(url) {
            return Ok(hit);
        }

        let mut res = self
            .client
            .get(parsed)
            .send()
            .await
            .map_err(|e| ArtworkError::Upstream(e.to_string()))?;
        if !res.status().is_success() {
            return Err(ArtworkError::Upstream(format!("status {}", res.status())));
        }
        let content_type = res
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
            .filter(|t| t.starts_with("image/"))
            .ok_or(ArtworkError::NotImage)?;
        if res.content_length().is_some_and(|n| n > MAX_BYTES as u64) {
            return Err(ArtworkError::TooLarge);
        }
        let mut body = Vec::new();
        while let Some(chunk) = res
            .chunk()
            .await
            .map_err(|e| ArtworkError::Upstream(e.to_string()))?
        {
            if body.len() + chunk.len() > MAX_BYTES {
                return Err(ArtworkError::TooLarge);
            }
            body.extend_from_slice(&chunk);
        }

        let art = Artwork {
            content_type,
            bytes: Bytes::from(body),
        };
        self.remember(url, &art);
        Ok(art)
    }

    /// Resize to a square and encode a baseline JPEG for small clients.
    pub async fn fetch_resized(&self, url: &str, size: u32) -> Result<Artwork, ArtworkError> {
        self.parse_allowed(url)?;
        let key = format!("resize:{size}:{url}");
        if let Some(hit) = self.cached(&key) {
            return Ok(hit);
        }
        let original = self.fetch(url).await?;
        let art = tokio::task::spawn_blocking(move || {
            let mut reader = image::ImageReader::new(std::io::Cursor::new(original.bytes))
                .with_guessed_format()
                .map_err(|_| ArtworkError::InvalidImage)?;
            let mut limits = image::Limits::default();
            limits.max_image_width = Some(8192);
            limits.max_image_height = Some(8192);
            limits.max_alloc = Some(128 * 1024 * 1024);
            reader.limits(limits);
            let image = reader
                .decode()
                .map_err(|_| ArtworkError::InvalidImage)?
                .resize_to_fill(size, size, image::imageops::FilterType::Lanczos3)
                .to_rgb8();
            let mut output = Vec::new();
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut output, 85)
                .encode_image(&image)
                .map_err(|_| ArtworkError::InvalidImage)?;
            Ok::<_, ArtworkError>(Artwork {
                content_type: "image/jpeg".into(),
                bytes: output.into(),
            })
        })
        .await
        .map_err(|_| ArtworkError::InvalidImage)??;
        self.remember(&key, &art);
        Ok(art)
    }

    fn cached(&self, url: &str) -> Option<Artwork> {
        let cache = self.cache.lock().unwrap();
        cache.iter().find(|(u, _)| u == url).map(|(_, a)| a.clone())
    }

    fn remember(&self, url: &str, art: &Artwork) {
        let mut cache = self.cache.lock().unwrap();
        if cache.len() >= CACHE_SIZE {
            cache.pop_front();
        }
        cache.push_back((url.to_owned(), art.clone()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::http::header;
    use axum::routing::get;

    #[test]
    fn upscale_rewrites_lh3_size() {
        assert_eq!(
            upscale("https://lh3.googleusercontent.com/abc=w544-h544-l90-rj"),
            "https://lh3.googleusercontent.com/abc=w1200-h1200-l90-rj"
        );
        assert_eq!(
            upscale("https://lh3.googleusercontent.com/abc=w60-h60"),
            "https://lh3.googleusercontent.com/abc=w1200-h1200"
        );
    }

    #[test]
    fn sized_rewrites_lh3() {
        assert_eq!(
            sized(
                "https://lh3.googleusercontent.com/abc=w544-h544-l90-rj",
                120
            ),
            "https://lh3.googleusercontent.com/abc=w120-h120-l90-rj"
        );
    }

    #[test]
    fn sized_leaves_other_hosts() {
        assert_eq!(
            sized("https://i.ytimg.com/vi/x/hq.jpg", 120),
            "https://i.ytimg.com/vi/x/hq.jpg"
        );
    }

    #[test]
    fn upscale_rewrites_yt3_size() {
        assert_eq!(
            upscale("https://yt3.googleusercontent.com/abc=w544-h544-l90-rj"),
            "https://yt3.googleusercontent.com/abc=w1200-h1200-l90-rj"
        );
    }

    #[test]
    fn proxy_allows_yt3_googleusercontent() {
        let proxy = ArtworkProxy::youtube();
        assert!(
            proxy
                .allowed_hosts
                .iter()
                .any(|h| h == "yt3.googleusercontent.com")
        );
    }

    #[test]
    fn upscale_leaves_other_urls() {
        assert_eq!(
            upscale("https://i.ytimg.com/vi/x/sddefault.jpg"),
            "https://i.ytimg.com/vi/x/sddefault.jpg"
        );
        assert_eq!(
            upscale("https://lh3.googleusercontent.com/abc"),
            "https://lh3.googleusercontent.com/abc"
        );
        assert_eq!(
            upscale("https://i.ytimg.com/x=w60-h60"),
            "https://i.ytimg.com/x=w60-h60"
        );
    }

    #[tokio::test]
    async fn proxy_rejects_other_hosts() {
        let proxy = ArtworkProxy::youtube();
        assert!(matches!(
            proxy.fetch("https://evil.example/x.png").await,
            Err(ArtworkError::NotAllowed)
        ));
        assert!(matches!(
            proxy.fetch("not a url").await,
            Err(ArtworkError::NotAllowed)
        ));
        assert!(matches!(
            proxy.fetch("ftp://i.ytimg.com/x.png").await,
            Err(ArtworkError::NotAllowed)
        ));
    }

    #[tokio::test]
    async fn proxy_passes_images_and_rejects_non_images() {
        let app = Router::new()
            .route(
                "/a.png",
                get(|| async { ([(header::CONTENT_TYPE, "image/png")], vec![1u8, 2, 3]) }),
            )
            .route(
                "/t.txt",
                get(|| async { ([(header::CONTENT_TYPE, "text/plain")], "hi") }),
            )
            .route(
                "/big.png",
                get(|| async {
                    (
                        [(header::CONTENT_TYPE, "image/png")],
                        vec![0u8; MAX_BYTES + 1],
                    )
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let proxy = ArtworkProxy::new(vec!["127.0.0.1".into()]);
        let art = proxy.fetch(&format!("http://{addr}/a.png")).await.unwrap();
        assert_eq!(art.content_type, "image/png");
        assert_eq!(art.bytes.as_ref(), [1, 2, 3]);
        // Second call is served from the cache.
        assert_eq!(proxy.cache.lock().unwrap().len(), 1);
        assert!(proxy.fetch(&format!("http://{addr}/a.png")).await.is_ok());
        assert_eq!(proxy.cache.lock().unwrap().len(), 1);

        assert!(matches!(
            proxy.fetch(&format!("http://{addr}/t.txt")).await,
            Err(ArtworkError::NotImage)
        ));
        assert!(matches!(
            proxy.fetch(&format!("http://{addr}/big.png")).await,
            Err(ArtworkError::TooLarge)
        ));
        assert!(matches!(
            proxy.fetch(&format!("http://{addr}/missing")).await,
            Err(ArtworkError::Upstream(_))
        ));
    }
}
