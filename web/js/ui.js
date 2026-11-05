// Shared UI pieces: element builder, covers, song rows, list headers, toasts, menus, sheets.
// Hearts, play/pause buttons and "playing" rows stay in sync with the store by themselves.

import { artworkAt, favoriteToggle, player, playlists, playNext, queueAdd } from './api.js';
import { icon, setIcon } from './icons.js';
import { store } from './store.js';

const WIDE = window.matchMedia('(min-width: 900px)');

/** True in the laptop layout (sidebar + player bar). */
export const isWide = () => WIDE.matches;

/**
 * Element builder. `props`: `class`, `text`, `dataset`, `style`, plain attributes, and
 * `on<event>` handlers. Children may be nodes, strings, arrays, or null/false (skipped).
 */
export function h(tag, props = {}, ...children) {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(props || {})) {
    if (v == null || v === false) continue;
    if (k === 'class') el.className = v;
    else if (k === 'text') el.textContent = v;
    else if (k === 'dataset') Object.assign(el.dataset, v);
    else if (k === 'style') Object.assign(el.style, v);
    else if (k.startsWith('on') && typeof v === 'function') el.addEventListener(k.slice(2), v);
    else el.setAttribute(k, v === true ? '' : v);
  }
  append(el, children);
  return el;
}

function append(el, children) {
  for (const c of children) {
    if (c == null || c === false) continue;
    if (Array.isArray(c)) append(el, c);
    else el.append(c);
  }
}

/** 83 s → "1:23". */
export function fmtTime(ms) {
  const s = Math.max(0, Math.floor(ms / 1000));
  const m = Math.floor(s / 60);
  if (m >= 60) return `${Math.floor(m / 60)}:${String(m % 60).padStart(2, '0')}:${String(s % 60).padStart(2, '0')}`;
  return `${m}:${String(s % 60).padStart(2, '0')}`;
}

/** Total length of songs as "1 hr 4 min" / "23 min", or "" when unknown. */
export function fmtTotal(songs) {
  const secs = songs.reduce((t, s) => t + (s.durationSecs || 0), 0);
  if (!secs) return '';
  const min = Math.round(secs / 60);
  return min >= 60 ? `${Math.floor(min / 60)} hr ${min % 60} min` : `${min} min`;
}

export const plural = (n, word) => `${n} ${word}${n === 1 ? '' : 's'}`;

// ---------- covers ----------

/**
 * A square cover. `size` in px (omit to fill the parent's width). `mosaic`: up to 4 URLs drawn
 * as a 2×2 grid (4 needed; fewer shows the first). No URL: a tidy placeholder.
 */
export function cover(url, { size, mosaic, cls = '', eager = false } = {}) {
  const box = h('div', { class: ('cover ' + cls).trim() });
  if (size) {
    box.style.width = size + 'px';
    box.style.height = size + 'px';
  }
  const urls = (mosaic || [url]).filter(Boolean);
  const px = size ? Math.min(1200, Math.ceil((size * 2) / 60) * 60) : 1200;
  const placeholder = () => {
    box.classList.add('cover-empty');
    box.replaceChildren(icon('music', { size: size ? Math.max(16, Math.round(size * 0.42)) : 72 }));
  };
  if (!urls.length) {
    placeholder();
    return box;
  }
  const tiles = urls.length >= 4 ? urls.slice(0, 4) : urls.slice(0, 1);
  if (tiles.length === 4) box.classList.add('cover-mosaic');
  for (const u of tiles) {
    const img = h('img', {
      alt: '',
      src: artworkAt(u, tiles.length === 4 ? Math.ceil(px / 2) : px),
      loading: eager ? 'eager' : 'lazy',
      decoding: 'async',
      referrerpolicy: 'no-referrer',
      draggable: 'false',
    });
    img.addEventListener('load', () => img.classList.add('loaded'), { once: true });
    img.addEventListener('error', placeholder, { once: true });
    box.append(img);
  }
  return box;
}

// ---------- buttons ----------

/** A round icon button with an accessible label. */
export function iconButton(name, label, onClick, { cls = '', size = 24 } = {}) {
  return h(
    'button',
    {
      type: 'button',
      class: ('icon-btn ' + cls).trim(),
      'aria-label': label,
      title: label,
      onclick: (e) => {
        e.stopPropagation();
        onClick(e);
      },
    },
    icon(name, { size }),
  );
}

/** A pill button (`primary`: white, filled). */
export function pillButton(name, label, onClick, { primary = false } = {}) {
  return h(
    'button',
    { type: 'button', class: 'pill' + (primary ? ' pill-primary' : ''), onclick: onClick },
    icon(name, { size: 20 }),
    h('span', { text: label }),
  );
}

/** ♥ for `song`; stays in sync with the favorites list everywhere. */
export function heartButton(song, { size = 22, cls = '' } = {}) {
  const btn = iconButton('heart', '', () => favoriteToggle(song).catch((err) => toast(err.message)), {
    cls: 'heart ' + cls,
    size,
  });
  btn.dataset.heart = song.videoId;
  btn.dataset.title = song.title;
  syncHeart(btn);
  return btn;
}

function syncHeart(btn) {
  const on = store.isFavorite(btn.dataset.heart);
  btn.classList.toggle('on', on);
  btn.setAttribute('aria-pressed', String(on));
  const label = on ? `Remove ${btn.dataset.title} from favorites` : `Add ${btn.dataset.title} to favorites`;
  btn.setAttribute('aria-label', label);
  btn.title = on ? 'Remove from favorites' : 'Add to favorites';
  setIcon(btn.querySelector('svg'), on ? 'heartFill' : 'heart');
}

// ---------- transport ----------

/** Pause/resume, skip and previous. skip/previous name the current entry (409 = already moved on). */
export function togglePlay() {
  const s = store.state;
  if (!s.current) return;
  const call = s.status === 'playing' ? player.pause() : player.resume();
  call.catch((err) => toast(err.message));
}
export function skip() {
  const cur = store.state.current;
  if (cur) player.skip(cur.id).catch(ignoreStale);
}
export function previous() {
  const cur = store.state.current;
  if (cur) player.previous(cur.id).catch(ignoreStale);
}
function ignoreStale(err) {
  if (err.status !== 409) toast(err.message);
}

/** Play/pause button that follows the playback status. */
export function playPauseButton({ cls = '', size = 24 } = {}) {
  const btn = iconButton('pause', 'Pause', togglePlay, { cls: 'playpause ' + cls, size });
  btn.dataset.playpause = '';
  syncPlayPause(btn);
  return btn;
}

function syncPlayPause(btn) {
  const playing = store.state.status === 'playing';
  setIcon(btn.querySelector('svg'), playing ? 'pause' : 'play');
  btn.setAttribute('aria-label', playing ? 'Pause' : 'Play');
  btn.title = playing ? 'Pause' : 'Play';
  btn.disabled = !store.state.current;
}

/** [previous, play/pause, next] buttons. */
export function transport({ size = 24, mainSize = size, mainCls = '' } = {}) {
  return [
    iconButton('prev', 'Previous', previous, { size, cls: 'transport-btn' }),
    playPauseButton({ size: mainSize, cls: mainCls }),
    iconButton('next', 'Next', skip, { size, cls: 'transport-btn' }),
  ];
}

// ---------- song rows ----------

/**
 * One song in any list. Options:
 * - `onPlay()`: tap/Enter on the row.
 * - `menu`: items for the "⋯" button (array, or a function returning one), see `menu()`.
 * - `draggable`: show a drag handle (`.drag-handle`) for reordering.
 * - `onRemove()`: show a ✕ button.
 */
export function songRow(song, { onPlay, menu: items, draggable = false, onRemove } = {}) {
  const sub = h('div', { class: 'song-sub' }, h('span', { text: song.artists }));
  if (song.album) sub.append(h('span', { class: 'song-album', text: ' · ' + song.album }));
  const row = h(
    'div',
    { class: 'song-row', dataset: { vid: song.videoId } },
    draggable &&
      h('span', { class: 'drag-handle', 'aria-hidden': 'true' }, icon('drag', { size: 20 })),
    h(
      'div',
      { class: 'song-art' },
      cover(song.artworkUrl, { size: 48 }),
      h('span', { class: 'eq', 'aria-hidden': 'true' }, h('i'), h('i'), h('i')),
    ),
    h('div', { class: 'song-text' }, h('div', { class: 'song-title', text: song.title }), sub),
    h('span', {
      class: 'song-dur',
      text: song.durationSecs ? fmtTime(song.durationSecs * 1000) : '',
    }),
    heartButton(song, { size: 20 }),
    items &&
      iconButton('more', `More for ${song.title}`, (e) =>
        menu(e.currentTarget, typeof items === 'function' ? items() : items),
      ),
    onRemove && iconButton('close', `Remove ${song.title} from the queue`, onRemove, { size: 20, cls: 'remove-btn' }),
  );
  if (onPlay) {
    row.tabIndex = 0;
    row.setAttribute('role', 'button');
    row.setAttribute('aria-label', `Play ${song.title} by ${song.artists}`);
    row.addEventListener('click', () => onPlay());
    row.addEventListener('keydown', (e) => {
      if (e.target === row && (e.key === 'Enter' || e.key === ' ')) {
        e.preventDefault();
        onPlay();
      }
    });
  }
  syncRow(row);
  return row;
}

function syncRow(row) {
  const cur = store.state.current;
  const on = !!cur && cur.song.videoId === row.dataset.vid;
  row.classList.toggle('is-current', on);
  row.classList.toggle('is-paused', on && store.state.status !== 'playing');
}

// ---------- page pieces ----------

/** Header for list pages: big art, kicker, title, meta line and action buttons. */
export function listHeader({ art, kicker, title, meta, actions = [] }) {
  return h(
    'header',
    { class: 'list-header' },
    h('div', { class: 'list-header-art' }, art),
    h(
      'div',
      { class: 'list-header-text' },
      kicker && h('div', { class: 'kicker', text: kicker }),
      h('h1', { class: 'list-header-title', text: title }),
      meta && h('div', { class: 'list-header-meta', text: meta }),
      h('div', { class: 'list-header-actions' }, actions),
    ),
  );
}

/** Centered icon + title + text, for empty pages. */
export function emptyState(iconName, title, text, action) {
  return h(
    'div',
    { class: 'empty' },
    h('div', { class: 'empty-icon' }, icon(iconName, { size: 36 })),
    h('h2', { text: title }),
    text && h('p', { text }),
    action,
  );
}

// ---------- toast ----------
let toastTimer = 0;

/** A short message above the player. */
export function toast(text, ms = 2600) {
  const el = document.getElementById('toast');
  if (!el) return;
  el.textContent = text;
  el.classList.add('show');
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => el.classList.remove('show'), ms);
}

// ---------- sheets, menus, confirm ----------
const openLayers = [];

/**
 * Shows `content` in a bottom sheet (phone) or a centered popover card (laptop).
 * Returns {el, close, closed}; `closed` resolves when it goes away (Esc, scrim, close()).
 */
export function sheet(content, { title, cls = '' } = {}) {
  const prevFocus = document.activeElement;
  let resolveClosed;
  const closed = new Promise((r) => (resolveClosed = r));
  const panel = h(
    'div',
    { class: ('sheet ' + cls).trim(), role: 'dialog', 'aria-modal': 'true', 'aria-label': title || 'Dialog' },
    h('div', { class: 'sheet-grip', 'aria-hidden': 'true' }),
    title && h('div', { class: 'sheet-title', text: title }),
    h('div', { class: 'sheet-body' }, content),
  );
  const layer = h('div', { class: 'layer layer-sheet' }, h('div', { class: 'scrim' }), panel);
  let open = true;
  const close = () => {
    if (!open) return;
    open = false;
    openLayers.splice(openLayers.indexOf(entry), 1);
    layer.classList.remove('in');
    const done = () => layer.remove();
    if (reducedMotion()) done();
    else setTimeout(done, 220);
    if (prevFocus && prevFocus.focus) prevFocus.focus({ preventScroll: true });
    resolveClosed();
  };
  const entry = { close };
  openLayers.push(entry);
  layer.firstChild.addEventListener('click', close);
  document.body.append(layer);
  requestAnimationFrame(() => layer.classList.add('in'));
  const first = panel.querySelector('button, [href], input, [tabindex="0"]');
  (first || panel).focus({ preventScroll: true });
  if (!first) panel.tabIndex = -1;
  return { el: panel, close, closed };
}

/**
 * A menu of actions for `anchor`: popover next to it on a laptop, bottom sheet on a phone.
 * `items`: [{label, icon?, onSelect(), danger?, disabled?}]; falsy entries are skipped.
 */
export function menu(anchor, items) {
  const list = h('div', { class: 'menu', role: 'menu' });
  let close = () => {};
  for (const item of items.filter(Boolean)) {
    list.append(
      h(
        'button',
        {
          type: 'button',
          role: 'menuitem',
          class: 'menu-item' + (item.danger ? ' danger' : ''),
          disabled: item.disabled,
          onclick: () => {
            close();
            item.onSelect();
          },
        },
        item.icon && icon(item.icon, { size: 22 }),
        h('span', { text: item.label }),
      ),
    );
  }
  if (!isWide() || !anchor) {
    close = sheet(list, { cls: 'sheet-menu' }).close;
    return;
  }
  // A transparent full-screen layer: a click outside closes the menu without reaching the page.
  const layer = h('div', { class: 'layer layer-popover' }, list);
  const prevFocus = document.activeElement;
  const entry = { close: () => close() };
  close = () => {
    const i = openLayers.indexOf(entry);
    if (i < 0) return;
    openLayers.splice(i, 1);
    layer.remove();
    window.removeEventListener('resize', entry.close);
    if (prevFocus && prevFocus.focus) prevFocus.focus({ preventScroll: true });
  };
  layer.addEventListener('click', (e) => {
    if (e.target === layer) close();
  });
  layer.addEventListener('wheel', entry.close, { passive: true });
  window.addEventListener('resize', entry.close);
  openLayers.push(entry);
  document.body.append(layer);
  const r = anchor.getBoundingClientRect();
  const w = list.offsetWidth, hgt = list.offsetHeight;
  const left = Math.max(8, Math.min(r.right - w, innerWidth - w - 8));
  const below = r.bottom + 4 + hgt <= innerHeight - 8;
  list.style.left = left + 'px';
  list.style.top = (below ? r.bottom + 4 : Math.max(8, r.top - 4 - hgt)) + 'px';
  const first = list.querySelector('button:not([disabled])');
  if (first) first.focus({ preventScroll: true });
}

/**
 * Items for a song's "⋯" menu: Play next, Add to queue, Add to playlist…, Remove from playlist
 * (only with `inPlaylistId`) and the ♥ toggle. Pass it to `songRow` as `menu: () => songMenu(song)`.
 */
export function songMenu(song, { inPlaylistId } = {}) {
  const fav = store.isFavorite(song.videoId);
  const fail = (err) => toast(err.message);
  return [
    {
      label: 'Play next',
      icon: 'queue',
      onSelect: () => playNext(song).then(() => toast('Playing next'), fail),
    },
    {
      label: 'Add to queue',
      icon: 'addQueue',
      onSelect: () => queueAdd([song], 'end').then(() => toast('Added to the queue'), fail),
    },
    { label: 'Add to playlist…', icon: 'plus', onSelect: () => openPlaylistPicker([song]) },
    inPlaylistId && {
      label: 'Remove from playlist',
      icon: 'close',
      danger: true,
      onSelect: () =>
        playlists.removeSong(inPlaylistId, song.videoId).then(() => toast('Removed from the playlist'), fail),
    },
    {
      label: fav ? 'Remove from favorites' : 'Add to favorites',
      icon: fav ? 'heartFill' : 'heart',
      onSelect: () => favoriteToggle(song).then((on) => toast(on ? 'Added to favorites' : 'Removed from favorites'), fail),
    },
  ];
}

/**
 * A sheet asking for a playlist name. `submit(name)` does the work (an API call) and resolves
 * to a result; a rejection is shown under the field (the API's 400 text) and the sheet stays.
 * Resolves to that result, or null when the sheet is dismissed.
 */
export function promptName({ title, confirmLabel = 'Create', initial = '', submit }) {
  return new Promise((resolve) => {
    let result = null;
    let busy = false;
    const input = h('input', {
      class: 'field-input',
      type: 'text',
      name: 'name',
      value: initial,
      placeholder: 'Playlist name',
      autocomplete: 'off',
      'aria-label': 'Playlist name',
    });
    const error = h('div', { class: 'field-error', role: 'alert' });
    const ok = h('button', { type: 'submit', class: 'pill pill-primary' }, confirmLabel);
    const form = h(
      'form',
      {
        class: 'name-form',
        novalidate: true,
        onsubmit: async (e) => {
          e.preventDefault();
          if (busy) return;
          busy = true;
          ok.disabled = true;
          error.textContent = '';
          try {
            result = await submit(input.value);
            s.close();
          } catch (err) {
            error.textContent = err.message;
            input.setAttribute('aria-invalid', 'true');
            input.focus();
          } finally {
            busy = false;
            ok.disabled = false;
          }
        },
      },
      input,
      error,
      h(
        'div',
        { class: 'confirm-actions' },
        h('button', { type: 'button', class: 'pill', onclick: () => s.close() }, 'Cancel'),
        ok,
      ),
    );
    const s = sheet(form, { title, cls: 'sheet-name' });
    s.closed.then(() => resolve(result));
    input.focus({ preventScroll: true });
    input.select();
  });
}

/** Asks which playlist `songs` go to (or a new one), adds them, and toasts the outcome. */
export function openPlaylistPicker(songs) {
  const fail = (err) => toast(err.message);
  let s;
  const choose = (p) => {
    s.close();
    playlists.addSongs(p.id, songs).then((r) => {
      toast(r.skipped === songs.length ? `Already in ${p.name}` : `Added to ${p.name}`);
    }, fail);
  };
  const create = () =>
    promptName({ title: 'New playlist', submit: (name) => playlists.create(name, songs) }).then((pl) => {
      if (!pl) return;
      s.close();
      toast(`Added to ${pl.name}`);
    });
  const list = h(
    'div',
    { class: 'pick-list' },
    h(
      'button',
      { type: 'button', class: 'pick-row pick-new', onclick: create },
      h('div', { class: 'cover cover-new', style: { width: '48px', height: '48px' } }, icon('plus', { size: 24 })),
      h('div', { class: 'pick-text' }, h('div', { class: 'pick-name', text: 'New playlist' })),
    ),
    store.playlists.map((p) =>
      h(
        'button',
        { type: 'button', class: 'pick-row', onclick: () => choose(p) },
        cover(null, { size: 48, mosaic: p.artworkUrls }),
        h(
          'div',
          { class: 'pick-text' },
          h('div', { class: 'pick-name', text: p.name }),
          h('div', { class: 'pick-sub', text: plural(p.songCount, 'song') }),
        ),
      ),
    ),
  );
  s = sheet(list, { title: songs.length === 1 ? 'Add to playlist' : `Add ${plural(songs.length, 'song')} to playlist`, cls: 'sheet-pick' });
}

/** Asks inside the page (never `window.confirm`). Resolves true on confirm, false otherwise. */
export function confirmInPage(text, { confirmLabel = 'Delete', cancelLabel = 'Cancel', danger = true } = {}) {
  return new Promise((resolve) => {
    let answer = false;
    const body = h(
      'div',
      { class: 'confirm' },
      h('p', { class: 'confirm-text', text }),
      h(
        'div',
        { class: 'confirm-actions' },
        h('button', { type: 'button', class: 'pill', onclick: () => s.close() }, cancelLabel),
        h(
          'button',
          {
            type: 'button',
            class: 'pill ' + (danger ? 'pill-danger' : 'pill-primary'),
            onclick: () => {
              answer = true;
              s.close();
            },
          },
          confirmLabel,
        ),
      ),
    );
    const s = sheet(body, { cls: 'sheet-confirm' });
    s.closed.then(() => resolve(answer));
  });
}

export const reducedMotion = () => window.matchMedia('(prefers-reduced-motion: reduce)').matches;

// ---------- global wiring ----------
document.addEventListener('keydown', (e) => {
  if (e.key === 'Escape' && openLayers.length) {
    e.preventDefault();
    e.stopImmediatePropagation();
    openLayers[openLayers.length - 1].close();
  }
});

store.on('favorites', () => document.querySelectorAll('[data-heart]').forEach(syncHeart));
store.on('state', () => {
  document.querySelectorAll('[data-playpause]').forEach(syncPlayPause);
  document.querySelectorAll('.song-row[data-vid]').forEach(syncRow);
});
