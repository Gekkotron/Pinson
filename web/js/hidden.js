// Pure helpers for "Don't play this again": credit splitting that mirrors the server's
// `stats::split_artists`, so the artist we offer to hide is the one the server matches.

const SEPARATORS = [', ', ' & ', ' feat. ', ' ft. '];

/** "A, B & C feat. D" → ["A", "B", "C", "D"]. Splits only on the server's separators; trims; drops empties. */
export function splitArtists(credit) {
  let parts = [String(credit == null ? '' : credit)];
  for (const sep of SEPARATORS) parts = parts.flatMap((p) => p.split(sep));
  return parts.map((p) => p.trim()).filter(Boolean);
}

/** The first credited artist, or '' when there is none. */
export function firstArtist(credit) {
  return splitArtists(credit)[0] || '';
}
