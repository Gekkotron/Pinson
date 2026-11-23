// Pure helpers for the "Follow artists" feature: card subtitles and badge text.

/** "Single" or "Album" for a release `kind` from the API. */
export const kindLabel = (kind) => (kind === 'single' ? 'Single' : 'Album');

/** Subtitle of a New releases card: "<artist> · Single · 2026"; missing parts are left out. */
export function releaseSubtitle(r) {
  return [r.artistName, kindLabel(r.kind), r.year].filter(Boolean).join(' · ');
}

/** "3 new" for a followed artist with new releases, '' otherwise. */
export function newBadge(n) {
  return Number.isInteger(n) && n > 0 ? `${n} new` : '';
}
