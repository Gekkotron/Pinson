// Hash router: `#/home`, `#/search?q=…`, `#/favorites`, `#/playlist/:id`, `#/album/:id`,
// `#/ytplaylist/:id`, `#/mix/:id`, `#/now`. Back/forward and reload keep the page.

const routes = []; // {pattern, keys, re, render}
const changeListeners = new Set();
let outlet = null;
let cleanup = null;
// Each history entry we create carries {depth}: how many in-app pages lie behind it, so back()
// knows whether the browser's Back stays inside the app.
let depth = 0;

export const DEFAULT = '#/home';

/**
 * Registers `render(el, {params, query, path})` for `pattern` (e.g. "/playlist/:id").
 * `render` may return a cleanup function, called before the next page renders.
 * Registering an existing pattern replaces it.
 */
export function route(pattern, render) {
  const keys = [];
  const re = new RegExp(
    '^' + pattern.replace(/:(\w+)/g, (_, k) => (keys.push(k), '([^/]+)')) + '$',
  );
  const existing = routes.find((r) => r.pattern === pattern);
  if (existing) existing.render = render;
  else routes.push({ pattern, keys, re, render });
}

/** Navigates to `hash` ("#/now", "#/search?q=abba"). `replace` swaps the history entry. */
export function go(hash, { replace = false } = {}) {
  if (!hash.startsWith('#')) hash = '#' + hash;
  if (hash === location.hash) return;
  if (replace) history.replaceState({ depth }, '', hash);
  else history.pushState({ depth: depth + 1 }, '', hash);
  render();
}

/** Browser Back when the previous page is ours, else Home. */
export function back() {
  if (depth > 0) {
    history.back();
  } else {
    go(DEFAULT, { replace: true });
  }
}

/** The page currently shown: {path, params, query, pattern}, or null before a route matches. */
export function current() {
  return parse(location.hash);
}

/** Called with `current()` after every page change. Returns an unsubscribe function. */
export function onChange(fn) {
  changeListeners.add(fn);
  return () => changeListeners.delete(fn);
}

/** Starts routing into `el`. Unknown or empty hashes go to `#/home`. */
export function start(el) {
  outlet = el;
  window.addEventListener('hashchange', () => {
    // A plain link or a typed URL: number the new entry after the page it came from.
    if (!history.state || typeof history.state.depth !== 'number') {
      history.replaceState({ depth: depth + 1 }, '');
    }
    render();
  });
  render();
}

function parse(hash) {
  const raw = (hash || '').replace(/^#/, '');
  const [path, qs = ''] = raw.split('?');
  const query = Object.fromEntries(new URLSearchParams(qs));
  for (const r of routes) {
    const m = path.match(r.re);
    if (!m) continue;
    const params = {};
    r.keys.forEach((k, i) => {
      params[k] = decodeURIComponent(m[i + 1]);
    });
    return { path, params, query, pattern: r.pattern, render: r.render };
  }
  return null;
}

function render() {
  if (!outlet) return;
  depth = (history.state && history.state.depth) || 0;
  let page = parse(location.hash);
  if (!page) {
    history.replaceState(history.state, '', DEFAULT);
    page = parse(DEFAULT);
    if (!page) return;
  }
  if (typeof cleanup === 'function') {
    try {
      cleanup();
    } catch (err) {
      console.error('page cleanup failed', err);
    }
  }
  cleanup = null;
  outlet.replaceChildren();
  outlet.scrollTop = 0;
  if (page.render) {
    try {
      cleanup = page.render(outlet, page);
    } catch (err) {
      console.error('page render failed', page.path, err);
    }
  }
  for (const fn of changeListeners) fn(page);
}
