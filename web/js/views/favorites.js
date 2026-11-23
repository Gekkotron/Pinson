// Favorites (`#/favorites`): header with Play / Shuffle / Add to queue, then the songs.

import { favoritesReorder, favoritesShuffle, offlineSet, offlineStatus, queueAdd, queuePlay } from '../api.js';
import { lingeringList } from '../linger.js';
import { formatBytes, offlineSentence, offlineWorking } from '../offline-format.js';
import { enableReorder, orderIds } from '../reorder.js';
import { store } from '../store.js';
import { confirmInPage, emptyState, fmtTotal, h, listHeader, pillButton, plural, songMenu, songRow, toast } from '../ui.js';
import { favoritesArt, playFavorites } from './home.js';

function shuffle() {
  favoritesShuffle()
    .then((r) => toast(`Shuffling ${plural(r.count, 'favorite')}`))
    .catch((err) => toast(err.message));
}

function addToQueue() {
  const songs = store.favorites;
  queueAdd(songs, 'end')
    .then((r) => toast(`Added ${plural(r.count, 'song')} to the queue`))
    .catch((err) => toast(err.message));
}

const OFFLINE_POLL_MS = 3000;

/**
 * The "Keep on the server" row. `row` is one persistent element, updated in place (never rebuilt with the
 * song list). `row.stop()` ends polling. Hidden until the first status arrives, and for good if that fails.
 */
function offlineRow() {
  let status = null;
  let timer = null;
  let stopped = false;
  const ctrl = new AbortController();
  const sw = h('button', { type: 'button', class: 'switch', role: 'switch', 'aria-checked': 'false', 'aria-label': 'Keep on the server' });
  const sentence = h('p', { class: 'offline-text' });
  const fill = h('div', { class: 'offline-bar-fill' });
  const bar = h('div', { class: 'offline-bar', role: 'progressbar', 'aria-label': 'Saved favorites' }, fill);
  const now = h('p', { class: 'offline-now' });
  const row = h(
    'div',
    { class: 'offline-row' },
    h('div', { class: 'offline-head' }, h('span', { class: 'offline-label', text: 'Keep on the server' }), sw),
    sentence,
    bar,
    now,
  );
  row.hidden = true;

  const render = () => {
    row.hidden = !status;
    if (!status) return;
    const on = !!status.enabled;
    sw.setAttribute('aria-checked', String(on));
    sentence.textContent = offlineSentence(status);
    const working = on && !!status.downloading;
    bar.hidden = !working;
    now.hidden = !working;
    if (working) {
      const pct = status.total ? Math.min(100, Math.round((status.saved / status.total) * 100)) : 0;
      fill.style.width = pct + '%';
      bar.setAttribute('aria-valuenow', String(pct));
      now.textContent = `Now: ${status.downloading.title || status.downloading.videoId}`;
    }
  };
  const schedule = () => {
    clearTimeout(timer);
    if (!stopped && offlineWorking(status)) timer = setTimeout(poll, OFFLINE_POLL_MS);
  };
  const poll = () =>
    offlineStatus(ctrl.signal)
      .then((st) => {
        if (stopped) return;
        status = st;
        render();
        schedule();
      })
      .catch(() => {
        // an older server has no route: stay hidden; a later hiccup keeps the last status and retries
        if (!stopped && status) schedule();
      });
  sw.onclick = async () => {
    if (!status) return;
    const enable = !status.enabled;
    if (!enable) {
      const n = status.saved;
      const ok = await confirmInPage(`Remove ${n} saved ${n === 1 ? 'song' : 'songs'} (${formatBytes(status.bytes)}) from the server?`, {
        confirmLabel: 'Remove',
      });
      if (!ok) return;
    }
    sw.disabled = true;
    try {
      status = await offlineSet(enable);
      render();
      schedule();
    } catch (err) {
      toast(err.message);
    } finally {
      sw.disabled = false;
    }
  };
  row.stop = () => {
    stopped = true;
    clearTimeout(timer);
    ctrl.abort();
  };
  poll();
  return row;
}

/** How long an un-hearted song stays on the page, greyed. */
const LINGER_MS = 4000;

export function renderFavorites(el) {
  const page = h('div', { class: 'page page-list' });
  el.append(page);
  const offline = offlineRow();
  const paint = () => {
    const songs = store.favorites;
    const meta = [plural(songs.length, 'song'), fmtTotal(songs)].filter(Boolean).join(' · ');
    const header = listHeader({
      art: favoritesArt(),
      kicker: 'Auto playlist',
      title: 'Favorites',
      meta,
      actions: songs.length
        ? [
            pillButton('play', 'Play', playFavorites, { primary: true }),
            pillButton('shuffle', 'Shuffle', shuffle),
            pillButton('addQueue', 'Add to queue', addToQueue),
          ]
        : [],
    });
    const index = new Map(songs.map((s, i) => [s.videoId, i]));
    const body = list.shown.length
      ? h(
          'div',
          { class: 'song-list' },
          list.shown.map((song) => {
            const i = index.get(song.videoId);
            const play = () => queuePlay(songs, { startIndex: i }).catch((err) => toast(err.message));
            const leaving = list.isLeaving(song.videoId);
            return songRow(song, {
              draggable: true, // hidden on a lingering row (CSS), keeping the columns lined up
              onPlay: i == null ? null : play,
              menu: () => songMenu(song),
              cls: leaving ? 'leaving' : null,
            });
          }),
        )
      : emptyState('heart', 'No favorites yet', 'Tap the heart on any song to keep it here.');
    if (list.shown.length) enableReorder(body, { onState, onDrop });
    page.replaceChildren(header, ...(songs.length ? [offline] : []), body);
  };
  // A list change mid-drag would repaint the rows under the finger: hold it until the drop.
  let dragging = false;
  let pending = false;
  const onState = (on) => {
    dragging = on;
    if (!on && pending) {
      pending = false;
      list.set(store.favorites);
    }
  };
  const onDrop = (ids) => favoritesReorder(orderIds(ids, store.favorites)).catch((err) => toast(err.message));
  // an un-hearted song stays a moment, greyed, so a mis-tap can be undone
  const list = lingeringList(store.favorites, LINGER_MS, () => (dragging ? (pending = true) : paint()));
  paint();
  const off = store.on('favorites', () => {
    if (dragging) pending = true;
    else list.set(store.favorites);
  });
  return () => {
    off();
    list.stop();
    offline.stop();
  };
}
