// Hidden (`#/hidden`): the songs and artists "Don't play this again" keeps out of autoplay
// and mixes. Unhiding removes the row in place.

import { hiddenList, queuePlay, unhideArtist, unhideSong } from '../api.js';
import { icon } from '../icons.js';
import { emptyState, h, songRow, toast } from '../ui.js';

const EMPTY_TEXT = 'Nothing hidden. Use ⋯ → Don\'t play this again on any song.';

export function renderHidden(el) {
  let songs = [];
  let artists = [];
  let loaded = false;
  let abort = new AbortController();

  const body = h('div', { class: 'hidden-body', 'aria-live': 'polite' });
  el.append(
    h(
      'div',
      { class: 'page page-list page-hidden' },
      h('h1', { class: 'page-title', text: 'Hidden' }),
      h('p', { class: 'hidden-note', text: 'Autoplay and mixes skip these. Songs you pick yourself still play.' }),
      body,
    ),
  );

  const unhideButton = (label, onClick) =>
    h('button', { type: 'button', class: 'pill hidden-unhide', 'aria-label': label, onclick: onClick }, 'Unhide');

  function removeSong(song, btn) {
    btn.disabled = true;
    unhideSong(song.videoId).then(
      () => {
        songs = songs.filter((s) => s.videoId !== song.videoId);
        paint();
        toast(`Unhid ${song.title}`);
      },
      (err) => {
        btn.disabled = false;
        toast(err.message);
      },
    );
  }

  function removeArtist(a, btn) {
    btn.disabled = true;
    unhideArtist(a.name).then(
      () => {
        artists = artists.filter((x) => x.name !== a.name);
        paint();
        toast(`Unhid ${a.name}`);
      },
      (err) => {
        btn.disabled = false;
        toast(err.message);
      },
    );
  }

  function songItem(song) {
    const btn = unhideButton(`Unhide ${song.title}`, () => removeSong(song, btn));
    return h(
      'div',
      { class: 'hidden-row' },
      songRow(song, { onPlay: () => queuePlay([song]).catch((err) => toast(err.message)) }),
      btn,
    );
  }

  function artistItem(a) {
    const btn = unhideButton(`Unhide ${a.name}`, () => removeArtist(a, btn));
    return h(
      'div',
      { class: 'hidden-row hidden-artist' },
      h('div', { class: 'hidden-artist-icon', 'aria-hidden': 'true' }, icon('person', { size: 24 })),
      h('div', { class: 'song-text' }, h('div', { class: 'song-title', text: a.name })),
      btn,
    );
  }

  function paint() {
    if (!loaded) return;
    if (!songs.length && !artists.length) {
      body.replaceChildren(emptyState('block', 'Nothing hidden', EMPTY_TEXT));
      return;
    }
    body.replaceChildren(
      songs.length > 0 &&
        h(
          'section',
          { class: 'stats-section' },
          h('h2', { class: 'history-day', text: 'Songs' }),
          h('div', { class: 'song-list' }, songs.map(songItem)),
        ),
      artists.length > 0 &&
        h(
          'section',
          { class: 'stats-section' },
          h('h2', { class: 'history-day', text: 'Artists' }),
          h('div', { class: 'song-list' }, artists.map(artistItem)),
        ),
    );
  }

  hiddenList(abort.signal).then(
    (data) => {
      // The server may name the cover `thumbnail`; songRow reads `artworkUrl`.
      songs = ((data && data.songs) || []).map((s) => (s.artworkUrl ? s : { ...s, artworkUrl: s.thumbnail }));
      artists = (data && data.artists) || [];
      loaded = true;
      paint();
    },
    (err) => {
      if (err.name === 'AbortError') return;
      body.replaceChildren(emptyState('block', "Couldn't load", err.message));
    },
  );

  return () => abort.abort();
}
