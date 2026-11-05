// Search (`#/search?q=…`): one query box; songs, albums and playlists are fetched in parallel.
// Laptop shows the three sections; on a phone chips switch which one is shown (CSS only).

import { playNext, search } from '../api.js';
import { icon } from '../icons.js';
import { cover, emptyState, h, songMenu, songRow, toast } from '../ui.js';

const KINDS = [
  ['songs', 'Songs'],
  ['albums', 'Albums'],
  ['playlists', 'Playlists'],
];
const DEBOUNCE_MS = 300;

let lastKind = 'songs'; // the phone chip, remembered while the app stays open

/** A search result card for an album or a YouTube playlist. */
function collectionTile(c) {
  const href = (c.kind === 'album' ? '#/album/' : '#/ytplaylist/') + encodeURIComponent(c.id);
  return h(
    'a',
    { class: 'tile', href },
    h('div', { class: 'tile-art' }, cover(c.artworkUrl)),
    h('div', { class: 'tile-title', text: c.title }),
    c.subtitle && h('div', { class: 'tile-sub', text: c.subtitle }),
  );
}

function songResults(songs) {
  return h(
    'div',
    { class: 'song-list' },
    songs.map((song) =>
      songRow(song, {
        onPlay: () => playNext(song).then(() => toast('Playing next'), (err) => toast(err.message)),
        menu: () => songMenu(song),
      }),
    ),
  );
}

const collectionResults = (items) => h('div', { class: 'tile-grid' }, items.map(collectionTile));

export function renderSearch(el, { query }) {
  let kind = lastKind;
  let seq = 0; // a newer search makes older answers stale
  let timer = 0;
  let shown = null; // the query currently on screen

  const input = h('input', {
    class: 'search-input',
    type: 'search',
    placeholder: 'Search songs, albums, playlists',
    'aria-label': 'Search YouTube Music',
    autocomplete: 'off',
    autocapitalize: 'off',
    spellcheck: 'false',
    enterkeyhint: 'search',
    value: query.q || '',
  });
  const box = h('label', { class: 'search-box' }, icon('search', { size: 22 }), input);

  const chips = h(
    'div',
    { class: 'chips', role: 'tablist', 'aria-label': 'Result type' },
    KINDS.map(([k, label]) =>
      h('button', {
        type: 'button',
        class: 'chip',
        role: 'tab',
        text: label,
        dataset: { kind: k },
        onclick: () => setKind(k),
      }),
    ),
  );

  const sections = Object.fromEntries(
    KINDS.map(([k, label]) => [
      k,
      h('section', { class: 'search-section', dataset: { kind: k } }, h('h2', { class: 'section-label', text: label }), h('div', { class: 'search-body' })),
    ]),
  );

  const results = h('div', { class: 'search-results' }, Object.values(sections));
  const page = h('div', { class: 'page page-search' }, box, chips, results);
  el.append(page);

  function setKind(k) {
    kind = lastKind = k;
    page.dataset.kind = k;
    for (const c of chips.children) {
      c.classList.toggle('active', c.dataset.kind === k);
      c.setAttribute('aria-selected', String(c.dataset.kind === k));
    }
  }
  setKind(kind);

  const setBody = (k, node) => sections[k].querySelector('.search-body').replaceChildren(node);
  const intro = emptyState('search', 'Search YouTube Music', 'Find songs, albums and playlists, then play them on the home speakers.');

  function run(q) {
    q = q.trim();
    if (q === shown) return;
    shown = q;
    const mine = ++seq;
    // Keep the address in step so a reload or Back lands on the same search (no re-render).
    history.replaceState(history.state, '', q ? '#/search?q=' + encodeURIComponent(q) : '#/search');
    results.hidden = !q;
    intro.hidden = !!q;
    if (!q) return;
    for (const [k] of KINDS) {
      setBody(k, h('div', { class: 'search-status', text: 'Searching…', role: 'status' }));
      search(q, k).then(
        (items) => {
          if (mine !== seq) return;
          if (!items.length) return setBody(k, h('div', { class: 'search-status', text: `No ${k} found for “${q}”.` }));
          setBody(k, k === 'songs' ? songResults(items) : collectionResults(items));
        },
        (err) => {
          if (mine === seq) setBody(k, h('div', { class: 'search-status search-error', role: 'alert', text: err.message }));
        },
      );
    }
  }

  page.append(intro);
  input.addEventListener('input', () => {
    clearTimeout(timer);
    timer = setTimeout(() => run(input.value), DEBOUNCE_MS);
  });
  input.addEventListener('keydown', (e) => {
    if (e.key !== 'Enter') return;
    clearTimeout(timer);
    run(input.value);
    input.blur();
  });

  results.hidden = true;
  if (query.q && query.q.trim()) run(query.q);
  else if (window.matchMedia('(hover: hover)').matches) input.focus({ preventScroll: true });
  return () => {
    clearTimeout(timer);
    seq++;
  };
}
