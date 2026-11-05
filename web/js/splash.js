// Easter egg: double-click (or double-tap) any Pinson logo. The page blurs, a big chaffinch pops in,
// hops twice and sings a short song while notes fly out, then everything fades away.
// The song is synthesised with Web Audio (no audio file) and plays on this device, not the house speakers.
// Animations live in app.css (.splash), so prefers-reduced-motion turns them off there.

import { pinsonMark } from './logo.js';
import { h } from './ui.js';

const TAP_GAP_MS = 400; // two clicks on a logo closer than this count as a double-click
const SHOW_MS = 2600; // time on screen before the fade-out starts
const FADE_MS = 350; // keep in sync with .splash.out in app.css
const SING_AT_S = 0.42; // the bird starts hopping (and singing) once the pop-in ends

// [x, y] start offset from the bird's centre, [dx, dy] drift, delay in ms
const NOTES = [
  [52, -40, 36, -56, 0, '♪'],
  [-58, -30, -38, -52, 180, '♫'],
  [62, 28, 46, -18, 360, '♫'],
  [-60, 40, -48, -12, 520, '♪'],
];

let ctx = null;
let current = null;
let lastTap = { at: 0, logo: null };

function audio() {
  const Ctx = window.AudioContext || window.webkitAudioContext;
  if (!Ctx) return null;
  ctx ??= new Ctx();
  if (ctx.state === 'suspended') ctx.resume();
  return ctx;
}

/** One note: a quick sine sweep f0 → f1 (→ f2), with a short attack and an exponential decay. */
function tone(c, t0, dur, gain, f0, f1, f2) {
  const osc = c.createOscillator();
  const env = c.createGain();
  osc.type = 'sine';
  osc.frequency.setValueAtTime(f0, t0);
  if (f2) {
    osc.frequency.exponentialRampToValueAtTime(f1, t0 + dur * 0.45);
    osc.frequency.exponentialRampToValueAtTime(f2, t0 + dur);
  } else {
    osc.frequency.exponentialRampToValueAtTime(f1, t0 + dur);
  }
  env.gain.setValueAtTime(0.0001, t0);
  env.gain.exponentialRampToValueAtTime(gain, t0 + 0.006);
  env.gain.exponentialRampToValueAtTime(0.0001, t0 + dur);
  osc.connect(env).connect(c.destination);
  osc.start(t0);
  osc.stop(t0 + dur + 0.02);
}

/** About one second of chaffinch-like song: a falling trill that speeds up, then a final flourish. */
export function chirp(delayS = 0) {
  const c = audio();
  if (!c) return;
  let at = c.currentTime + 0.02 + delayS;
  let gap = 0.11;
  let f = 5400;
  for (let i = 0; i < 9; i++) {
    tone(c, at, 0.055, 0.16, f, f * 0.8);
    at += gap;
    gap = Math.max(0.055, gap * 0.9);
    f *= 0.965;
  }
  tone(c, at + 0.03, 0.3, 0.2, 2900, 5600, 2500);
}

function close() {
  if (!current || current.closing) return;
  const el = current;
  el.closing = true;
  clearTimeout(el.timer);
  el.classList.add('out');
  setTimeout(() => {
    el.remove();
    if (current === el) current = null;
  }, FADE_MS);
}

/** Show the splash and sing. Does nothing while a splash is already on screen. */
export function showSplash() {
  if (current) return;
  const notes = NOTES.map(([x, y, dx, dy, delay, glyph]) => {
    const n = h('span', { class: 'splash-note', text: glyph });
    n.style.setProperty('--x', `${x}px`);
    n.style.setProperty('--y', `${y}px`);
    n.style.setProperty('--dx', `${dx}px`);
    n.style.setProperty('--dy', `${dy}px`);
    n.style.animationDelay = `${SING_AT_S * 1000 + delay}ms`;
    return n;
  });
  const el = h(
    'div',
    { class: 'splash', 'aria-hidden': 'true', onclick: close },
    h('div', { class: 'splash-stage' }, pinsonMark(132), ...notes),
    h('div', { class: 'splash-word', text: 'Pinson' }),
  );
  current = el;
  document.body.append(el);
  chirp(SING_AT_S);
  el.timer = setTimeout(close, SHOW_MS);
}

// Delegated, so it covers the sidebar logo, the Home logo and any logo a view adds later.
// Two clicks rather than `dblclick`: phones send two clicks for a double tap but rarely a dblclick.
document.addEventListener('click', (e) => {
  const logo = e.target instanceof Element ? e.target.closest('.pinson') : null;
  if (!logo || logo.closest('.splash')) return;
  const now = performance.now();
  if (lastTap.logo === logo && now - lastTap.at < TAP_GAP_MS) {
    lastTap = { at: 0, logo: null };
    showSplash();
  } else {
    lastTap = { at: now, logo };
  }
});
