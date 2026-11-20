// The Pinson logo: a chaffinch singing inside a glowing ring that ends in a music note,
// perched on a sound wave. While music plays it sings: the beak opens and closes, the sound
// lines flash, the note glows, the wave bars bounce and a light runs round the ring (see .pinson in app.css).
// web/img/pinson.svg is the same drawing without the animation hooks (keep the two in sync).

import { store } from './store.js';

let uid = 0;

/** A fresh <svg> of the logo, `size` px square. Decorative: the caller labels the link around it. */
export function pinsonMark(size = 32) {
  const id = `pn${++uid}`;
  const tpl = document.createElement('template');
  tpl.innerHTML = `<svg class="pinson" width="${size}" height="${size}" viewBox="0 0 200 200" aria-hidden="true" focusable="false">
    <defs>
      <radialGradient id="${id}bg" cx="100" cy="92" r="125" gradientUnits="userSpaceOnUse"><stop offset="0" stop-color="#232f85"/><stop offset=".55" stop-color="#131c5c"/><stop offset="1" stop-color="#0a1140"/></radialGradient>
      <radialGradient id="${id}halo" cx="104" cy="98" r="62" gradientUnits="userSpaceOnUse"><stop offset="0" stop-color="#5a4fd6" stop-opacity=".45"/><stop offset="1" stop-color="#5a4fd6" stop-opacity="0"/></radialGradient>
      <linearGradient id="${id}ring" x1="30" y1="0" x2="176" y2="0" gradientUnits="userSpaceOnUse"><stop offset="0" stop-color="#36e0ff"/><stop offset=".38" stop-color="#5e98ff"/><stop offset=".72" stop-color="#9c6bff"/><stop offset="1" stop-color="#da7cff"/></linearGradient>
      <linearGradient id="${id}note" x1="0" y1="15" x2="0" y2="104" gradientUnits="userSpaceOnUse"><stop offset="0" stop-color="#ffe7a0"/><stop offset=".32" stop-color="#ffb37c"/><stop offset=".66" stop-color="#f68ccc"/><stop offset=".84" stop-color="#d57bff"/><stop offset="1" stop-color="#d57bff" stop-opacity="0"/></linearGradient>
      <linearGradient id="${id}flash" x1="0" y1="15" x2="0" y2="104" gradientUnits="userSpaceOnUse"><stop offset=".6" stop-color="#fff"/><stop offset=".9" stop-color="#fff" stop-opacity="0"/></linearGradient>
      <radialGradient id="${id}body" cx="118" cy="94" r="58" gradientUnits="userSpaceOnUse"><stop offset="0" stop-color="#ff5552"/><stop offset=".5" stop-color="#ff824f"/><stop offset="1" stop-color="#ffd0ac"/></radialGradient>
      <radialGradient id="${id}cap" cx="62" cy="62" r="52" gradientUnits="userSpaceOnUse"><stop offset="0" stop-color="#4cc0ff"/><stop offset=".5" stop-color="#6a84ff" stop-opacity=".9"/><stop offset=".82" stop-color="#8a6cff" stop-opacity="0"/></radialGradient>
      <linearGradient id="${id}wing" x1="44" y1="98" x2="114" y2="136" gradientUnits="userSpaceOnUse"><stop offset="0" stop-color="#3fdcff"/><stop offset=".45" stop-color="#5f8cff"/><stop offset="1" stop-color="#8d60ff"/></linearGradient>
      <linearGradient id="${id}tail" x1="26" y1="158" x2="90" y2="130" gradientUnits="userSpaceOnUse"><stop offset="0" stop-color="#b45cff"/><stop offset="1" stop-color="#6f5cff"/></linearGradient>
      <linearGradient id="${id}beak" x1="0" y1="67" x2="0" y2="84" gradientUnits="userSpaceOnUse"><stop offset="0" stop-color="#ffd27a"/><stop offset="1" stop-color="#ff9640"/></linearGradient>
      <linearGradient id="${id}bars" x1="60" y1="0" x2="140" y2="0" gradientUnits="userSpaceOnUse"><stop offset="0" stop-color="#33e2ff"/><stop offset=".55" stop-color="#8f6dff"/><stop offset="1" stop-color="#dc7cff"/></linearGradient>
      <clipPath id="${id}t"><rect width="200" height="200" rx="45"/></clipPath>
      <clipPath id="${id}b"><path d="M119 70C114 61 106 57 98 58 85 59 75 67 70 78 63 92 58 110 64 126 70 140 86 147 100 146 116 145 128 134 132 118 135 106 133 94 129 86 127 83 125 81 123 80Z"/></clipPath>
      <filter id="${id}glow" x="-20%" y="-20%" width="140%" height="140%"><feGaussianBlur stdDeviation="2.2" result="b"/><feMerge><feMergeNode in="b"/><feMergeNode in="SourceGraphic"/></feMerge></filter>
    </defs>
    <g clip-path="url(#${id}t)">
      <rect width="200" height="200" fill="url(#${id}bg)"/>
      <circle cx="104" cy="98" r="62" fill="url(#${id}halo)"/>
      <g filter="url(#${id}glow)">
        <path d="M132 34.1A75 75 0 1 0 173 101V86" fill="none" stroke="url(#${id}ring)" stroke-width="8"/>
        <path class="p-spark" d="M132 34.1A75 75 0 1 0 173 101" pathLength="100" fill="none" stroke="#fff" stroke-width="3.5" stroke-linecap="round" stroke-dasharray="7 93" stroke-dashoffset="100" opacity="0"/>
        <g fill="url(#${id}note)"><!-- the flag fades into the ring, which runs up under it: no seam -->
          <rect x="129" y="15" width="6" height="44" rx="3"/>
          <path d="M132 15C154 19 177 34 177 62V104H169V74C167 55 153 42 134.5 36Z"/>
        </g>
        <g class="p-note" fill="url(#${id}flash)" opacity="0">
          <rect x="129" y="15" width="6" height="44" rx="3"/>
          <path d="M132 15C154 19 177 34 177 62V104H169V74C167 55 153 42 134.5 36Z"/>
        </g>
      </g>
      <g class="p-bird">
        <path d="M78 124C66 136 46 148 24 157 20 159 21.5 162.5 26 162 50 160 72 152 92 138Z" fill="url(#${id}tail)"/>
        <path d="M119 70C114 61 106 57 98 58 85 59 75 67 70 78 63 92 58 110 64 126 70 140 86 147 100 146 116 145 128 134 132 118 135 106 133 94 129 86 127 83 125 81 123 80Z" fill="url(#${id}body)"/>
        <rect width="200" height="200" fill="url(#${id}cap)" clip-path="url(#${id}b)"/>
        <path d="M116 103C106 93 86 92 72 100 60 107 48 120 36 132 54 132 70 134 86 134 104 133 116 122 116 103Z" fill="url(#${id}wing)"/>
        <path d="M114 113C106 127 92 135 74 137 88 143 106 140 116 126Z" fill="#7650e6" opacity=".75"/>
        <path d="M99 76Q104.5 69.5 110 76" fill="none" stroke="#3a1a30" stroke-width="2.4" stroke-linecap="round"/>
        <path d="M122 74.5 133 66 135 82Z" fill="#d84f2c"/>
        <path class="p-beak-up" d="M118 67C123 65 129 63.5 134.5 63 131 68 127 72.5 122.5 76Z" fill="url(#${id}beak)"/>
        <path class="p-beak-lo" d="M122.5 77.5C127 79 131.5 80.5 136 82 132 84.5 128 86.3 124 86.8Z" fill="url(#${id}beak)"/>
        <g class="p-sound" fill="none" stroke="#ffc95c" stroke-width="3.6" stroke-linecap="round" filter="url(#${id}glow)">
          <path d="M142 63 147 56"/><path d="M144 73.5 156 72"/><path d="M142.5 84 151.5 89.5"/>
        </g>
      </g>
      <g class="p-bars" fill="url(#${id}bars)" filter="url(#${id}glow)">
        <rect x="60" y="157.5" width="5.6" height="7" rx="2.8"/>
        <rect x="71.2" y="153.5" width="5.6" height="13" rx="2.8"/>
        <rect x="85.2" y="146" width="5.6" height="24" rx="2.8"/>
        <rect x="97.2" y="138" width="5.6" height="40" rx="2.8"/>
        <rect x="110.4" y="144" width="5.6" height="28" rx="2.8"/>
        <rect x="120.4" y="150" width="5.6" height="16" rx="2.8"/>
        <rect x="130" y="157.5" width="5.6" height="7" rx="2.8"/>
      </g>
    </g>
  </svg>`;
  return tpl.content.firstElementChild;
}

// One root class drives every mark on the page, including ones views create later.
const sync = () => document.documentElement.classList.toggle('is-playing', store.state.status === 'playing');
store.on('state', sync);
sync();
