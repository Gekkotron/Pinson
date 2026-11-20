// Your stats (`#/stats`): top songs, top artists, time listened and when, for a rolling period.
// The server does the counting; this page paints one answer and ignores stale ones.

import { queuePlay, stats } from '../api.js';
import { artistNameHref } from '../artist-page.js';
import { icon } from '../icons.js';
import { barHeights, barLabel, DEFAULT_PERIOD, emptyTitle, formatListened, isPeriod, PERIODS } from '../stats-format.js';
import { cover, emptyState, h, plural, songMenu, songRow, toast } from '../ui.js';

const STORAGE_KEY = 'pinson.statsPeriod';

function savedPeriod() {
  try {
    const key = localStorage.getItem(STORAGE_KEY);
    return isPeriod(key) ? key : DEFAULT_PERIOD;
  } catch {
    return DEFAULT_PERIOD;
  }
}

function savePeriod(key) {
  try {
    localStorage.setItem(STORAGE_KEY, key);
  } catch {
    // storage blocked: the choice just isn't remembered
  }
}

const tile = (value, label) =>
  h('div', { class: 'stat-tile' }, h('div', { class: 'stat-value', text: value }), h('div', { class: 'stat-label', text: label }));

function hoursChart(byHour) {
  const heights = barHeights(byHour);
  const bars = heights.map((pct, hour) =>
    h(
      'div',
      { class: 'hour-bar', role: 'img', 'aria-label': barLabel(hour, byHour[hour] || 0) },
      h('i', { style: { height: pct + '%' } }),
    ),
  );
  const axis = h(
    'div',
    { class: 'hour-axis', 'aria-hidden': 'true' },
    ...[0, 6, 12, 18].map((n) => h('span', { text: String(n), style: { left: `${(n / 24) * 100}%` } })),
  );
  return h('div', { class: 'hour-chart' }, h('div', { class: 'hour-bars' }, bars), axis);
}

function artistRow(a) {
  return h(
    'a',
    { class: 'stat-artist', href: artistNameHref(a.name) },
    cover(a.artworkUrl, { size: 48, cls: 'cover-round' }),
    h('div', { class: 'song-text' }, h('div', { class: 'song-title', text: a.name })),
    h('span', { class: 'song-when', text: plural(a.plays, 'play') }),
  );
}

export function renderStats(el) {
  let period = savedPeriod();
  let seq = 0; // a newer pick makes older answers stale
  let abort = null;

  const pills = PERIODS.map((p) =>
    h('button', { type: 'button', class: 'pill stats-pill', dataset: { period: p.key }, onclick: () => pick(p.key) }, p.label),
  );
  const body = h('div', { class: 'stats-body', 'aria-live': 'polite' });
  el.append(
    h(
      'div',
      { class: 'page page-list page-stats' },
      h('h1', { class: 'page-title', text: 'Your stats' }),
      h('div', { class: 'stats-pills', role: 'group', 'aria-label': 'Period' }, pills),
      body,
    ),
  );

  function markPills() {
    for (const b of pills) b.setAttribute('aria-pressed', String(b.dataset.period === period));
  }

  function paint(data) {
    if (!data.plays) {
      body.replaceChildren(emptyState('chart', emptyTitle(period), 'Songs you play show up here.'));
      return;
    }
    const songs = data.topSongs.map((t) => t.song);
    body.replaceChildren(
      h(
        'div',
        { class: 'stat-tiles' },
        tile(String(data.plays), data.plays === 1 ? 'play' : 'plays'),
        tile(formatListened(data.listenedSecs), 'listened'),
        tile(String(data.songs), data.songs === 1 ? 'song' : 'songs'),
        tile(String(data.artists), data.artists === 1 ? 'artist' : 'artists'),
      ),
      data.topSongs.length > 0 &&
        h(
          'section',
          { class: 'stats-section' },
          h('h2', { class: 'history-day', text: 'Top songs' }),
          h(
            'div',
            { class: 'song-list' },
            data.topSongs.map((t, i) =>
              h(
                'div',
                { class: 'stat-rank-row' },
                h('span', { class: 'stat-rank', text: String(i + 1) }),
                songRow(t.song, {
                  aside: plural(t.plays, 'play'),
                  menu: () => songMenu(t.song),
                  onPlay: () => queuePlay(songs, { startIndex: i }).catch((err) => toast(err.message)),
                }),
              ),
            ),
          ),
        ),
      data.topArtists.length > 0 &&
        h(
          'section',
          { class: 'stats-section' },
          h('h2', { class: 'history-day', text: 'Top artists' }),
          h('div', { class: 'song-list' }, data.topArtists.map(artistRow)),
        ),
      h('section', { class: 'stats-section' }, h('h2', { class: 'history-day', text: 'When you listen' }), hoursChart(data.byHour)),
    );
  }

  function load() {
    const mine = ++seq;
    if (abort) abort.abort();
    abort = new AbortController();
    markPills();
    body.replaceChildren(h('div', { class: 'search-status', role: 'status', text: 'Loading…' }));
    const tz = Intl.DateTimeFormat().resolvedOptions().timeZone;
    stats({ period, tz }, abort.signal).then(
      (data) => {
        if (mine === seq) paint(data);
      },
      (err) => {
        if (mine !== seq || err.name === 'AbortError') return;
        toast(err.message);
        body.replaceChildren(emptyState('chart', 'Could not load your stats', err.message));
      },
    );
  }

  function pick(key) {
    if (key === period) return;
    period = key;
    savePeriod(key);
    load();
  }

  load();
  return () => {
    seq++;
    if (abort) abort.abort();
  };
}
