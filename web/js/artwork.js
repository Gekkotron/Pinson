// Cover URLs sized for where they are drawn.

const RESIZABLE = /^https?:\/\/(lh3|yt3)\.googleusercontent\.com\//;
// A music video's thumbnail. YouTube Music sends `hqdefault`/`sddefault` (4:3 with black bars
// baked in above and below the 16:9 picture); the same video has bar-free 16:9 frames.
const VIDEO_THUMB = /^https?:\/\/i\.ytimg\.com\/vi(?:_webp)?\/([\w-]{11})\//;
const video = (id, name) => `https://i.ytimg.com/vi/${id}/${name}.jpg`;

/**
 * `url` asking the image host for about `px`×`px`: googleusercontent `=wN-hN` suffix; a video
 * thumbnail becomes the bar-free 320×180 frame (always there) or, above 320 px, the 1280×720 one
 * (missing for some videos: see `artworkFallback`). Other URLs unchanged.
 */
export function artworkAt(url, px) {
  if (!url) return url;
  if (RESIZABLE.test(url)) return url.replace(/=w\d+-h\d+/, `=w${px}-h${px}`);
  const m = VIDEO_THUMB.exec(url);
  if (m) return video(m[1], px > 320 ? 'maxresdefault' : 'mqdefault');
  return url;
}

/** What to show when `src` (from `artworkAt`) is missing: the 320×180 frame for a 1280×720 one, else null. */
export function artworkFallback(src) {
  const m = src && VIDEO_THUMB.exec(src);
  return m && src.endsWith('/maxresdefault.jpg') ? video(m[1], 'mqdefault') : null;
}

/** YouTube answers a missing frame with a 120×90 grey placeholder (status 404, but it may still load). */
export const isMissingFrame = (img) => img.naturalWidth > 0 && img.naturalWidth <= 120 && artworkFallback(img.currentSrc || img.src) != null;

// Covers already on screen once, by the URL asked for → the URL that loaded (the 320×180 frame
// when the 1280×720 one was missing). A list rebuilt for a new state (Crossfade, a heart, a
// move...) makes new <img>s; for these they start shown instead of fading in from blank again.
const shown = new Map();

/** Remembers that `asked` (from `artworkAt`) was drawn, as `loaded` (the URL that actually loaded). */
export function rememberShown(asked, loaded) {
  if (asked && loaded) shown.set(asked, loaded);
}

/** The URL `asked` was last drawn from, or null if it was never shown. */
export const shownSrc = (asked) => shown.get(asked) ?? null;
