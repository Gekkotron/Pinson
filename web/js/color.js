// Dominant cover colour, computed by the server so every client shows the same tint.

const cache = new Map(); // artworkUrl -> Promise<{color, glow} | null>

/** `{color, glow}` (`#rrggbb` each) for a cover, or null when it cannot be coloured. Cached per URL. */
export function coverColor(url) {
  if (!url) return Promise.resolve(null);
  if (!cache.has(url)) {
    const p = fetch('/api/artwork/color?url=' + encodeURIComponent(url))
      .then((res) => (res.ok ? res.json() : Promise.reject(new Error(String(res.status)))))
      .then((c) => (c && c.color && c.glow ? c : null))
      .catch(() => {
        // A failed request is not remembered, so the next song change retries it.
        cache.delete(url);
        return null;
      });
    cache.set(url, p);
  }
  return cache.get(url);
}
