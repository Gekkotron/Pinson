// Pure helpers for the artist page and the "Go to artist" links.

export const TOP_SONGS = 10;

/** Hash of the artist page for a song's artist text; the page resolves the name to an id. */
export const artistNameHref = (name) => '#/artist-name/' + encodeURIComponent(name);

/** The songs shown in "Top songs": the first `TOP_SONGS`, or all of them once expanded. */
export const visibleSongs = (songs, expanded) => (expanded ? songs : songs.slice(0, TOP_SONGS));
