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

export function get(path, signal) {
  return send('GET', path, undefined, signal);
}

export async function send(method, path, body, signal) {
  const opts = { method };
  if (signal) opts.signal = signal;
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

/** Autocomplete rows for a half-typed search: `[{ text, runs: [{ text, bold }] }]`. Pass a signal to cancel. */
export const searchSuggestions = (q, signal) => get('/api/search/suggestions?q=' + enc(q), signal);

// ---------- queue ----------
// The server takes 1-500 songs per queue call; longer lists are cut here, in one place.
const MAX_QUEUE_SONGS = 500;

/**
 * Plays the list from `startIndex` (the songs before it are dropped). A list longer than 500 plays its
 * 500 songs from `startIndex`; with `shuffle`, a random 500 of the whole list.
 */
export function queuePlay(songs, { startIndex = 0, shuffle = false } = {}) {
  let list;
  if (shuffle) {
    list = songs.slice();
    if (list.length > MAX_QUEUE_SONGS) {
      for (let i = list.length - 1; i > 0; i--) {
        const j = Math.floor(Math.random() * (i + 1));
        [list[i], list[j]] = [list[j], list[i]];
      }
      list.length = MAX_QUEUE_SONGS;
    }
  } else list = songs.slice(startIndex, startIndex + MAX_QUEUE_SONGS);
  return send('POST', '/api/queue/play', { songs: list, startIndex: 0, shuffle });
}
/** `at`: "next" (after the current song, Play-next rules) or "end". At most the first 500 songs are sent. */
export const queueAdd = (songs, at = 'end') =>
  send('POST', '/api/queue/add', { songs: songs.slice(0, MAX_QUEUE_SONGS), at });
export const playNext = (song) => send('POST', '/api/queue/next', song);
export const queueRemove = (entryId) => send('DELETE', `/api/queue/${enc(entryId)}`);

// ---------- autoplay ----------
export const setAutoplay = (enabled) => send('PUT', '/api/autoplay', { enabled });
/** Plays one of `state.autoplay.songs` now; a 409 means it is no longer suggested. */
export const playSuggestion = (videoId) => send('POST', '/api/autoplay/play', { videoId });

// ---------- player ----------
// skip/previous name the entry the user saw, so a tap racing the song's end is a 409, not a double skip.
const onEntry = (cmd, entryId) =>
  send('POST', `/api/player/${cmd}` + (entryId != null ? `?entryId=${enc(entryId)}` : ''));
export const player = {
  pause: () => send('POST', '/api/player/pause'),
  resume: () => send('POST', '/api/player/resume'),
  skip: (entryId) => onEntry('skip', entryId),
  previous: (entryId) => onEntry('previous', entryId),
  /** Jumps to `positionMs` in the current song (reopens it: about as slow as starting a song). */
  seek: (positionMs, entryId) =>
    send(
      'POST',
      `/api/player/seek?positionMs=${Math.max(0, Math.round(positionMs))}` + (entryId != null ? `&entryId=${enc(entryId)}` : ''),
    ),
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

// ---------- speakers (Nidoo satellites) ----------
/** `change`: {volume?: 0-100, muted?: bool}. Both calls 404 when the engine has no speaker control. */
export const speakers = {
  list: () => get('/api/speakers'),
  set: (id, change) => send('PUT', `/api/speakers/${enc(id)}`, change),
};

// ---------- YouTube Music catalog ----------
/** `kind`: "songs" (default), "albums" or "playlists". */
export const search = (q, kind = 'songs') => get(`/api/search?q=${enc(q)}&kind=${enc(kind)}`);
export const album = (id) => get(`/api/albums/${enc(id)}`);
/** Home mixes: `[Collection]` (0-6), and one mix page. */
export const mixes = () => get('/api/mixes');
export const mix = (id) => get('/api/mixes/' + enc(id));
export const ytplaylist = (id) => get(`/api/ytplaylists/${enc(id)}`);

export function lyrics(song) {
  const q = new URLSearchParams({ videoId: song.videoId, title: song.title, artists: song.artists });
  if (song.album) q.set('album', song.album);
  if (song.durationSecs) q.set('durationSecs', String(song.durationSecs));
  return get(`/api/lyrics?${q}`);
}

// ---------- artwork ----------
/** Same-origin URL for a cover, for reading its pixels (canvas). Display covers from `artworkUrl` directly. */

const RESIZABLE = /^https?:\/\/(lh3|yt3)\.googleusercontent\.com\//;

/** `url` asking the image host for about `px`×`px` (googleusercontent `=wN-hN` suffix); other URLs unchanged. */
export function artworkAt(url, px) {
  if (!url || !RESIZABLE.test(url)) return url;
  return url.replace(/=w\d+-h\d+/, `=w${px}-h${px}`);
}
