// Now playing (`#/now`).
// Laptop: big glowing cover on the left, tabs Up next · Lyrics on the right (the player bar has the controls).
// Phone: full screen with a Song · Lyrics switch, cover, title/artist/♥, progress, controls and an
// "Up next" button that opens a sheet. The background is tinted from the cover colour.

import { animateCover } from '../animated-artwork.js';
import { player, playSuggestion, queueMove, queuePlay, queueRemove, setAutoplay, setCrossfade } from '../api.js';
import { enableReorder } from '../reorder.js';
import { upNextKey } from '../upnext-key.js';
import { icon } from '../icons.js';
import { back, go } from '../router.js';
import { deviceId } from '../device-id.js';
import { choose } from '../output.js';
import { isOtherDevice } from '../output-pick.js';
import { store } from '../store.js';
import { openSpeakers } from '../speakers.js';
import { sleepButton } from '../sleep.js';
import { keyDebugBox, listenRemote, loadCustomKeys, remoteSetupSeen } from '../tv-remote.js';
import { mountLyrics } from './lyrics.js';
import { openRemoteSetup } from './remote-setup.js';
import {
  cover,
  emptyState,
  h,
  heartButton,
  iconButton,
  pillButton,
  plural,
  previous,
  seekable,
  sheet,
  songMenu,
  songRow,
  skip,
  toast,
  togglePlay,
  transport,
} from '../ui.js';


// Remembered while the app stays open.
let laptopTab = 'upnext'; // "upnext" | "lyrics"
let phoneMode = 'song'; // "song" | "lyrics"
// Big-screen (TV) mode: smaller cover, more room for the lyrics. Set by the `/lyrics` shortcut
// (`#/now?tab=lyrics&tv=1`) and kept until the page reloads without it.
let bigScreen = false;

// Big-screen lyrics text size: a scale on top of the TV size, saved per device (the TV keeps it).
const LYRICS_SCALE_KEY = 'pinson.lyricsScale';
const LYRICS_SCALE_MIN = 0.6;
const LYRICS_SCALE_MAX = 1.6;
const LYRICS_SCALE_STEP = 0.1;

function loadLyricsScale() {
  try {
    const v = Number(localStorage.getItem(LYRICS_SCALE_KEY));
    if (v >= LYRICS_SCALE_MIN && v <= LYRICS_SCALE_MAX) return v;
  } catch {
    // storage blocked: use the default size
  }
  return 1;
}

function saveLyricsScale(v) {
  try {
    if (v === 1) localStorage.removeItem(LYRICS_SCALE_KEY);
    else localStorage.setItem(LYRICS_SCALE_KEY, String(v));
  } catch {
    // storage blocked: the size still applies until the page reloads
  }
}

/**
 * The big-screen pill: A− / A+ change the lyrics text size, the remote icon calls `onRemote()`.
 * Returns `{ el, change(±1) }`.
 */
function lyricsSizeControl(root, onRemote) {
  let scale = loadLyricsScale();
  const smaller = h('button', { type: 'button', class: 'now-size-btn now-size-down', 'aria-label': 'Smaller lyrics', title: 'Smaller lyrics' }, 'A');
  const bigger = h('button', { type: 'button', class: 'now-size-btn now-size-up', 'aria-label': 'Bigger lyrics', title: 'Bigger lyrics' }, 'A');
  const apply = () => {
    root.style.setProperty('--lyrics-scale', String(scale));
    smaller.disabled = scale <= LYRICS_SCALE_MIN + 1e-9;
    bigger.disabled = scale >= LYRICS_SCALE_MAX - 1e-9;
  };
  const change = (dir) => {
    const next = Math.round((scale + dir * LYRICS_SCALE_STEP) * 10) / 10;
    scale = Math.min(LYRICS_SCALE_MAX, Math.max(LYRICS_SCALE_MIN, next));
    saveLyricsScale(scale);
    apply();
  };
  smaller.onclick = () => change(-1);
  bigger.onclick = () => change(1);
  apply();
  const remote = h(
    'button',
    { type: 'button', class: 'now-size-btn now-size-remote', 'aria-label': 'Set up your remote', title: 'Set up your remote', onclick: onRemote },
    icon('remote', { size: 22 }),
  );
  const el = h('div', { class: 'now-size', role: 'group', 'aria-label': 'Lyrics text size and remote' }, smaller, bigger, remote);
  return { el, change };
}

/** TV remote buttons on the big screen: ↑/↓ text size, OK play/pause, ←/→ previous/next, colour keys. */
function listenTvRemote(size, getCustom) {
  const status = () => store.state.current && store.state.status;
  const fail = (err) => toast(err.message);
  return listenRemote({
    bigger: () => size.change(1),
    smaller: () => size.change(-1),
    playPause: togglePlay,
    play: () => status() && status() !== 'playing' && player.resume().catch(fail),
    pause: () => status() === 'playing' && player.pause().catch(fail),
    next: skip,
    previous,
  }, getCustom);
}

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

const CROSSFADE_CHOICES = [[0, 'Off'], [3, '3 s'], [6, '6 s'], [12, '12 s']];

/** The Crossfade row: label, then four choices (Off, 3 s, 6 s, 12 s). */
function crossfadeSection(list, secs) {
  const group = h(
    'div',
    { class: 'segmented', role: 'group', 'aria-label': 'Crossfade' },
    ...CROSSFADE_CHOICES.map(([value, label]) =>
      h('button', {
        type: 'button',
        class: 'segmented-btn',
        'aria-pressed': String(value === secs),
        text: label,
        onclick: () => setCrossfade(value).catch((err) => toast(err.message)),
      }),
    ),
  );
  list.append(h('div', { class: 'upnext-label upnext-auto-head' }, h('span', { text: 'Crossfade' }), group));
}

// True while an Up next row is being dragged: a rebuild then would repaint the rows under the finger.
let upNextDragging = false;

/**
 * The Up next list: the current song, the upcoming entries (drag handle to move, ✕ to remove),
 * then the Autoplay section. `repaint(force)` is called when a drag ends, to show what changed
 * meanwhile (`force`: the move failed, so the rows on screen are in the wrong order).
 */
function upNextList(repaint) {
  const s = store.state;
  const list = h('div', { class: 'upnext' });
  if (!s.current) return list;
  list.append(
    h('div', { class: 'upnext-label', text: 'Now playing' }),
    // Same "⋯" menu as the other rows, so the playing song can go to a playlist from here.
    songRow(s.current.song, { menu: () => songMenu(s.current.song) }),
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
  // The upcoming rows sit alone in their own box, which is what drag-to-move reorders.
  const rows = h('div', { class: 'upnext-rows' });
  s.upcoming.forEach((entry, i) => {
    const row = songRow(entry.song, {
      // Jump: play the upcoming list from this song (the songs before it are dropped).
      onPlay: () => queuePlay(songs, { startIndex: i }).catch((err) => toast(err.message)),
      menu: () => songMenu(entry.song),
      draggable: true,
      onRemove: () => queueRemove(entry.id).catch((err) => toast(err.message)),
    });
    row.dataset.entry = entry.id; // the same song can be queued twice: entries have their own id
    rows.append(row);
  });
  if (s.upcoming.length) {
    list.append(rows);
    enableReorder(rows, {
      onState: (on) => {
        upNextDragging = on;
        if (!on) repaint(false);
      },
      // Named by its new neighbour, not an index, so the move stays right if a song ends meanwhile.
      onDrop: (_ids, row) => {
        const prev = row.previousElementSibling;
        queueMove(row.dataset.entry, prev ? Number(prev.dataset.entry) : null).catch((err) => {
          toast(err.message);
          repaint(true);
        });
      },
    });
  }
  autoplaySection(list, s.autoplay);
  crossfadeSection(list, s.crossfadeSecs);
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
  const elsewhere = h('div', { class: 'now-elsewhere' });
  elsewhere.hidden = true;
  const paintElsewhere = () => {
    const cur = store.output?.current;
    elsewhere.hidden = !isOtherDevice(cur, deviceId());
    if (elsewhere.hidden) return;
    elsewhere.replaceChildren(
      h('span', { text: `Playing on ${cur.name}` }),
      pillButton('speaker', 'Play here', () => choose({ kind: 'browser' }), { primary: true }),
    );
  };
  paintElsewhere();
  const offElsewhere = store.on('output', paintElsewhere);
  const progress = h(
    'div',
    { class: 'now-progress' },
    seekable(h('div', { class: 'seek-hit' }, h('div', { class: 'bar' }, h('i', { dataset: { posFill: '' } })))),
    h('div', { class: 'now-times' }, h('span', { dataset: { posCur: '' } }, '0:00'), h('span', { dataset: { posDur: '' } }, '0:00')),
  );
  // Speakers sits at the end of the row; the empty first cell keeps the transport centred.
  const speakersBtn = iconButton('speaker', 'Speakers', openSpeakers, { size: 26, cls: 'now-speakers' });
  speakersBtn.hidden = true;
  const paintSpeakers = () => (speakersBtn.hidden = !store.output?.current?.speakers);
  paintSpeakers();
  const offOutput = store.on('output', paintSpeakers);
  // The sleep timer mirrors Speakers on the left of the row.
  const sleepBtn = sleepButton({ size: 26, cls: 'now-sleep' });
  const controls = h(
    'div',
    { class: 'now-controls' },
    sleepBtn.el,
    h('div', { class: 'now-transport' }, transport({ size: 36, mainSize: 40, mainCls: 'now-play' })),
    speakersBtn,
  );
  let queueSheet = null;
  const upnextBtn = h(
    'button',
    {
      type: 'button',
      class: 'now-upnext-btn',
      onclick: () => {
        const body = h('div', { class: 'sheet-upnext' }, upNextList(repaint));
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
  root.append(bg, top, art, meta, elsewhere, progress, controls, upnextBtn, tabs, upnextPane, lyricsPane, idle);
  // Big screen only: the A− / A+ lyrics size pill (over the lower-right corner of the lyrics), the TV
  // remote buttons, and with `keys=1` (`/lyrics?keys=1`) a box showing what each remote button sends.
  let offRemote = null;
  let keyDebug = null;
  let closeSetup = null;
  if (bigScreen) {
    // The remote mapping saved by "Set up your remote" (wins over the built-in keys).
    let customKeys = loadCustomKeys();
    const setup = () => {
      if (closeSetup) closeSetup();
      closeSetup = openRemoteSetup((map) => (customKeys = map));
    };
    const size = lyricsSizeControl(root, setup);
    root.append(size.el);
    offRemote = listenTvRemote(size, () => customKeys);
    if (query.keys === '1') keyDebug = keyDebugBox();
    else if (!remoteSetupSeen()) setup(); // first visit on this TV
  }
  setTab(laptopTab);
  setMode(phoneMode);

  // ---- updates ----
  let shownVideoId;
  let stopAnimation = () => {};
  let shownList; // upNextKey of the list on screen
  const paintSong = (song) => {
    title.textContent = song.title;
    artist.textContent = song.album ? `${song.artists} · ${song.album}` : song.artists;
    heartSlot.replaceChildren(heartButton(song, { size: 28 }));
    stopAnimation();
    const image = cover(song.artworkUrl, { cls: 'now-cover-media', eager: true });
    const next = h('div', { class: 'now-cover' }, image);
    const old = art.querySelector('.now-cover:not(.leaving)');
    if (old) {
      old.classList.add('leaving');
      setTimeout(() => old.remove(), 400);
    }
    art.append(next); // the background and glow colours come from tint.js
    stopAnimation = animateCover(image, song, { canvas: bigScreen }); // TV video ignores corners
  };

  const paint = () => {
    const s = store.state;
    root.classList.toggle('is-idle', !s.current);
    if (s.current && s.current.song.videoId !== shownVideoId) {
      shownVideoId = s.current.song.videoId;
      paintSong(s.current.song);
    }
    if (!s.current) {
      stopAnimation();
      shownVideoId = null;
      art.replaceChildren();
    }
    // Hearts and the playing row are kept in sync by ui.js; only a new list needs new rows.
    const key = upNextKey(s);
    // (The phone sheet builds its own list from the same state when it opens.)
    if (key === shownList || upNextDragging) return;
    shownList = key;
    const scroll = upnextPane.scrollTop;
    upnextPane.replaceChildren(upNextList(repaint));
    upnextPane.scrollTop = scroll;
    if (queueSheet) {
      const scroller = queueSheet.el.querySelector('.sheet-body');
      const y = scroller.scrollTop;
      queueSheet.el.querySelector('.sheet-upnext').replaceChildren(upNextList(repaint));
      scroller.scrollTop = y;
    }
  };
  /** After an Up next drag: rebuild if the list changed meanwhile, or always when `force`. */
  function repaint(force) {
    if (force) shownList = null;
    paint();
  }
  paint();
  const off = store.on('state', paint);
  const offLyrics = mountLyrics(lyricsPane, root);
  return () => {
    stopAnimation();
    sleepBtn.dispose();
    off();
    offOutput();
    offElsewhere();
    offLyrics();
    if (offRemote) offRemote();
    if (keyDebug) keyDebug.stop();
    if (closeSetup) closeSetup();
    if (queueSheet) queueSheet.close();
  };
}
