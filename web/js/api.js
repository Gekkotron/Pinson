// HTTP API client (docs/api.md). Every helper resolves to the parsed JSON body, or `null`
// for a 204, and rejects with an ApiError carrying the server's `error` text.

import { store } from './store.js';

export class ApiError extends Error {
  constructor(status, message) {
    super(message);
    this.name = 'ApiError';
    this.status = status;
  }
}

export function get(path) {
  return send('GET', path);
}

export async function send(method, path, body) {
  const opts = { method };
  if (body !== undefined) {
    opts.headers = { 'Content-Type': 'application/json' };
    opts.body = JSON.stringify(body);
  }
  let res;
  try {
    res = await fetch(path, opts);
  } catch (_) {
    throw new ApiError(0, 'Server unreachable');
  }
  if (res.status === 204) return null;
  const data = await res.json().catch(() => null);
  if (!res.ok) throw new ApiError(res.status, (data && data.error) || `Request failed (${res.status})`);
  return data;
}

const enc = encodeURIComponent;

// ---------- queue ----------
export const queuePlay = (songs, { startIndex = 0, shuffle = false } = {}) =>
  send('POST', '/api/queue/play', { songs, startIndex, shuffle });
/** `at`: "next" (after the current song, Play-next rules) or "end". */
export const queueAdd = (songs, at = 'end') => send('POST', '/api/queue/add', { songs, at });
export const playNext = (song) => send('POST', '/api/queue/next', song);
export const queueRemove = (entryId) => send('DELETE', `/api/queue/${enc(entryId)}`);

// ---------- player ----------
// skip/previous name the entry the user saw, so a tap racing the song's end is a 409, not a double skip.
const onEntry = (cmd, entryId) =>
  send('POST', `/api/player/${cmd}` + (entryId != null ? `?entryId=${enc(entryId)}` : ''));
export const player = {
  pause: () => send('POST', '/api/player/pause'),
  resume: () => send('POST', '/api/player/resume'),
  skip: (entryId) => onEntry('skip', entryId),
  previous: (entryId) => onEntry('previous', entryId),
};

// ---------- favorites ----------
/** Adds or removes `song`; the local list changes at once and is rolled back on failure. */
export async function favoriteToggle(song) {
  const add = !store.isFavorite(song.videoId);
  const apply = (on) =>
    store.setFavorites(
      on ? [song, ...store.favorites] : store.favorites.filter((s) => s.videoId !== song.videoId),
    );
  apply(add);
  try {
    if (add) await send('PUT', `/api/favorites/${enc(song.videoId)}`, song);
    else await send('DELETE', `/api/favorites/${enc(song.videoId)}`);
  } catch (err) {
    if (store.isFavorite(song.videoId) === add) apply(!add);
    throw err;
  }
  return add;
}
export const favoritesShuffle = () => send('POST', '/api/favorites/shuffle');

// ---------- my playlists ----------
export const playlists = {
  list: () => get('/api/playlists'),
  create: (name, songs) => send('POST', '/api/playlists', songs ? { name, songs } : { name }),
  get: (id) => get(`/api/playlists/${enc(id)}`),
  rename: (id, name) => send('PATCH', `/api/playlists/${enc(id)}`, { name }),
  remove: (id) => send('DELETE', `/api/playlists/${enc(id)}`),
  addSongs: (id, songs) => send('POST', `/api/playlists/${enc(id)}/songs`, { songs }),
  removeSong: (id, videoId) => send('DELETE', `/api/playlists/${enc(id)}/songs/${enc(videoId)}`),
  reorder: (id, videoIds) => send('PUT', `/api/playlists/${enc(id)}/order`, { videoIds }),
};

// ---------- YouTube Music catalog ----------
/** `kind`: "songs" (default), "albums" or "playlists". */
export const search = (q, kind = 'songs') => get(`/api/search?q=${enc(q)}&kind=${enc(kind)}`);
export const album = (id) => get(`/api/albums/${enc(id)}`);
export const ytplaylist = (id) => get(`/api/ytplaylists/${enc(id)}`);

export function lyrics(song) {
  const q = new URLSearchParams({ videoId: song.videoId, title: song.title, artists: song.artists });
  if (song.album) q.set('album', song.album);
  if (song.durationSecs) q.set('durationSecs', String(song.durationSecs));
  return get(`/api/lyrics?${q}`);
}

// ---------- artwork ----------
/** Same-origin URL for a cover, for reading its pixels (canvas). Display covers from `artworkUrl` directly. */
export const artworkProxyUrl = (url) => `/api/artwork?url=${enc(url)}`;

const RESIZABLE = /^https?:\/\/(lh3|yt3)\.googleusercontent\.com\//;

/** `url` asking the image host for about `px`×`px` (googleusercontent `=wN-hN` suffix); other URLs unchanged. */
export function artworkAt(url, px) {
  if (!url || !RESIZABLE.test(url)) return url;
  return url.replace(/=w\d+-h\d+/, `=w${px}-h${px}`);
}
