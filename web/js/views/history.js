// Recently played (`#/history?q=…`): every play, grouped by local day, a song once per day.
// The search box searches the whole history on the server; more loads as you scroll.

import { historyPage, queuePlay } from '../api.js';
import { groupByDay, playTime } from '../history-days.js';
import { icon } from '../icons.js';
import { emptyState, h, songMenu, songRow, toast } from '../ui.js';

const PAGE = 50; // matches the server's default page size
const DEBOUNCE_MS = 300;

export function renderHistory(el, { query }) {
  let q = (query.q || '').trim();
  let plays = []; // raw plays from every page loaded, newest first
  let done = false;
  let loading = false;
  let seq = 0; // a new search makes older answers stale
  let abort = null;
  let timer = 0;

  const input = h('input', {
    class: 'search-input',
    type: 'search',
    placeholder: 'Search your history',
    'aria-label': 'Search your history',
    autocomplete: 'off',
    autocapitalize: 'off',
    spellcheck: 'false',
    enterkeyhint: 'search',
    value: q,
  });
  const box = h('label', { class: 'search-box history-search' }, icon('search', { size: 22 }), input);
  const list = h('div', { class: 'history-list' });
  const status = h('div', { class: 'search-status', role: 'status', hidden: true });
  const sentinel = h('div', { class: 'history-sentinel', 'aria-hidden': 'true' });
  el.append(h('div', { class: 'page page-list page-history' }, h('h1', { class: 'page-title', text: 'Recently played' }), box, list, status, sentinel));

  function paint() {
    const days = groupByDay(plays);
    const shown = days.flatMap((d) => d.plays.map((p) => p.song));
    if (!shown.length) {
      list.replaceChildren(
        done
          ? q
            ? emptyState('search', 'No matches', `Nothing in your history matches “${q}”.`)
            : emptyState('history', 'Nothing played yet', 'Songs you play show up here.')
          : '',
      );
      return;
    }
    let i = 0;
    list.replaceChildren(
      ...days.map((d) =>
        h(
          'section',
          { class: 'history-day-group' },
          h('h2', { class: 'history-day', text: d.label }),
          h(
            'div',
            { class: 'song-list' },
            d.plays.map((p) => {
              const index = i++;
              return songRow(p.song, {
                aside: playTime(new Date(p.playedAt)),
                menu: () => songMenu(p.song),
                onPlay: () => queuePlay(shown, { startIndex: index }).catch((err) => toast(err.message)),
              });
            }),
          ),
        ),
      ),
    );
  }

  function loadMore() {
    if (loading || done) return;
    loading = true;
    const mine = seq;
    abort = new AbortController();
    const before = plays.length ? plays[plays.length - 1].id : undefined;
    status.hidden = false;
    status.textContent = 'Loading…';
    historyPage({ q, before, limit: PAGE }, abort.signal).then(
      (page) => {
        if (mine !== seq) return;
        loading = false;
        status.hidden = true;
        plays = plays.concat(page);
        done = page.length < PAGE;
        paint();
        // A short first page may leave the sentinel on screen: the observer won't fire again.
        if (!done && visible) loadMore();
      },
      (err) => {
        if (mine !== seq) return;
        loading = false;
        status.hidden = true;
        toast(err.message);
        if (!plays.length) list.replaceChildren(emptyState('history', 'Could not load your history', err.message));
        // the next time the sentinel scrolls into view, it retries
      },
    );
  }

  function restart(text) {
    const next = text.trim();
    if (next === q && (plays.length || loading)) return;
    q = next;
    seq++;
    if (abort) abort.abort();
    plays = [];
    done = false;
    loading = false;
    list.replaceChildren();
    // Keep the address in step so a reload or Back lands on the same search (no re-render).
    history.replaceState(history.state, '', q ? '#/history?q=' + encodeURIComponent(q) : '#/history');
    loadMore();
  }

  input.addEventListener('input', () => {
    clearTimeout(timer);
    timer = setTimeout(() => restart(input.value), DEBOUNCE_MS);
  });
  input.addEventListener('keydown', (e) => {
    if (e.key !== 'Enter') return;
    clearTimeout(timer);
    restart(input.value);
    input.blur();
  });

  let visible = false;
  const observer = new IntersectionObserver(
    (entries) => {
      visible = entries.some((e) => e.isIntersecting);
      if (visible) loadMore();
    },
    { rootMargin: '400px 0px' },
  );
  observer.observe(sentinel);
  loadMore();

  return () => {
    seq++;
    observer.disconnect();
    clearTimeout(timer);
    if (abort) abort.abort();
  };
}
