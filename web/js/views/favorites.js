// Favorites (`#/favorites`): header with Play / Shuffle / Add to queue, then the songs.

import { favoritesReorder, favoritesShuffle, queueAdd, queuePlay } from '../api.js';
import { lingeringList } from '../linger.js';
import { enableReorder, orderIds } from '../reorder.js';
import { store } from '../store.js';
import { emptyState, fmtTotal, h, listHeader, pillButton, plural, songMenu, songRow, toast } from '../ui.js';
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

/** How long an un-hearted song stays on the page, greyed. */
const LINGER_MS = 4000;

export function renderFavorites(el) {
  const page = h('div', { class: 'page page-list' });
  el.append(page);
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
    page.replaceChildren(header, body);
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
  };
}
