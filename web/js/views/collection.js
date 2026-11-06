// Album, YouTube playlist and mix pages (`#/album/:id`, `#/ytplaylist/:id`, `#/mix/:id`): header with Play,
// Shuffle, Add to queue and Save to my playlists, then the tracks.

import { album, mix, playlists, queueAdd, queuePlay, ytplaylist } from '../api.js';
import { go } from '../router.js';
import { cover, emptyState, fmtTotal, h, listHeader, pillButton, plural, songMenu, songRow, toast } from '../ui.js';

export function renderCollection(el, { pattern, params }) {
  const [load, kicker] = { '/album/:id': [album, 'Album'], '/mix/:id': [mix, 'Mix'] }[pattern] || [ytplaylist, 'Playlist'];
  const page = h('div', { class: 'page page-list' }, h('div', { class: 'search-status', role: 'status', text: 'Loading…' }));
  el.append(page);
  let alive = true;

  load(params.id).then(
    (c) => alive && paint(c),
    (err) => alive && page.replaceChildren(emptyState('music', "Couldn't open this", err.message)),
  );

  function paint(c) {
    const songs = c.songs;
    const fail = (err) => toast(err.message);
    const meta = [plural(songs.length, 'song'), fmtTotal(songs)].filter(Boolean).join(' · ');
    const actions = songs.length
      ? [
          pillButton('play', 'Play', () => queuePlay(songs).catch(fail), { primary: true }),
          pillButton('shuffle', 'Shuffle', () =>
            queuePlay(songs, { shuffle: true }).then(() => toast(`Shuffling ${plural(songs.length, 'song')}`), fail),
          ),
          pillButton('addQueue', 'Add to queue', () =>
            queueAdd(songs, 'end').then((r) => toast(`Added ${plural(r.count, 'song')} to the queue`), fail),
          ),
          pillButton('plus', 'Save to my playlists', () =>
            playlists.create(Array.from(c.title).slice(0, 100).join('').trim() || 'Saved playlist', songs).then((pl) => {
              toast(`Saved “${pl.name}”`);
              go('#/playlist/' + encodeURIComponent(pl.id));
            }, fail),
          ),
        ]
      : [];
    const body = songs.length
      ? h(
          'div',
          { class: 'song-list' },
          songs.map((song, i) =>
            songRow(song, { onPlay: () => queuePlay(songs, { startIndex: i }).catch(fail), menu: () => songMenu(song) }),
          ),
        )
      : emptyState('music', 'Nothing to play', 'YouTube Music has no playable songs here.');
    page.replaceChildren(
      listHeader({
        art: cover(c.artworkUrl, { eager: true }),
        kicker,
        title: c.title,
        meta: [c.subtitle, meta].filter(Boolean).join(' · '),
        actions,
      }),
      body,
    );
  }
  return () => {
    alive = false;
  };
}
