// Favorites (`#/favorites`): header with Play / Shuffle / Add to queue, then the songs.

import { favoritesShuffle, queueAdd, queuePlay } from '../api.js';
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
    const body = songs.length
      ? h(
          'div',
          { class: 'song-list' },
          songs.map((song, i) =>
            songRow(song, {
              onPlay: () => queuePlay(songs, { startIndex: i }).catch((err) => toast(err.message)),
              menu: () => songMenu(song),
            }),
          ),
        )
      : emptyState('heart', 'No favorites yet', 'Tap the heart on any song to keep it here.');
    page.replaceChildren(header, body);
  };
  paint();
  return store.on('favorites', paint);
}
