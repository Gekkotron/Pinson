//! The phone web page, embedded in the binary from `web/`.

use axum::Router;
use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "web/"]
struct Assets;

/// Where the `/lyrics` shortcut lands: Now playing on the Lyrics tab, in big-screen (TV) mode.
const TV_LYRICS: &str = "/#/now?tab=lyrics&tv=1";

/// Serves `web/` at `/` (`/` is `index.html`), plus the `/lyrics` shortcut for a TV browser.
/// Merge it after the API router.
pub fn web_routes() -> Router {
    Router::new()
        .route("/lyrics", get(tv_lyrics))
        .route("/lyrics/", get(tv_lyrics))
        .fallback(asset)
}

/// `/lyrics?keys=1` also turns on the box that shows what each TV remote button sends.
async fn tv_lyrics(uri: Uri) -> Redirect {
    let keys = uri
        .query()
        .is_some_and(|q| q.split('&').any(|p| p == "keys=1"));
    if keys {
        Redirect::temporary(&format!("{TV_LYRICS}&keys=1"))
    } else {
        Redirect::temporary(TV_LYRICS)
    }
}

async fn asset(uri: Uri) -> Response {
    let path = match uri.path().trim_start_matches('/') {
        "" => "index.html",
        p => p,
    };
    match Assets::get(path) {
        Some(file) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            (
                [
                    (header::CONTENT_TYPE, mime.as_ref().to_owned()),
                    (header::CACHE_CONTROL, "no-cache".to_owned()),
                ],
                file.data,
            )
                .into_response()
        }
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}
