// App shell: sidebar + player bar (laptop), bottom tabs + mini player (phone), routes,
// reconnect banner, song-failed notices and the position ticker.

import { playlists } from './api.js';
import { startDevicePlayer } from './device-player.js';
import { deviceSoundOn, startDeviceSound, stopDeviceSound } from './device-sound.js';
import { icon } from './icons.js';
import { pinsonMark } from './logo.js';
import { openOutputPicker, watchOutput } from './output.js';
import { openSpeakers } from './speakers.js';
import './splash.js';
import { openThemeMenu } from './theme.js';
import { back, current, go, onChange, route, start } from './router.js';
import { connect, positionMs, store } from './store.js';
import {
  closeLayers,
  cover,
  fmtTime,
  h,
  heartButton,
  iconButton,
  playPauseButton,
  plural,
  promptName,
  seekable,
  skip,
  toast,
  togglePlay,
  transport,
} from './ui.js';
import { renderCollection } from './views/collection.js';
import { renderFavorites } from './views/favorites.js';
import { renderHistory } from './views/history.js';
import { renderHome, renderLibrary } from './views/home.js';
import { openNow, renderNow } from './views/now.js';
import { prefetchLyrics } from './views/lyrics.js';
import { renderPlaylist } from './views/playlist.js';
import { renderSearch } from './views/search.js';

const $ = (id) => document.getElementById(id);

// ---------- routes ----------
route('/home', renderHome);
route('/library', renderLibrary);
route('/favorites', renderFavorites);
route('/history', renderHistory);
route('/now', renderNow);

route('/search', renderSearch);
route('/album/:id', renderCollection);
route('/ytplaylist/:id', renderCollection);
route('/mix/:id', renderCollection);

route('/playlist/:id', renderPlaylist);

// ---------- sidebar (laptop) ----------
const sidePlaylists = h('div', { class: 'side-playlists' });

function navLink(iconName, label, href, key) {
  return h('a', { class: 'side-link', href, dataset: { nav: key } }, icon(iconName, { size: 24 }), h('span', { text: label }));
}

$('sidebar').append(
  h('a', { class: 'brand', href: '#/home', 'aria-label': 'Pinson home' }, pinsonMark(32), h('span', { text: 'Pinson' })),
  h(
    'nav',
    { class: 'side-nav', 'aria-label': 'Main' },
    navLink('home', 'Home', '#/home', 'home'),
    navLink('search', 'Search', '#/search', 'search'),
    navLink('heartFill', 'Favorites', '#/favorites', 'favorites'),
    navLink('history', 'Recently played', '#/history', 'history'),
  ),
  h('div', { class: 'side-sep' }),
  h('button', { type: 'button', class: 'side-new', dataset: { action: 'new-playlist' } }, icon('plus', { size: 20 }), h('span', { text: 'New playlist' })),
  sidePlaylists,
  h('button', { type: 'button', class: 'side-new side-theme', 'aria-label': 'Theme', onclick: (e) => openThemeMenu(e.currentTarget) }, icon('palette', { size: 20 }), h('span', { text: 'Theme' })),
  h('button', { type: 'button', class: 'side-new side-output', 'aria-label': 'Output', onclick: openOutputPicker }, icon('output', { size: 20 }), h('span', { text: 'Output' })),
);

function paintSidePlaylists() {
  sidePlaylists.replaceChildren(
    ...store.playlists.map((p) =>
      h(
        'a',
        { class: 'side-pl', href: '#/playlist/' + encodeURIComponent(p.id), dataset: { pl: p.id } },
        h('span', { class: 'side-pl-name', text: p.name }),
        h('span', { class: 'side-pl-sub', text: `Playlist · ${plural(p.songCount, 'song')}` }),
      ),
    ),
  );
  markActive(current());
}
store.on('playlists', paintSidePlaylists);

// ---------- bottom tabs (phone) ----------
function tabLink(iconName, label, href, key) {
  return h('a', { class: 'tab', href, dataset: { tab: key } }, icon(iconName, { size: 24 }), h('span', { text: label }));
}
$('tabbar').append(
  tabLink('home', 'Home', '#/home', 'home'),
  tabLink('search', 'Search', '#/search', 'search'),
  tabLink('library', 'Library', '#/library', 'library'),
);

// Which sidebar entry / tab a page belongs to.
const SECTION = {
  '/home': ['home', 'home'],
  '/search': ['search', 'search'],
  '/album/:id': ['search', 'search'],
  '/ytplaylist/:id': ['search', 'search'],
  '/mix/:id': ['home', 'home'],
  '/favorites': ['favorites', 'library'],
  '/history': ['history', 'library'],
  '/library': [null, 'library'],
  '/playlist/:id': [null, 'library'],
};

function markActive(page) {
  if (!page) return;
  const [nav, tab] = SECTION[page.pattern] || [];
  for (const a of document.querySelectorAll('[data-nav]')) toggleCurrent(a, a.dataset.nav === nav);
  for (const a of document.querySelectorAll('[data-tab]')) toggleCurrent(a, a.dataset.tab === tab);
  for (const a of document.querySelectorAll('[data-pl]')) {
    toggleCurrent(a, page.pattern === '/playlist/:id' && a.dataset.pl === page.params.id);
  }
}

function toggleCurrent(el, on) {
  el.classList.toggle('active', on);
  if (on) el.setAttribute('aria-current', 'page');
  else el.removeAttribute('aria-current');
}

// ---------- player bar (laptop) and mini player (phone) ----------
const onNow = () => (current() || {}).pattern === '/now';
const toggleNow = () => (onNow() ? back() : openNow());

const bar = {
  art: h('div', { class: 'player-art' }),
  title: h('div', { class: 'player-title' }),
  sub: h('div', { class: 'player-sub' }),
  heart: h('div', { class: 'player-heart' }),
};
$('player').append(
  seekable(h('div', { class: 'player-progress' }, h('i', { dataset: { posFill: '' } }))),
  h(
    'div',
    { class: 'player-left' },
    transport({ size: 24, mainSize: 34, mainCls: 'player-play' }),
    h('span', { class: 'player-time' }, h('span', { dataset: { posCur: '' } }, '0:00'), ' / ', h('span', { dataset: { posDur: '' } }, '0:00')),
  ),
  h(
    'button',
    { type: 'button', class: 'player-np', 'aria-label': 'Open now playing', onclick: toggleNow },
    bar.art,
    h('div', { class: 'player-text' }, bar.title, bar.sub),
  ),
  h(
    'div',
    { class: 'player-right' },
    bar.heart,
    iconButton('speaker', 'Speakers', openSpeakers, { cls: 'player-speakers' }),
    iconButton('queue', 'Up next', () => (onNow() ? back() : openNow('upnext')), { cls: 'player-queue' }),
  ),
);

const mini = {
  art: h('div', { class: 'mini-art' }),
  title: h('div', { class: 'mini-title' }),
  sub: h('div', { class: 'mini-sub' }),
};
$('mini').append(
  h('div', { class: 'mini-progress' }, h('i', { dataset: { posFill: '' } })),
  h('button', { type: 'button', class: 'mini-open', 'aria-label': 'Open now playing', onclick: () => openNow() }, mini.art, h('div', { class: 'mini-text' }, mini.title, mini.sub)),
  iconButton('speaker', 'Speakers', openSpeakers, { size: 28, cls: 'mini-speakers' }),
  playPauseButton({ size: 30, cls: 'mini-play' }),
  iconButton('next', 'Next', skip, { size: 28 }),
);

// The speaker buttons stay hidden unless the current output has speaker control (Nidoo).
const speakerButtons = document.querySelectorAll('.player-speakers, .mini-speakers');
const paintSpeakerButtons = () => {
  const on = !!(store.output && store.output.current && store.output.current.speakers);
  speakerButtons.forEach((b) => (b.hidden = !on));
};
paintSpeakerButtons();
store.on('output', paintSpeakerButtons);

// Sound on this device follows the output: it stops when the output has no browser player, and
// (re)starts, if the user turned it on, when one appears or its address changes.
let deviceOutputId = null;
store.on('output', ({ current }) => {
  const id = current && current.browserPlayer ? current.id : null;
  if (id === deviceOutputId) return;
  deviceOutputId = id;
  stopDeviceSound();
  if (id && deviceSoundOn()) startDeviceSound();
});

let barVideoId = null;
function paintPlayer() {
  const cur = store.state.current;
  document.body.classList.toggle('has-player', !!cur);
  $('player').hidden = !cur;
  $('mini').hidden = !cur;
  document.title = cur ? `${cur.song.title} · ${cur.song.artists}` : 'Pinson';
  if (!cur) {
    barVideoId = null;
    return;
  }
  const song = cur.song;
  if (song.videoId === barVideoId) return;
  barVideoId = song.videoId;
  bar.art.replaceChildren(cover(song.artworkUrl, { size: 40, eager: true }));
  bar.title.textContent = song.title;
  bar.sub.textContent = song.album ? `${song.artists} · ${song.album}` : song.artists;
  bar.heart.replaceChildren(heartButton(song));
  mini.art.replaceChildren(cover(song.artworkUrl, { size: 40, eager: true }));
  mini.title.textContent = song.title;
  mini.sub.textContent = song.artists;
}

// Progress lines and times everywhere ([data-pos-fill], [data-pos-cur], [data-pos-dur]).
function paintPosition() {
  const cur = store.state.current;
  const dur = cur && cur.song.durationSecs ? cur.song.durationSecs * 1000 : 0;
  const pos = cur ? positionMs() : 0;
  const frac = dur ? Math.min(1, pos / dur) : 0;
  for (const el of document.querySelectorAll('[data-pos-fill]')) el.style.transform = `scaleX(${frac})`;
  for (const el of document.querySelectorAll('[data-pos-cur]')) el.textContent = fmtTime(pos);
  for (const el of document.querySelectorAll('[data-pos-dur]')) el.textContent = dur ? fmtTime(dur) : '--:--';
  for (const el of document.querySelectorAll('[data-seek]')) {
    el.setAttribute('aria-valuemax', String(Math.round(dur / 1000)));
    el.setAttribute('aria-valuenow', String(Math.round(pos / 1000)));
    el.setAttribute('aria-valuetext', `${fmtTime(pos)} of ${dur ? fmtTime(dur) : '--:--'}`);
    el.setAttribute('aria-disabled', String(!dur));
  }
}
setInterval(paintPosition, 250);

store.on('state', () => {
  paintPlayer();
  paintPosition();
  // Lyrics are fetched when the song changes, not when Now playing opens (one cache slot, stale answers dropped).
  if (store.state.current) prefetchLyrics(store.state.current.song);
});

// ---------- banner, notices, shortcuts ----------
store.on('connection', (on) => {
  $('banner').hidden = on;
});
store.on('notice', (msg) => {
  if (msg.type === 'songFailed') toast(`Skipped ${msg.title}: ${msg.reason}`, 4500);
  else if (msg.type === 'autoplayFailed') toast("Couldn't load autoplay suggestions");
});

// "+ New playlist" (sidebar, Home tile, Library row): ask for a name, create, open the new page.
document.addEventListener('click', (e) => {
  if (!e.target.closest('[data-action="new-playlist"]')) return;
  promptName({ title: 'New playlist', submit: (name) => playlists.create(name) }).then((pl) => {
    if (pl) go('#/playlist/' + encodeURIComponent(pl.id));
  });
});

document.addEventListener('keydown', (e) => {
  if (e.defaultPrevented || e.metaKey || e.ctrlKey || e.altKey) return;
  const t = e.target;
  const typing = t.closest && t.closest('input, textarea, select, [contenteditable="true"]');
  if (e.key === 'Escape' && onNow() && !typing) {
    back();
  } else if (e.key === ' ' && !typing && !(t.closest && t.closest('button, a, [role="button"]'))) {
    e.preventDefault();
    togglePlay();
  }
});

onChange((page) => {
  closeLayers(); // a sheet or menu belongs to the page it was opened on
  document.body.dataset.route = page.pattern.split('/')[1];
  markActive(page);
});

paintPlayer();
paintPosition();
start($('view'));
watchOutput();
startDevicePlayer();
connect();
