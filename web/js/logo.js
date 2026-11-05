// The Pinson logo: a chaffinch on a sky tile. It hops while music plays (see .pinson in app.css).

import { store } from './store.js';

let uid = 0;

/** A fresh <svg> of the logo, `size` px square. Decorative: the caller labels the link around it. */
export function pinsonMark(size = 32) {
  const id = `pn${++uid}`;
  const tpl = document.createElement('template');
  tpl.innerHTML = `<svg class="pinson" width="${size}" height="${size}" viewBox="0 0 200 200" aria-hidden="true" focusable="false">
    <defs>
      <clipPath id="${id}t"><rect width="200" height="200" rx="46"/></clipPath>
      <clipPath id="${id}b"><circle cx="100" cy="112" r="46"/></clipPath>
      <linearGradient id="${id}s" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="#7cc8ff"/><stop offset="1" stop-color="#d4efff"/></linearGradient>
    </defs>
    <g clip-path="url(#${id}t)">
      <rect width="200" height="200" fill="url(#${id}s)"/>
      <g fill="#fff" opacity=".6"><circle cx="40" cy="44" r="14"/><circle cx="56" cy="40" r="18"/><circle cx="72" cy="46" r="12"/></g>
      <ellipse class="p-shadow" cx="100" cy="172" rx="36" ry="6" fill="#1d4a6b" opacity=".25"/>
      <g class="p-bird">
        <path d="M60 116 L24 100 L28 120 L62 134 Z" fill="#3b3a44"/>
        <circle cx="100" cy="112" r="46" fill="#f2875a"/>
        <g clip-path="url(#${id}b)">
          <ellipse cx="86" cy="64" rx="56" ry="34" fill="#6f8fb8"/>
          <ellipse cx="100" cy="150" rx="40" ry="16" fill="#f6b08a"/>
        </g>
        <ellipse cx="80" cy="118" rx="28" ry="18" transform="rotate(-18 80 118)" fill="#4c3b33"/>
        <path d="M60 116 Q 80 104 100 110" stroke="#fff" stroke-width="5" fill="none" stroke-linecap="round"/>
        <ellipse cx="126" cy="106" rx="9" ry="5" fill="#ff9e9e" opacity=".55"/>
        <g class="p-eye"><circle cx="120" cy="92" r="6.5" fill="#1b1b1f"/><circle cx="122.2" cy="89.8" r="2" fill="#fff"/></g>
        <polygon points="142,96 160,103 142,110" fill="#8d93a3"/>
        <path d="M90 156 L87 168 M110 156 L113 168" stroke="#e0784b" stroke-width="4" stroke-linecap="round"/>
      </g>
      <text class="p-note" x="146" y="70" font-family="Georgia, serif" font-size="30" fill="#1d4a6b">♪</text>
    </g>
  </svg>`;
  return tpl.content.firstElementChild;
}

// One root class drives every mark on the page, including ones views create later.
const sync = () => document.documentElement.classList.toggle('is-playing', store.state.status === 'playing');
store.on('state', sync);
sync();
