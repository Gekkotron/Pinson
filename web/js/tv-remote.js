// TV remote buttons for the big-screen lyrics page (`/lyrics`).
// TV browsers report remote buttons differently: modern ones give a `key` name ("ArrowUp",
// "MediaPlayPause", "ChannelUp"), older ones only a numeric `keyCode` (HbbTV / CE-HTML codes used by
// LG webOS, Samsung Tizen, Philips, Sony, Panasonic...). Each action lists both, so one table covers
// many TVs. TV volume buttons are left alone: they belong to the TV.
// Some TV browsers (Philips without Android TV, for one) keep the arrows and OK for their own pointer
// and only pass the colour keys on, so those carry the playback actions too:
// red previous · green pause · yellow play · blue next.

/** Action -> `key` names (KeyboardEvent.key). */
const BY_KEY = {
  bigger: ['ArrowUp', 'ChannelUp', 'PageUp', '+', '='],
  smaller: ['ArrowDown', 'ChannelDown', 'PageDown', '-', '_'],
  playPause: ['Enter', 'MediaPlayPause'],
  play: ['MediaPlay', 'Play', 'ColorF2Yellow'],
  pause: ['MediaPause', 'Pause', 'ColorF1Green'],
  next: ['ArrowRight', 'MediaTrackNext', 'MediaFastForward', 'ColorF3Blue'],
  previous: ['ArrowLeft', 'MediaTrackPrevious', 'MediaRewind', 'ColorF0Red'],
};

/** Action -> `keyCode` values, for TVs whose `key` is missing or "Unidentified". */
const BY_CODE = {
  bigger: [38, 33, 427, 107, 187], // ↑, PageUp (LG channel +), VK_CHANNEL_UP, numpad +, =
  smaller: [40, 34, 428, 109, 189], // ↓, PageDown (LG channel −), VK_CHANNEL_DOWN, numpad −, -
  playPause: [13, 179, 402, 463, 10252], // OK, media play/pause (Chrome, HbbTV, LG, Samsung)
  play: [415, 250, 405], // VK_PLAY, yellow
  pause: [19, 404], // VK_PAUSE, green
  next: [39, 176, 417, 425, 10233, 406], // →, track next, VK_FAST_FWD, VK_TRACK_NEXT, Samsung, blue
  previous: [37, 177, 412, 424, 10232, 403], // ←, track previous, VK_REWIND, VK_TRACK_PREV, Samsung, red
};

const KEY_TO_ACTION = new Map();
for (const [action, keys] of Object.entries(BY_KEY)) for (const k of keys) KEY_TO_ACTION.set(k, action);
const CODE_TO_ACTION = new Map();
for (const [action, codes] of Object.entries(BY_CODE)) for (const c of codes) CODE_TO_ACTION.set(c, action);

/** How a key is saved in a custom mapping: its name, or its code when the TV gives no name. */
export function keyId(e) {
  return e.key && e.key !== 'Unidentified' ? `key:${e.key}` : `code:${e.keyCode}`;
}

/**
 * The remote action for a keydown event, or null:
 * "bigger" | "smaller" | "playPause" | "play" | "pause" | "next" | "previous".
 * `custom` (from "Set up your remote", `{ [keyId]: action }`) wins over the built-in keys.
 */
export function remoteAction(e, custom = null) {
  if (e.metaKey || e.ctrlKey || e.altKey) return null;
  if (custom) {
    const mine = custom[keyId(e)];
    if (mine) return mine;
  }
  if (e.key && e.key !== 'Unidentified') {
    const byKey = KEY_TO_ACTION.get(e.key);
    if (byKey) return byKey;
  }
  return CODE_TO_ACTION.get(e.keyCode) || null;
}

const COLOURS = { 403: 'red', 404: 'green', 405: 'yellow', 406: 'blue' };
const COLOUR_NAMES = { ColorF0Red: 403, ColorF1Green: 404, ColorF2Yellow: 405, ColorF3Blue: 406 };

/** A short name for a remote button, as shown in "Set up your remote". */
export function keyLabel(e) {
  const colour = COLOURS[e.keyCode] || COLOURS[COLOUR_NAMES[e.key]];
  if (colour) return `${colour} key (${e.keyCode})`;
  if (e.key === 'Enter') return `OK / Enter (${e.keyCode})`;
  if (e.key && e.key !== 'Unidentified') return `${e.key} (${e.keyCode})`;
  return `key ${e.keyCode}`;
}

const IGNORED_KEYS = new Set(['AudioVolumeUp', 'AudioVolumeDown', 'AudioVolumeMute', 'Shift', 'Control', 'Alt', 'Meta']);
const IGNORED_CODES = new Set([16, 17, 18, 91, 173, 174, 175, 181, 182, 183, 447, 448, 449]);

/** Volume and modifier keys: they stay with the TV and are never mapped. */
export function isIgnoredKey(e) {
  return IGNORED_KEYS.has(e.key) || IGNORED_CODES.has(e.keyCode);
}

// ---------- saved per device ----------
const CUSTOM_KEYS = 'pinson.remoteKeys';
const SETUP_SEEN = 'pinson.remoteSetup';

/** The custom mapping saved on this device (`{}` when none, or when storage is blocked). */
export function loadCustomKeys() {
  try {
    const v = JSON.parse(localStorage.getItem(CUSTOM_KEYS));
    return v && typeof v === 'object' && !Array.isArray(v) ? v : {};
  } catch {
    return {};
  }
}

export function saveCustomKeys(map) {
  try {
    localStorage.setItem(CUSTOM_KEYS, JSON.stringify(map));
  } catch {
    // storage blocked: the mapping still applies until the page reloads
  }
}

/** True once "Set up your remote" was finished or cancelled on this device (it opens by itself once). */
export function remoteSetupSeen() {
  try {
    return localStorage.getItem(SETUP_SEEN) === 'done';
  } catch {
    return true; // storage blocked: don't open it on every visit
  }
}

export function markRemoteSetupSeen() {
  try {
    localStorage.setItem(SETUP_SEEN, 'done');
  } catch {
    // storage blocked
  }
}

/**
 * Listens for remote buttons on the page and calls `handlers[action]()`. `getCustom()` returns the
 * current custom mapping. Keys typed in a field, or pressed inside an open sheet, menu or dialog,
 * are left alone; OK (Enter) on a focused button or link keeps its normal meaning.
 * Returns a function that stops listening.
 */
export function listenRemote(handlers, getCustom = () => null) {
  const onKey = (e) => {
    if (e.defaultPrevented) return;
    const action = remoteAction(e, getCustom());
    if (!action || !handlers[action]) return;
    const t = e.target;
    if (t && t.closest) {
      if (t.closest('input, textarea, select, [contenteditable="true"]')) return;
      if (t.closest('.sheet, .menu, [role="dialog"], [role="menu"]')) return;
      if (e.key === 'Enter' || e.keyCode === 13) {
        if (t.closest('button, a, [role="button"], [role="tab"], [tabindex]')) return;
      }
    }
    e.preventDefault();
    if (!e.repeat || action === 'bigger' || action === 'smaller') handlers[action]();
  };
  document.addEventListener('keydown', onKey);
  return () => document.removeEventListener('keydown', onKey);
}

/**
 * A box at the top of the screen that lists the last remote events (`/lyrics?keys=1`), to map a new
 * remote. It shows keydown, keyup and keypress, since some TV browsers only send some of them; a
 * button that adds no line is kept by the browser itself (for example to move its pointer).
 * Appended to `document.body` so no page layer can hide it.
 */
export function keyDebugBox() {
  const box = document.createElement('div');
  box.className = 'key-debug';
  const lines = [];
  const show = () => {
    box.textContent = lines.length ? lines.join('\n') : 'Remote key check: press a remote button…';
  };
  const onKey = (e) => {
    const action = e.type === 'keydown' ? ` → ${remoteAction(e) || 'not mapped'}` : '';
    lines.unshift(`${e.type}  key: ${e.key} · code: ${e.code || '—'} · keyCode: ${e.keyCode}${action}`);
    lines.length = Math.min(lines.length, 8);
    show();
  };
  const types = ['keydown', 'keyup', 'keypress'];
  for (const t of types) window.addEventListener(t, onKey, true);
  show();
  document.body.append(box);
  return {
    stop: () => {
      for (const t of types) window.removeEventListener(t, onKey, true);
      box.remove();
    },
  };
}
