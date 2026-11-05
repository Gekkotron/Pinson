// Search (`#/search?q=…`): one query box; songs, albums and playlists are fetched in parallel.
// Laptop shows the three sections; on a phone chips switch which one is shown (CSS only).

import { playNext, search, searchSuggestions } from '../api.js';
import { icon } from '../icons.js';
import { cover, emptyState, h, songMenu, songRow, toast } from '../ui.js';

const KINDS = [
  ['songs', 'Songs'],
  ['albums', 'Albums'],
  ['playlists', 'Playlists'],
];
const DEBOUNCE_MS = 300;
const SUGGEST_MS = 150;

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
  let sugSeq = 0; // a newer suggestion fetch (or a close) makes older answers stale
  let sugTimer = 0;
  let sugAbort = null;
  let items = [];
  let active = -1; // the highlighted suggestion, -1 for none

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
    role: 'combobox',
    'aria-autocomplete': 'list',
    'aria-controls': 'search-suggestions',
    'aria-expanded': 'false',
  });
  const box = h('label', { class: 'search-box' }, icon('search', { size: 22 }), input);
  const list = h('ul', { class: 'suggest-list', id: 'search-suggestions', role: 'listbox', 'aria-label': 'Suggestions', hidden: true });
  const combo = h('div', { class: 'search-combo' }, box, list);

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
  const page = h('div', { class: 'page page-search' }, combo, chips, results);
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
          if (mine !== seq) return;
          shown = null; // so pressing Enter again retries
          setBody(k, h('div', { class: 'search-status search-error', role: 'alert', text: err.message }));
        },
      );
    }
  }

  function setActive(i) {
    active = i;
    for (const [n, row] of [...list.children].entries()) {
      row.classList.toggle('active', n === i);
      row.setAttribute('aria-selected', String(n === i));
    }
    if (i >= 0) {
      input.setAttribute('aria-activedescendant', 'suggest-' + i);
      list.children[i].scrollIntoView({ block: 'nearest' });
    } else input.removeAttribute('aria-activedescendant');
  }

  function closeList() {
    sugSeq++;
    clearTimeout(sugTimer);
    if (sugAbort) sugAbort.abort();
    list.hidden = true;
    input.setAttribute('aria-expanded', 'false');
    setActive(-1);
  }

  function pick(text) {
    input.value = text;
    closeList();
    clearTimeout(timer);
    run(text);
    input.blur();
  }

  // The arrow puts the text in the box for editing: no search, the list refreshes for it.
  function fill(text) {
    input.value = text;
    clearTimeout(timer);
    input.focus();
    fetchSuggestions(text.trim());
  }

  function suggestRow(item, i) {
    return h(
      'li',
      { class: 'suggest-row', role: 'option', id: 'suggest-' + i, 'aria-selected': 'false', onclick: () => pick(item.text) },
      icon('search', { size: 20 }),
      h('span', { class: 'suggest-text' }, item.runs.map((r) => h(r.bold ? 'b' : 'span', { text: r.text }))),
      h(
        'button',
        {
          type: 'button',
          class: 'icon-btn suggest-fill',
          tabindex: '-1',
          'aria-label': `Fill in “${item.text}”`,
          onclick: (e) => {
            e.stopPropagation();
            fill(item.text);
          },
        },
        icon('north-west'),
      ),
    );
  }

  function fetchSuggestions(q) {
    if (sugAbort) sugAbort.abort();
    sugAbort = new AbortController();
    const mine = ++sugSeq;
    searchSuggestions(q, sugAbort.signal).then(
      (found) => {
        if (mine !== sugSeq || input.value.trim() !== q) return;
        if (!found || !found.length) return closeList();
        items = found;
        list.replaceChildren(...items.map(suggestRow));
        setActive(-1);
        list.hidden = false;
        input.setAttribute('aria-expanded', 'true');
      },
      () => {
        if (mine === sugSeq) closeList(); // an abort means a newer fetch or a close took over
      },
    );
  }

  page.append(intro);
  input.addEventListener('input', () => {
    clearTimeout(timer);
    timer = setTimeout(() => run(input.value), DEBOUNCE_MS);
    const q = input.value.trim();
    if (!q) return closeList();
    clearTimeout(sugTimer);
    sugTimer = setTimeout(() => fetchSuggestions(q), SUGGEST_MS);
  });
  input.addEventListener('keydown', (e) => {
    const open = !list.hidden;
    if (open && (e.key === 'ArrowDown' || e.key === 'ArrowUp')) {
      e.preventDefault();
      const step = e.key === 'ArrowDown' ? 1 : -1;
      setActive(((active + 1 + step + items.length + 1) % (items.length + 1)) - 1);
    } else if (e.key === 'Escape') {
      if (open) e.preventDefault(); // keeps the text; with no list the key behaves as before
      closeList(); // also cancels a suggestion fetch that is still pending
    } else if (e.key === 'Enter') {
      if (open && active >= 0) {
        e.preventDefault();
        return pick(items[active].text);
      }
      closeList();
      clearTimeout(timer);
      run(input.value);
      input.blur();
    }
  });
  list.addEventListener('mousedown', (e) => e.preventDefault()); // keep focus (and the phone keyboard) in the input
  const onOutside = (e) => {
    if (!combo.contains(e.target)) closeList();
  };
  document.addEventListener('pointerdown', onOutside);

  results.hidden = true;
  if (query.q && query.q.trim()) run(query.q);
  else if (window.matchMedia('(hover: hover)').matches) input.focus({ preventScroll: true });
  return () => {
    clearTimeout(timer);
    seq++;
    document.removeEventListener('pointerdown', onOutside);
    closeList();
  };
}
