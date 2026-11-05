// Themes: the active one lives in <html data-theme>, saved per device in localStorage.
// To add a theme: one entry in THEMES, one [data-theme="<id>"] block in css/themes.css, and the
// same id/bar/scheme in the inline script of index.html.

import { menu } from './ui.js';

// `dot`: the menu swatch (theme background + accent); `bar`: phone status bar color.
export const THEMES = [
  { id: 'crepuscule', name: 'Crépuscule', dot: 'linear-gradient(135deg, #12161d 50%, #f2875a 50%)', bar: '#12161d', scheme: 'dark' },
  { id: 'matin', name: 'Matin', dot: 'linear-gradient(135deg, #f7f2ea 50%, #e0784b 50%)', bar: '#f7f2ea', scheme: 'light' },
  { id: 'ciel', name: 'Ciel', dot: 'linear-gradient(135deg, #cfeaff 50%, #f2875a 50%)', bar: '#cfeaff', scheme: 'light' },
];

const KEY = 'pinson.theme';
const DEFAULT = THEMES[0];
const find = (id) => THEMES.find((t) => t.id === id);

/** The active theme's id. */
export function currentTheme() {
  return (find(document.documentElement.dataset.theme) || DEFAULT).id;
}

/** Applies theme `id` (unknown ids fall back to the default) and remembers it on this device. */
export function setTheme(id) {
  const t = find(id) || DEFAULT;
  document.documentElement.dataset.theme = t.id;
  try {
    localStorage.setItem(KEY, t.id);
  } catch {
    // Storage unavailable: the theme still applies for this session.
  }
  setMeta('theme-color', t.bar);
  setMeta('color-scheme', t.scheme);
}

function setMeta(name, content) {
  let m = document.querySelector(`meta[name="${name}"]`);
  if (!m) {
    m = document.createElement('meta');
    m.name = name;
    document.head.append(m);
  }
  m.content = content;
}

/** A menu of the themes next to `anchor`; the current one is checked. */
export function openThemeMenu(anchor) {
  const cur = currentTheme();
  menu(
    anchor,
    THEMES.map((t) => ({ label: t.name, swatch: t.dot, checked: t.id === cur, onSelect: () => setTheme(t.id) })),
  );
}
