// Artist page (`#/artist/:id`): round photo header with Play, Shuffle and Add to queue on the top songs, a
// clamped description, Top songs, Albums, Singles & EPs and "Fans also like". `#/artist-name/:name` looks the
// artist up by name and redirects here.

import { artist, artistLookup, queueAdd, queuePlay } from '../api.js';
import { visibleSongs, TOP_SONGS } from '../artist-page.js';
import { go } from '../router.js';
import { cover, emptyState, h, listHeader, pillButton, plural, songMenu, songRow, toast } from '../ui.js';

function albumTile(c) {
  return h(
    'a',
    { class: 'tile', href: '#/album/' + encodeURIComponent(c.id) },
    h('div', { class: 'tile-art' }, cover(c.artworkUrl)),
    h('div', { class: 'tile-title', text: c.title }),
    c.subtitle && h('div', { class: 'tile-sub', text: c.subtitle }),
  );
}

/** A search result card for an artist: round photo and name. */
export function artistTile(a) {
  return h(
    'a',
    { class: 'tile tile-artist', href: '#/artist/' + encodeURIComponent(a.id) },
    h('div', { class: 'tile-art round' }, cover(a.artworkUrl)),
    h('div', { class: 'tile-title', text: a.name }),
    a.subtitle && h('div', { class: 'tile-sub', text: a.subtitle }),
  );
}

const section = (title, body) => h('section', { class: 'artist-section' }, h('h2', { class: 'section-label', text: title }), body);

export function renderArtist(el, { params }) {
  const page = h('div', { class: 'page page-list page-artist' }, h('div', { class: 'search-status', role: 'status', text: 'Loading…' }));
  el.append(page);
  let alive = true;

  artist(params.id).then(
    (a) => alive && paint(a),
    (err) => alive && page.replaceChildren(emptyState('music', "Couldn't open this artist", err.message)),
  );

  function paint(a) {
    const songs = a.songs || [];
    const fail = (err) => toast(err.message);
    // The API gives a bare count ("7.22M"); add the word unless it already carries one.
    const subs = a.subscribers && (/^[\d.,\s]+[KMB]?$/i.test(a.subscribers) ? `${a.subscribers} subscribers` : a.subscribers);
    const meta = [subs, songs.length ? plural(songs.length, 'song') : null].filter(Boolean).join(' · ');
    const actions = songs.length
      ? [
          pillButton('play', 'Play', () => queuePlay(songs).catch(fail), { primary: true }),
          pillButton('shuffle', 'Shuffle', () =>
            queuePlay(songs, { shuffle: true }).then(() => toast(`Shuffling ${plural(songs.length, 'song')}`), fail),
          ),
          pillButton('addQueue', 'Add to queue', () =>
            queueAdd(songs, 'end').then((r) => toast(`Added ${plural(r.count, 'song')} to the queue`), fail),
          ),
        ]
      : [];
    const parts = [
      listHeader({
        art: h('div', { class: 'artist-art' }, cover(a.artworkUrl, { eager: true })),
        kicker: 'Artist',
        title: a.name,
        meta,
        actions,
      }),
    ];

    if (a.description) {
      const text = h('p', { class: 'artist-about clamped', text: a.description, tabindex: '0', role: 'button', 'aria-expanded': 'false' });
      const toggle = () => {
        const open = text.classList.toggle('clamped') === false;
        text.setAttribute('aria-expanded', String(open));
      };
      text.addEventListener('click', toggle);
      text.addEventListener('keydown', (e) => {
        if (e.key === 'Enter' || e.key === ' ') {
          e.preventDefault();
          toggle();
        }
      });
      parts.push(text);
    }

    if (songs.length) {
      const list = h('div', { class: 'song-list' });
      const more = songs.length > TOP_SONGS ? h('button', { type: 'button', class: 'pill artist-show-all', text: 'Show all' }) : null;
      let expanded = false;
      const draw = () => {
        list.replaceChildren(
          ...visibleSongs(songs, expanded).map((song, i) =>
            songRow(song, { onPlay: () => queuePlay(songs, { startIndex: i }).catch(fail), menu: () => songMenu(song) }),
          ),
        );
        if (more) more.hidden = expanded;
      };
      if (more)
        more.addEventListener('click', () => {
          expanded = true;
          draw();
        });
      draw();
      parts.push(section('Top songs', h('div', {}, list, more)));
    }
    if (a.albums && a.albums.length) parts.push(section('Albums', h('div', { class: 'tile-grid' }, a.albums.map(albumTile))));
    if (a.singles && a.singles.length) parts.push(section('Singles & EPs', h('div', { class: 'tile-grid' }, a.singles.map(albumTile))));
    if (a.related && a.related.length) parts.push(section('Fans also like', h('div', { class: 'tile-grid' }, a.related.map(artistTile))));
    if (parts.length === 1 || (!songs.length && parts.length === 2 && a.description))
      parts.push(emptyState('music', 'Nothing to show yet', 'YouTube Music has no songs or albums for this artist.'));
    page.replaceChildren(...parts);
  }
  return () => {
    alive = false;
  };
}

/** `#/artist-name/:name`: finds the artist by name, then swaps this address for the artist page. */
export function renderArtistByName(el, { params }) {
  const page = h('div', { class: 'page' }, h('div', { class: 'search-status', role: 'status', text: 'Loading…' }));
  el.append(page);
  let alive = true;
  artistLookup(params.name).then(
    (r) => alive && go('#/artist/' + encodeURIComponent(r.id), { replace: true }),
    () => alive && page.replaceChildren(emptyState('music', "Couldn't find this artist", params.name)),
  );
  return () => {
    alive = false;
  };
}
