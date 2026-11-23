// Home (`#/home`): tiles for Favorites, my playlists and "+ New playlist".
// Library (`#/library`, the phone's third tab): the same collection as a list.

import { mixes as mixesApi, newReleases as newReleasesApi, moments as momentsApi, playlists as playlistsApi, queuePlay, ytplaylist } from '../api.js';
import { releaseSubtitle } from '../follows.js';
import { icon } from '../icons.js';
import { pinsonMark } from '../logo.js';
import { outputButton } from '../output.js';
import { store } from '../store.js';
import { openThemeMenu } from '../theme.js';
import { cover, h, iconButton, plural, toast } from '../ui.js';

/** The Favorites artwork: a red gradient with a big heart. */
export function favoritesArt(size) {
  const art = h('div', { class: 'cover cover-favorites' }, icon('heartFill', { size: size ? Math.round(size * 0.4) : 64 }));
  if (size) {
    art.style.width = size + 'px';
    art.style.height = size + 'px';
  }
  return art;
}

/** The Recently played artwork: a blue gradient with a clock. */
function historyArt(size) {
  const art = h('div', { class: 'cover cover-history' }, icon('history', { size: Math.round(size * 0.5) }));
  art.style.width = size + 'px';
  art.style.height = size + 'px';
  return art;
}

/** The Your stats artwork: the Recently played gradient with bars. */
function statsArt(size) {
  const art = h('div', { class: 'cover cover-history' }, icon('chart', { size: Math.round(size * 0.5) }));
  art.style.width = size + 'px';
  art.style.height = size + 'px';
  return art;
}

/** The Artists you follow artwork: the Recently played gradient with a person. */
function followingArt(size) {
  const art = h('div', { class: 'cover cover-history' }, icon('person', { size: Math.round(size * 0.5) }));
  art.style.width = size + 'px';
  art.style.height = size + 'px';
  return art;
}

/** Plays all favorites from the top. */
export function playFavorites() {
  if (!store.favorites.length) return toast('No favorites yet');
  queuePlay(store.favorites).catch((err) => toast(err.message));
}

/** Plays one of my playlists from the top. */
export async function playPlaylist(id) {
  try {
    const pl = await playlistsApi.get(id);
    if (!pl.songs.length) return toast(`${pl.name} is empty`);
    await queuePlay(pl.songs);
  } catch (err) {
    toast(err.message);
  }
}

const playlistSub = (p) => `Playlist · ${plural(p.songCount, 'song')}`;

function tile({ href, art, title, sub, onPlay, label }) {
  return h(
    'a',
    { class: 'tile', href },
    h(
      'div',
      { class: 'tile-art' },
      art,
      onPlay && iconButton('play', label, (e) => {
        e.preventDefault();
        onPlay();
      }, { cls: 'tile-play', size: 26 }),
    ),
    h('div', { class: 'tile-title', text: title }),
    h('div', { class: 'tile-sub', text: sub }),
  );
}

// The last mixes fetched, painted at once on the next visit while a fresh answer loads.
const MIXES_KEY = 'pinson.lastMixes';
function loadMixes() {
  try {
    const v = JSON.parse(localStorage.getItem(MIXES_KEY));
    return Array.isArray(v) ? v : null;
  } catch {
    return null;
  }
}
function saveMixes(list) {
  try {
    localStorage.setItem(MIXES_KEY, JSON.stringify(list));
  } catch {
    // storage may be full or blocked; the row just won't paint early
  }
}
let lastMixes = null;

// The last "Right now" answer, cached the same way.
const MOMENTS_KEY = 'pinson.lastMoments';
function loadMoments() {
  try {
    const v = JSON.parse(localStorage.getItem(MOMENTS_KEY));
    return v && Array.isArray(v.playlists) && v.slot && v.day ? v : null;
  } catch {
    return null;
  }
}
function saveMoments(answer) {
  try {
    localStorage.setItem(MOMENTS_KEY, JSON.stringify(answer));
  } catch {
    // storage may be full or blocked; the row just won't paint early
  }
}
let lastMoments = null;

/** The local date as YYYY-MM-DD (not toISOString, which is UTC). */
function localDay(d) {
  const pad = (n) => String(n).padStart(2, '0');
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
}

/** The part of the day for `hour`, as the server names it (6-10, 11-16, 17-21, else night). */
function slotForHour(hour) {
  if (hour >= 6 && hour <= 10) return 'morning';
  if (hour >= 11 && hour <= 16) return 'afternoon';
  if (hour >= 17 && hour <= 21) return 'evening';
  return 'night';
}

// Tiles whose playlist is loading for ▶, so a second tap does not queue it twice.
const loadingPlaylists = new Set();

async function playYtPlaylist(p) {
  if (loadingPlaylists.has(p.id)) return;
  loadingPlaylists.add(p.id);
  try {
    const pl = await ytplaylist(p.id);
    if (!pl.songs.length) return toast(`${p.title} is empty`);
    await queuePlay(pl.songs);
  } catch (err) {
    toast(err.message);
  } finally {
    loadingPlaylists.delete(p.id);
  }
}

function momentTile(p) {
  return tile({
    href: '#/ytplaylist/' + encodeURIComponent(p.id),
    art: cover(p.artworkUrl),
    title: p.title,
    sub: p.subtitle || '',
    onPlay: () => playYtPlaylist(p),
    label: 'Play ' + p.title,
  });
}

function mixTile(m) {
  return tile({
    href: '#/mix/' + encodeURIComponent(m.id),
    art: h('div', { class: 'mix-art' }, cover(m.artworkUrl), h('span', { class: 'mix-badge', text: 'Mix' })),
    title: m.title,
    sub: m.subtitle || '',
    onPlay: () => queuePlay(m.songs).catch((e) => toast(e.message)),
    label: 'Play ' + m.title,
  });
}

function releaseTile(r) {
  return tile({
    href: '#/album/' + encodeURIComponent(r.id),
    art: cover(r.artworkUrl),
    title: r.title,
    sub: releaseSubtitle(r),
  });
}

function newPlaylistTile() {
  return h(
    'button',
    { type: 'button', class: 'tile tile-new', dataset: { action: 'new-playlist' } },
    h('div', { class: 'tile-art' }, h('div', { class: 'cover cover-new' }, icon('plus', { size: 44 }))),
    h('div', { class: 'tile-title', text: 'New playlist' }),
    h('div', { class: 'tile-sub', text: 'Start a collection' }),
  );
}

export function renderHome(el) {
  const grid = h('div', { class: 'tile-grid' });
  const mixRow = h('div', { class: 'mix-row' });
  const mixSection = h(
    'section',
    { class: 'mix-section', hidden: true },
    h('h2', { class: 'mix-heading', text: 'Mixes for you' }),
    h('div', { class: 'mix-sub', text: "From what you've been playing" }),
    mixRow,
  );
  const releaseRow = h('div', { class: 'mix-row' });
  const releaseSection = h(
    'section',
    { class: 'mix-section', hidden: true },
    h('h2', { class: 'mix-heading', text: 'New releases' }),
    h('div', { class: 'mix-sub', text: 'From artists you follow' }),
    releaseRow,
  );
  const momentRow = h('div', { class: 'mix-row' });
  const momentSub = h('div', { class: 'mix-sub' });
  const momentSection = h(
    'section',
    { class: 'mix-section', hidden: true },
    h('h2', { class: 'mix-heading', text: 'Right now' }),
    momentSub,
    momentRow,
  );
  let alive = true;
  const paintMoments = (answer) => {
    const slot = answer.slot ? answer.slot[0].toUpperCase() + answer.slot.slice(1) : '';
    momentSub.textContent = `${slot} picks · new every day`;
    momentRow.replaceChildren(...answer.playlists.map(momentTile));
    momentSection.hidden = !answer.playlists.length;
  };
  const paintMixes = (list) => {
    mixRow.replaceChildren(...list.map(mixTile));
    mixSection.hidden = !list.length;
  };
  const outBtn = outputButton('home-output', 20);
  el.append(
    h(
      'div',
      { class: 'page page-home' },
      h(
        'h1',
        { class: 'page-title home-title' },
        h(
          'span',
          { class: 'home-brand' },
          pinsonMark(34),
          h('span', { text: 'Pinson' }),
          iconButton('palette', 'Theme', (e) => openThemeMenu(e.currentTarget), { cls: 'home-theme', size: 22 }),
          outBtn.el,
        ),
        h('span', { class: 'home-wide', text: 'Home' }),
      ),
      momentSection,
      mixSection,
      releaseSection,
      h('div', { class: 'section-label', text: 'Your playlists' }),
      grid,
    ),
  );
  const paint = () => {
    const favs = store.favorites;
    grid.replaceChildren(
      tile({
        href: '#/favorites',
        art: favoritesArt(),
        title: 'Favorites',
        sub: plural(favs.length, 'song'),
        onPlay: favs.length ? playFavorites : null,
        label: 'Play Favorites',
      }),
      ...store.playlists.map((p) =>
        tile({
          href: '#/playlist/' + encodeURIComponent(p.id),
          art: cover(null, { mosaic: p.artworkUrls }),
          title: p.name,
          sub: playlistSub(p),
          onPlay: p.songCount ? () => playPlaylist(p.id) : null,
          label: `Play ${p.name}`,
        }),
      ),
      newPlaylistTile(),
    );
  };
  paint();
  if (!lastMixes) lastMixes = loadMixes();
  if (lastMixes) paintMixes(lastMixes);
  if (!lastMoments) lastMoments = loadMoments();
  const now = new Date();
  const day = localDay(now);
  const slot = slotForHour(now.getHours());
  // Paint the cached row only while it is still today's and this part of the day's.
  const current = (a) => a && a.day === day && a.slot === slot;
  if (current(lastMoments)) paintMoments(lastMoments);
  momentsApi(now.getHours(), day).then(
    (answer) => {
      const fresh = { slot: answer.slot, day, playlists: answer.playlists };
      // An empty answer (YouTube Music was unreachable) must not wipe a good row.
      if (!fresh.playlists.length && current(lastMoments) && lastMoments.playlists.length) return;
      lastMoments = fresh;
      saveMoments(fresh);
      if (alive) paintMoments(fresh);
    },
    () => {}, // keep whatever is shown; the row is optional
  );
  mixesApi().then(
    (list) => {
      lastMixes = list;
      saveMixes(list);
      if (alive) paintMixes(list);
    },
    () => {}, // keep whatever is shown; mixes are optional
  );
  const releasesCtl = new AbortController();
  newReleasesApi(releasesCtl.signal).then(
    (answer) => {
      const list = (answer && answer.releases) || [];
      if (!alive || !list.length) return;
      releaseRow.replaceChildren(...list.map(releaseTile));
      releaseSection.hidden = false;
    },
    () => {}, // the row stays hidden; it is optional
  );
  const offs = [store.on('favorites', paint), store.on('playlists', paint), outBtn.off];
  return () => {
    alive = false;
    releasesCtl.abort();
    offs.forEach((off) => off());
  };
}

function libraryRow({ href, art, title, sub }) {
  return h(
    'a',
    { class: 'lib-row', href },
    art,
    h('div', { class: 'lib-text' }, h('div', { class: 'lib-title', text: title }), h('div', { class: 'lib-sub', text: sub })),
  );
}

export function renderLibrary(el) {
  const list = h('div', { class: 'lib-list' });
  el.append(h('div', { class: 'page page-library' }, h('h1', { class: 'page-title', text: 'Library' }), list));
  const paint = () => {
    list.replaceChildren(
      libraryRow({
        href: '#/favorites',
        art: favoritesArt(56),
        title: 'Favorites',
        sub: `Auto playlist · ${plural(store.favorites.length, 'song')}`,
      }),
      libraryRow({
        href: '#/history',
        art: historyArt(56),
        title: 'Recently played',
        sub: "Everything you've played",
      }),
      libraryRow({
        href: '#/stats',
        art: statsArt(56),
        title: 'Your stats',
        sub: 'Top songs and artists',
      }),
      libraryRow({
        href: '#/following',
        art: followingArt(56),
        title: 'Artists you follow',
        sub: 'Their new releases show on Home',
      }),
      ...store.playlists.map((p) =>
        libraryRow({
          href: '#/playlist/' + encodeURIComponent(p.id),
          art: cover(null, { mosaic: p.artworkUrls, size: 56 }),
          title: p.name,
          sub: playlistSub(p),
        }),
      ),
      h(
        'button',
        { type: 'button', class: 'lib-row lib-new', dataset: { action: 'new-playlist' } },
        h('div', { class: 'cover cover-new', style: { width: '56px', height: '56px' } }, icon('plus', { size: 28 })),
        h('div', { class: 'lib-text' }, h('div', { class: 'lib-title', text: 'New playlist' })),
      ),
    );
  };
  paint();
  const offs = [store.on('favorites', paint), store.on('playlists', paint)];
  return () => offs.forEach((off) => off());
}
