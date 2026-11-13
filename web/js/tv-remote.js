// TV remote buttons for the big-screen lyrics page (`/lyrics`).
// TV browsers report remote buttons differently: modern ones give a `key` name ("ArrowUp",
// "MediaPlayPause", "ChannelUp"), older ones only a numeric `keyCode` (HbbTV / CE-HTML codes used by
// LG webOS, Samsung Tizen, Philips, Sony, Panasonic...). Each action lists both, so one table covers
// many TVs. TV volume buttons are left alone: they belong to the TV.

/** Action -> `key` names (KeyboardEvent.key). */
const BY_KEY = {
  bigger: ['ArrowUp', 'ChannelUp', 'PageUp', '+', '='],
  smaller: ['ArrowDown', 'ChannelDown', 'PageDown', '-', '_'],
  playPause: ['Enter', 'MediaPlayPause'],
  play: ['MediaPlay', 'Play'],
  pause: ['MediaPause', 'Pause'],
  next: ['ArrowRight', 'MediaTrackNext', 'MediaFastForward'],
  previous: ['ArrowLeft', 'MediaTrackPrevious', 'MediaRewind'],
};

/** Action -> `keyCode` values, for TVs whose `key` is missing or "Unidentified". */
const BY_CODE = {
  bigger: [38, 33, 427, 107, 187], // ↑, PageUp (LG channel +), VK_CHANNEL_UP, numpad +, =
  smaller: [40, 34, 428, 109, 189], // ↓, PageDown (LG channel −), VK_CHANNEL_DOWN, numpad −, -
  playPause: [13, 179, 402, 463, 10252], // OK, media play/pause (Chrome, HbbTV, LG, Samsung)
  play: [415, 250], // VK_PLAY
  pause: [19], // VK_PAUSE
  next: [39, 176, 417, 425, 10233], // →, track next, VK_FAST_FWD, VK_TRACK_NEXT, Samsung track next
  previous: [37, 177, 412, 424, 10232], // ←, track previous, VK_REWIND, VK_TRACK_PREV, Samsung
};

const KEY_TO_ACTION = new Map();
for (const [action, keys] of Object.entries(BY_KEY)) for (const k of keys) KEY_TO_ACTION.set(k, action);
const CODE_TO_ACTION = new Map();
for (const [action, codes] of Object.entries(BY_CODE)) for (const c of codes) CODE_TO_ACTION.set(c, action);

/**
 * The remote action for a keydown event, or null:
 * "bigger" | "smaller" | "playPause" | "play" | "pause" | "next" | "previous".
 */
export function remoteAction(e) {
  if (e.metaKey || e.ctrlKey || e.altKey) return null;
  if (e.key && e.key !== 'Unidentified') {
    const byKey = KEY_TO_ACTION.get(e.key);
    if (byKey) return byKey;
  }
  return CODE_TO_ACTION.get(e.keyCode) || null;
}

/**
 * Listens for remote buttons on the page and calls `handlers[action]()`.
 * Keys typed in a field, or pressed inside an open sheet or menu, are left alone; OK (Enter) on a
 * focused button or link keeps its normal meaning. Returns a function that stops listening.
 */
export function listenRemote(handlers) {
  const onKey = (e) => {
    if (e.defaultPrevented) return;
    const action = remoteAction(e);
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

/** A small box in the corner that shows what the last key sent (`&keys=1`), to map a new remote. */
export function keyDebugBox() {
  const box = document.createElement('div');
  box.className = 'key-debug';
  box.textContent = 'Press a remote button…';
  const onKey = (e) => {
    const action = remoteAction(e);
    box.textContent = `key: ${e.key} · code: ${e.code || '—'} · keyCode: ${e.keyCode} → ${action || 'not mapped'}`;
  };
  document.addEventListener('keydown', onKey, true);
  return { el: box, stop: () => document.removeEventListener('keydown', onKey, true) };
}
