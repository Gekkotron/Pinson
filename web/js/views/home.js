// Home (`#/home`): tiles for Favorites, my playlists and "+ New playlist".
// Library (`#/library`, the phone's third tab): the same collection as a list.

import { mixes as mixesApi, playlists as playlistsApi, queuePlay } from '../api.js';
import { icon } from '../icons.js';
import { pinsonMark } from '../logo.js';
import { openOutputPicker } from '../output.js';
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
  let alive = true;
  const paintMixes = (list) => {
    mixRow.replaceChildren(...list.map(mixTile));
    mixSection.hidden = !list.length;
  };
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
          iconButton('output', 'Output', openOutputPicker, { cls: 'home-output', size: 22 }),
        ),
        h('span', { class: 'home-wide', text: 'Home' }),
      ),
      mixSection,
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
  mixesApi().then(
    (list) => {
      lastMixes = list;
      saveMixes(list);
      if (alive) paintMixes(list);
    },
    () => {}, // keep whatever is shown; mixes are optional
  );
  const offs = [store.on('favorites', paint), store.on('playlists', paint)];
  return () => {
    alive = false;
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
