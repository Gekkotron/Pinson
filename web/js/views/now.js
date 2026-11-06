// Now playing (`#/now`).
// Laptop: big glowing cover on the left, tabs Up next · Lyrics on the right (the player bar has the controls).
// Phone: full screen with a Song · Lyrics switch, cover, title/artist/♥, progress, controls and an
// "Up next" button that opens a sheet. The background is tinted from the cover colour.

import { playSuggestion, queuePlay, queueRemove, setAutoplay } from '../api.js';
import { dominantColor, tintColor } from '../color.js';
import { icon } from '../icons.js';
import { back, go } from '../router.js';
import { store } from '../store.js';
import { mountLyrics } from './lyrics.js';
import {
  cover,
  emptyState,
  h,
  heartButton,
  iconButton,
  pillButton,
  plural,
  seekable,
  sheet,
  songMenu,
  songRow,
  toast,
  transport,
} from '../ui.js';

const NEUTRAL_TINT = 'hsl(240 4% 24%)';
const NEUTRAL_GLOW = 'rgb(0 0 0 / 0%)';

// Remembered while the app stays open.
let laptopTab = 'upnext'; // "upnext" | "lyrics"
let phoneMode = 'song'; // "song" | "lyrics"
// Big-screen (TV) mode: smaller cover, more room for the lyrics. Set by the `/lyrics` shortcut
// (`#/now?tab=lyrics&tv=1`) and kept until the page reloads without it.
let bigScreen = false;

/** Opens Now playing on a tab ("upnext" or "lyrics"). */
export function openNow(tab) {
  if (tab) laptopTab = tab;
  go('#/now');
}

/** The Autoplay section: label and switch, then what autoplay will play (or that it is off). */
function autoplaySection(list, autoplay) {
  const toggle = h('button', {
    type: 'button',
    class: 'switch',
    role: 'switch',
    'aria-checked': String(autoplay.enabled),
    'aria-label': 'Autoplay',
    onclick: () => setAutoplay(!autoplay.enabled).catch((err) => toast(err.message)),
  });
  list.append(h('div', { class: 'upnext-label upnext-auto-head' }, h('span', { text: 'Autoplay' }), toggle));
  if (!autoplay.enabled) {
    list.append(h('p', { class: 'upnext-empty', text: 'Autoplay is off' }));
    return;
  }
  if (!autoplay.songs.length) return;
  if (autoplay.basedOn) list.append(h('p', { class: 'upnext-based', text: `Based on ${autoplay.basedOn.title}` }));
  for (const song of autoplay.songs) {
    list.append(
      songRow(song, {
        onPlay: () => playSuggestion(song.videoId).catch((err) => toast(err.message)),
        menu: () => songMenu(song),
        cls: 'upnext-auto',
      }),
    );
  }
}

/** The Up next list: the current song, the upcoming entries with ✕, then the Autoplay section. */
function upNextList() {
  const s = store.state;
  const list = h('div', { class: 'upnext' });
  if (!s.current) return list;
  list.append(
    h('div', { class: 'upnext-label', text: 'Now playing' }),
    songRow(s.current.song, {}),
  );
  const songs = s.upcoming.map((e) => e.song);
  list.append(
    h(
      'div',
      { class: 'upnext-label' },
      'Up next',
      s.upcoming.length ? h('span', { class: 'upnext-count', text: plural(s.upcoming.length, 'song') }) : null,
    ),
  );
  if (!s.upcoming.length && !(s.autoplay.enabled && s.autoplay.songs.length)) {
    list.append(h('p', { class: 'upnext-empty', text: 'Nothing after this song. Add some from Home or Search.' }));
  }
  s.upcoming.forEach((entry, i) => {
    list.append(
      songRow(entry.song, {
        // Jump: play the upcoming list from this song (the songs before it are dropped).
        onPlay: () => queuePlay(songs, { startIndex: i }).catch((err) => toast(err.message)),
        menu: () => songMenu(entry.song),
        onRemove: () => queueRemove(entry.id).catch((err) => toast(err.message)),
      }),
    );
  });
  autoplaySection(list, s.autoplay);
  return list;
}

export function renderNow(el, page) {
  const query = (page && page.query) || {};
  if (query.tab === 'upnext' || query.tab === 'lyrics') laptopTab = query.tab;
  if (query.tab === 'lyrics') phoneMode = 'lyrics';
  if (query.tv === '1') bigScreen = true;
  const root = h('section', {
    class: bigScreen ? 'now now-tv' : 'now',
    'aria-label': 'Now playing',
    dataset: { tab: laptopTab, mode: phoneMode },
  });
  el.append(root);

  // ---- pieces (built once, updated on each state) ----
  const setTab = (tab) => {
    laptopTab = tab;
    root.dataset.tab = tab;
    for (const b of tabs.querySelectorAll('[role=tab]')) b.setAttribute('aria-selected', String(b.dataset.tab === tab));
  };
  const setMode = (mode) => {
    phoneMode = mode;
    root.dataset.mode = mode;
    for (const b of modeSwitch.querySelectorAll('button')) b.setAttribute('aria-pressed', String(b.dataset.mode === mode));
  };
  const tabBtn = (tab, label) =>
    h('button', { type: 'button', role: 'tab', class: 'now-tab', dataset: { tab }, onclick: () => setTab(tab) }, label);
  const modeBtn = (mode, label) =>
    h('button', { type: 'button', class: 'now-mode', dataset: { mode }, onclick: () => setMode(mode) }, label);

  const modeSwitch = h('div', { class: 'now-switch', role: 'group', 'aria-label': 'View' }, modeBtn('song', 'Song'), modeBtn('lyrics', 'Lyrics'));
  const top = h(
    'div',
    { class: 'now-top' },
    iconButton('chevronDown', 'Close now playing', back, { size: 28 }),
    modeSwitch,
    h('span', { class: 'now-top-spacer' }),
  );
  const art = h('div', { class: 'now-art' });
  const title = h('div', { class: 'now-title' });
  const artist = h('div', { class: 'now-artist' });
  const heartSlot = h('div', { class: 'now-heart' });
  const meta = h('div', { class: 'now-meta' }, h('div', { class: 'now-meta-text' }, title, artist), heartSlot);
  const progress = h(
    'div',
    { class: 'now-progress' },
    seekable(h('div', { class: 'seek-hit' }, h('div', { class: 'bar' }, h('i', { dataset: { posFill: '' } })))),
    h('div', { class: 'now-times' }, h('span', { dataset: { posCur: '' } }, '0:00'), h('span', { dataset: { posDur: '' } }, '0:00')),
  );
  const controls = h('div', { class: 'now-controls' }, transport({ size: 36, mainSize: 40, mainCls: 'now-play' }));
  let queueSheet = null;
  const upnextBtn = h(
    'button',
    {
      type: 'button',
      class: 'now-upnext-btn',
      onclick: () => {
        const body = h('div', { class: 'sheet-upnext' }, upNextList());
        queueSheet = sheet(body, { title: 'Up next', cls: 'sheet-tall' });
        queueSheet.closed.then(() => (queueSheet = null));
      },
    },
    icon('queue', { size: 20 }),
    h('span', { text: 'Up next' }),
  );
  const tabs = h('div', { class: 'now-tabs', role: 'tablist' }, tabBtn('upnext', 'Up next'), tabBtn('lyrics', 'Lyrics'));
  const upnextPane = h('div', { class: 'now-pane now-upnext', role: 'tabpanel' });
  const lyricsPane = h('div', { class: 'now-pane now-lyrics', role: 'tabpanel' });
  const idle = h(
    'div',
    { class: 'now-idle' },
    emptyState('music', 'Nothing playing', 'Pick a playlist or your favorites to start the music.', pillButton('home', 'Go to Home', () => go('#/home'), { primary: true })),
  );
  const bg = h('div', { class: 'now-bg', 'aria-hidden': 'true' });
  root.append(bg, top, art, meta, progress, controls, upnextBtn, tabs, upnextPane, lyricsPane, idle);
  setTab(laptopTab);
  setMode(phoneMode);

  // ---- updates ----
  let shownVideoId;
  let shownArtwork;
  const paintSong = (song) => {
    title.textContent = song.title;
    artist.textContent = song.album ? `${song.artists} · ${song.album}` : song.artists;
    heartSlot.replaceChildren(heartButton(song, { size: 28 }));
    if (song.artworkUrl === shownArtwork && art.firstChild) return;
    shownArtwork = song.artworkUrl;
    const next = cover(song.artworkUrl, { cls: 'now-cover', eager: true });
    const old = art.querySelector('.now-cover:not(.leaving)');
    if (old) {
      old.classList.add('leaving');
      setTimeout(() => old.remove(), 400);
    }
    art.append(next);
    root.style.setProperty('--tint', NEUTRAL_TINT);
    root.style.setProperty('--glow', NEUTRAL_GLOW);
    dominantColor(song.artworkUrl).then((rgb) => {
      if (shownArtwork !== song.artworkUrl || !rgb) return;
      root.style.setProperty('--tint', tintColor(rgb));
      root.style.setProperty('--glow', `rgb(${rgb.join(' ')} / 45%)`);
    });
  };

  const paint = () => {
    const s = store.state;
    root.classList.toggle('is-idle', !s.current);
    if (s.current && s.current.song.videoId !== shownVideoId) {
      shownVideoId = s.current.song.videoId;
      paintSong(s.current.song);
    }
    if (!s.current) {
      shownVideoId = null;
      shownArtwork = undefined;
      art.replaceChildren();
      root.style.setProperty('--tint', NEUTRAL_TINT);
      root.style.setProperty('--glow', NEUTRAL_GLOW);
    }
    const scroll = upnextPane.scrollTop;
    upnextPane.replaceChildren(upNextList());
    upnextPane.scrollTop = scroll;
    if (queueSheet) {
      const scroller = queueSheet.el.querySelector('.sheet-body');
      const y = scroller.scrollTop;
      queueSheet.el.querySelector('.sheet-upnext').replaceChildren(upNextList());
      scroller.scrollTop = y;
    }
  };
  paint();
  const off = store.on('state', paint);
  const offLyrics = mountLyrics(lyricsPane, root);
  return () => {
    off();
    offLyrics();
    if (queueSheet) queueSheet.close();
  };
}
