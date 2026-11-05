// Lyrics pane for Now playing: fetch (and prefetch) per song, then render synced, plain, instrumental,
// "no lyrics" or error states. The synced highlight follows `store.positionMs()` from a rAF loop that
// only runs while the pane is actually visible.

import * as api from '../api.js';
import { icon } from '../icons.js';
import { store } from '../store.js';
import { h, reducedMotion } from '../ui.js';

const FOLLOW_PAUSE_MS = 4000; // auto-scroll rests this long after the user scrolls or touches
const ANCHOR = 1 / 3; // the current line sits about a third from the top

// The last fetch, kept at module level so remounting Now playing for the same song doesn't refetch.
// status: "loading" | "ok" | "none" | "error"
let entry = null;
const listeners = new Set();

function startFetch(song) {
  const e = { videoId: song.videoId, status: 'loading', data: null, message: '' };
  entry = e;
  api
    .lyrics(song)
    .then((data) => {
      if (entry !== e || (data && data.videoId && data.videoId !== e.videoId)) return; // stale: drop
      e.data = data;
      e.status = 'ok';
    })
    .catch((err) => {
      if (entry !== e) return;
      if (err && err.status === 404) e.status = 'none';
      else {
        e.status = 'error';
        e.message = (err && err.message) || 'Could not load lyrics';
      }
    })
    .finally(() => {
      if (entry === e) for (const fn of [...listeners]) fn();
    });
  return e;
}

/** Index of the last line with timeMs <= pos whose text is not empty (gaps keep the previous line), or -1. */
function currentIndex(lines, pos) {
  let lo = 0;
  let hi = lines.length - 1;
  let idx = -1;
  while (lo <= hi) {
    const mid = (lo + hi) >> 1;
    if (lines[mid].timeMs <= pos) {
      idx = mid;
      lo = mid + 1;
    } else hi = mid - 1;
  }
  while (idx >= 0 && !lines[idx].text.trim()) idx--;
  return idx;
}

/**
 * Mounts the lyrics view into `pane`. `root` is the Now playing section (its data-tab / data-mode say
 * which pane is shown). Returns a cleanup function.
 */
export function mountLyrics(pane, root) {
  let alive = true;
  let rendered = null; // the entry currently painted
  let shownStatus = '';
  let lineEls = [];
  let lines = [];
  let applied = -1; // line index currently highlighted
  let rafId = 0;
  let lastUser = -Infinity;
  let needScroll = false;
  let instantScroll = false;
  let wasVisible = false;

  const isVisible = () =>
    alive &&
    root.isConnected &&
    document.visibilityState === 'visible' &&
    pane.getClientRects().length > 0 &&
    (root.dataset.tab === 'lyrics' || root.dataset.mode === 'lyrics');
  // The tab (laptop) and mode (phone) are separate settings; CSS decides which one applies by width,
  // and a hidden pane has no client rects, so the rects check above is the real gate.

  const message = (text, ...extra) =>
    h('div', { class: 'lyrics-msg' }, icon('lyrics', { size: 32 }), h('p', { text }), ...extra);

  const paint = (e) => {
    rendered = e;
    shownStatus = e ? e.status : '';
    lineEls = [];
    lines = [];
    applied = -1;
    needScroll = false;
    pane.scrollTop = 0;
    if (!e) return pane.replaceChildren();
    if (e.status === 'loading') {
      return pane.replaceChildren(h('div', { class: 'lyrics-loading', 'aria-label': 'Loading lyrics', role: 'status' }, h('i'), h('i'), h('i'), h('i')));
    }
    if (e.status === 'none') return pane.replaceChildren(message('No lyrics'));
    if (e.status === 'error') {
      const retry = h(
        'button',
        {
          type: 'button',
          class: 'pill',
          onclick: () => {
            const song = store.state.current && store.state.current.song;
            if (!song || song.videoId !== e.videoId) return;
            startFetch(song);
            sync();
          },
        },
        'Retry',
      );
      return pane.replaceChildren(message(e.message, retry));
    }
    const d = e.data;
    if (d.instrumental) return pane.replaceChildren(message('Instrumental'));
    if (d.synced && d.lines && d.lines.length) {
      lines = d.lines;
      const box = h('div', { class: 'lyrics-synced' });
      lineEls = lines.map((l) => h('p', { class: l.text.trim() ? 'lyric future' : 'lyric gap', 'aria-hidden': l.text.trim() ? null : 'true' }, l.text.trim() ? l.text : ''));
      box.append(...lineEls, h('div', { class: 'lyrics-tail', 'aria-hidden': 'true' }));
      pane.replaceChildren(box);
      instantScroll = true;
      return;
    }
    if (d.plain && d.plain.trim()) return pane.replaceChildren(h('div', { class: 'lyrics-plain', text: d.plain.trim() }));
    pane.replaceChildren(message('No lyrics'));
  };

  const sync = () => {
    if (!alive) return;
    const cur = store.state.current && store.state.current.song;
    if (!cur) {
      if (rendered) paint(null);
      return kick();
    }
    if (!entry || entry.videoId !== cur.videoId) startFetch(cur); // fetch even while the tab is closed (prefetch)
    if (rendered !== entry || shownStatus !== entry.status) paint(entry);
    kick();
  };

  const scrollToCurrent = (smooth) => {
    const el = lineEls[applied];
    if (!el) return;
    const top = pane.scrollTop + (el.getBoundingClientRect().top - pane.getBoundingClientRect().top) - pane.clientHeight * ANCHOR;
    pane.scrollTo({ top: Math.max(0, top), behavior: smooth && !reducedMotion() ? 'smooth' : 'auto' });
  };

  const setCurrent = (idx) => {
    for (let i = 0; i < lineEls.length; i++) {
      const cls = i === idx ? 'current' : i < idx ? 'past' : 'future';
      if (lineEls[i].classList.contains('gap')) continue;
      lineEls[i].className = `lyric ${cls}`;
    }
    applied = idx;
    needScroll = true;
  };

  const frame = () => {
    rafId = 0;
    if (!isVisible()) return;
    const s = store.state;
    if (lines.length && s.current && rendered && rendered.videoId === s.current.song.videoId) {
      // No current line until the position is running; frozen (not recomputed) while it isn't.
      if (s.positionRunning) {
        const idx = currentIndex(lines, store.positionMs());
        if (idx !== applied) setCurrent(idx);
      }
      if (needScroll && applied >= 0 && performance.now() - lastUser > FOLLOW_PAUSE_MS) {
        scrollToCurrent(!instantScroll);
        instantScroll = false;
        needScroll = false;
      }
    }
    rafId = requestAnimationFrame(frame);
  };

  function kick() {
    const vis = isVisible();
    if (vis && !wasVisible) {
      // Just became visible: jump to the current line without animating.
      instantScroll = true;
      needScroll = applied >= 0;
      lastUser = -Infinity;
    }
    wasVisible = vis;
    if (vis && !rafId) rafId = requestAnimationFrame(frame);
    else if (!vis && rafId) {
      cancelAnimationFrame(rafId);
      rafId = 0;
    }
  }

  const touched = () => {
    lastUser = performance.now();
    needScroll = applied >= 0;
  };
  const userEvents = ['wheel', 'touchstart', 'touchmove', 'pointerdown', 'keydown'];
  for (const ev of userEvents) pane.addEventListener(ev, touched, { passive: true });
  const obs = new MutationObserver(kick);
  obs.observe(root, { attributes: true, attributeFilter: ['data-tab', 'data-mode'] });
  document.addEventListener('visibilitychange', kick);
  window.addEventListener('resize', kick);
  const off = store.on('state', sync);
  listeners.add(sync);
  sync();

  return () => {
    alive = false;
    off();
    listeners.delete(sync);
    obs.disconnect();
    document.removeEventListener('visibilitychange', kick);
    window.removeEventListener('resize', kick);
    for (const ev of userEvents) pane.removeEventListener(ev, touched);
    if (rafId) cancelAnimationFrame(rafId);
    rafId = 0;
  };
}
