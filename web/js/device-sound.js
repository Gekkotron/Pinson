// Plays the music on this browser too, as a synced Nidoo speaker (Sendspin player). Off by default,
// per browser. The ~450 KB player bundle is only fetched when the switch is on.

import { ApiError, browserPlayer } from './api.js';
import { deviceName } from './device-name.js';
import { h, toast } from './ui.js';

export { deviceName };

const KEY = 'pinson.deviceSound';
const BUNDLE = '../vendor/sendspin-js-6.0.0.js';
// unlock() resolves once the AudioContext runs; a browser that blocks audio leaves it pending (or rejects).
const UNLOCK_WAIT_MS = 1500;

export function deviceSoundOn() {
  try {
    return localStorage.getItem(KEY) === 'on';
  } catch (_) {
    return false;
  }
}

export function setDeviceSound(on) {
  try {
    if (on) localStorage.setItem(KEY, 'on');
    else localStorage.removeItem(KEY);
  } catch (_) {
    // storage unavailable: the switch only lasts until reload
  }
}

let urlPromise = null;

function playerUrl() {
  if (!urlPromise) {
    urlPromise = browserPlayer().then((r) => r.url);
    urlPromise.catch(() => (urlPromise = null));
  }
  return urlPromise;
}

let player = null;
let starting = null;
let run = 0; // bumped by every start and stop, so a start that is overtaken drops its result
let note = null;
let armed = false;
const toasted = new Set();

function fail(kind, err, message) {
  console.error('Device sound:', kind, err);
  if (toasted.has(kind)) return;
  toasted.add(kind);
  toast(message);
}

/** Resolves true once the audio output runs, false if the browser blocks it. */
function unlockWithin(p) {
  return new Promise((resolve) => {
    const timer = setTimeout(() => resolve(false), UNLOCK_WAIT_MS);
    p.unlock().then(
      () => {
        clearTimeout(timer);
        if (player === p) hideNote();
        resolve(true);
      },
      (err) => {
        clearTimeout(timer);
        console.warn('Device sound: audio is blocked', err);
        resolve(false);
      },
    );
  });
}

const GESTURES = ['keydown', 'pointerdown', 'click'];

function onGesture() {
  disarm();
  const p = player;
  if (!p) return;
  // unlock() first, synchronously, so the browser sees it inside the gesture.
  unlockWithin(p).then((ok) => {
    if (player !== p) return;
    if (ok) hideNote();
    else arm();
  });
}

function arm() {
  if (armed) return;
  armed = true;
  for (const type of GESTURES) document.addEventListener(type, onGesture, true);
}

function disarm() {
  armed = false;
  for (const type of GESTURES) document.removeEventListener(type, onGesture, true);
}

function showNote() {
  if (!note) {
    note = h('div', { class: 'device-note', role: 'status', text: 'Press OK to start sound' });
    document.body.append(note);
  }
  arm();
}

function hideNote() {
  disarm();
  if (note) note.remove();
  note = null;
}

async function begin(gen) {
  let url;
  try {
    url = await playerUrl();
  } catch (err) {
    if (!(err instanceof ApiError && err.status === 404)) fail('server', err, "Couldn't reach the sound server");
    return;
  }
  let mod;
  try {
    mod = await import(BUNDLE);
  } catch (err) {
    fail('load', err, "Couldn't load the sound player");
    return;
  }
  if (run !== gen) return;
  const p = new mod.SendspinPlayer({
    url,
    clientName: deviceName(navigator.userAgent),
    productName: 'Pinson',
    manufacturer: 'Gekkotron',
    softwareVersion: '1',
    correctionMode: 'sync',
    reconnect: {
      onReconnecting: (n) => console.info(`Device sound: reconnecting (attempt ${n})`),
      onReconnected: () => console.info('Device sound: reconnected'),
    },
  });
  player = p;
  const unlocked = unlockWithin(p);
  try {
    await p.connect();
  } catch (err) {
    fail('connect', err, "Couldn't connect this device to Nidoo");
  }
  if (run !== gen) return;
  if (!(await unlocked) && run === gen) showNote();
}

/** Starts sound on this device (no-op if already running). Never throws. */
export function startDeviceSound() {
  if (player || starting) return starting || Promise.resolve();
  const gen = ++run;
  const job = begin(gen)
    .catch((err) => fail('start', err, "Couldn't start sound on this device"))
    .finally(() => {
      if (starting === job) starting = null;
    });
  starting = job;
  return job;
}

/** Stops sound at once and drops the player. */
export function stopDeviceSound() {
  run++;
  urlPromise = null; // the next output may serve another player URL
  starting = null;
  hideNote();
  const p = player;
  player = null;
  if (!p) return;
  try {
    p.destroy('user_request');
  } catch (err) {
    console.error('Device sound: stop failed', err);
  }
}
