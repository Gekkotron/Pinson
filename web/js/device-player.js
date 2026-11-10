// "This device" output: this tab plays the server's MP3 stream in one <audio> element.
// `reduce` is the pure core (node tests it); `startDevicePlayer` is the small DOM shell around it.

import { player } from './api.js';
import { deviceId, outputName } from './device-id.js';
import { setHello, send, store } from './store.js';

const POSITION_EVERY_MS = 5000;

const streamUrl = (local, generation, fromMs) =>
  `/api/device/stream/${generation}?device=${encodeURIComponent(local.deviceId)}&fromMs=${fromMs}`;

/**
 * `msg` is a server `device.*` message or a local event: `ended`, `error` ({reason}), `blocked`,
 * `position` ({currentTimeMs}). The shell adds `hasSource` and `elementPaused` where they matter.
 * Returns the next `local` and the actions the shell runs in order.
 */
export function reduce(local, msg) {
  switch (msg.type) {
    case 'device.load': {
      const adopts = msg.chained && (local.chainedAfter != null || local.generation === msg.generation);
      if (adopts) {
        const next = {
          ...local,
          generation: msg.generation,
          startMs: local.chainedAfter != null ? msg.startMs : local.startMs,
          chainedAfter: null,
          wantPlaying: !msg.paused,
        };
        const actions = [{ do: 'adopt' }];
        if (msg.paused) actions.push({ do: 'pause' });
        else if (msg.elementPaused) actions.push({ do: 'play' });
        return { local: next, actions };
      }
      const next = {
        ...local,
        generation: msg.generation,
        startMs: msg.startMs,
        chainedAfter: null,
        lastPositionMs: msg.startMs,
        errored: false,
        retried: false,
        wantPlaying: !msg.paused,
      };
      return { local: next, actions: [{ do: 'load', url: msg.url, paused: !!msg.paused }] };
    }
    case 'device.pause':
      return { local: { ...local, wantPlaying: false }, actions: [{ do: 'pause' }] };
    case 'device.resume': {
      if (!local.errored && msg.hasSource) return { local: { ...local, wantPlaying: true }, actions: [{ do: 'play' }] };
      if (local.generation == null) return { local, actions: [] };
      const url = streamUrl(local, local.generation, local.lastPositionMs);
      const next = { ...local, startMs: local.lastPositionMs, errored: false, wantPlaying: true };
      return { local: next, actions: [{ do: 'load', url, paused: true }, { do: 'play' }] };
    }
    case 'device.stop':
      return {
        local: { ...local, generation: null, startMs: 0, chainedAfter: null, errored: false, wantPlaying: false },
        actions: [{ do: 'stop' }],
      };
    case 'ended': {
      if (local.generation == null && local.chainedAfter != null) {
        // The chained stream was never adopted (the socket was suspended): keep following the server.
        const url = `/api/device/next?device=${encodeURIComponent(local.deviceId)}`;
        return { local, actions: [{ do: 'load', url, paused: false }] };
      }
      if (local.generation == null) return { local, actions: [] };
      const after = local.generation;
      const url = `/api/device/next?device=${encodeURIComponent(local.deviceId)}&after=${after}`;
      return {
        local: { ...local, generation: null, chainedAfter: after, wantPlaying: true },
        actions: [{ do: 'load', url, paused: false }],
      };
    }
    case 'error': {
      if (local.generation != null && !local.retried) {
        // Often a connection the browser dropped during a long pause: reopen where we were.
        const url = streamUrl(local, local.generation, local.lastPositionMs);
        const next = { ...local, startMs: local.lastPositionMs, retried: true, errored: false };
        return { local: next, actions: [{ do: 'load', url, paused: !local.wantPlaying }] };
      }
      const next = { ...local, errored: true };
      const actions =
        local.generation == null
          ? []
          : [{ do: 'send', msg: { type: 'device.failed', generation: local.generation, reason: msg.reason || 'playback error' } }];
      return { local: next, actions };
    }
    case 'position': {
      const positionMs = local.startMs + Math.round(msg.currentTimeMs);
      const next = { ...local, lastPositionMs: positionMs, retried: false };
      const actions =
        local.generation == null ? [] : [{ do: 'send', msg: { type: 'device.position', generation: local.generation, positionMs } }];
      return { local: next, actions };
    }
    case 'blocked':
      return { local: { ...local, wantPlaying: false }, actions: [{ do: 'pauseServer' }] };
    default:
      return { local, actions: [] };
  }
}

// ---------- DOM shell ----------
// ui.js needs `window` when it loads, so the shell imports it on use and node can still load this file.
const ui = () => import('./ui.js');

// One element for the whole page life: iOS unlocks audio per element.
let audio = null;
let local = null;
let positionTimer = 0;
let tapArmed = false;

// 0.1 s of silence, enough to earn the user-gesture unlock.
const SILENT_WAV =
  'data:audio/wav;base64,UklGRiQAAABXQVZFZm10IBAAAAABAAEAgLsAAAB3AQACABAAZGF0YQAAAAA=';

const hasSource = () => !!audio.getAttribute('src') && audio.getAttribute('src') !== SILENT_WAV;

/** Plays then pauses silence on the shared element so later play() calls are allowed. Only while it has no stream. */
export function unlockAudio() {
  if (!audio || hasSource()) return;
  audio.src = SILENT_WAV;
  audio
    .play()
    .then(() => {
      audio.pause();
      if (audio.getAttribute('src') === SILENT_WAV) {
        audio.removeAttribute('src');
        audio.load();
      }
    })
    .catch(() => {});
}

function playAudio() {
  audio.play().catch((err) => {
    if (err && err.name === 'NotAllowedError') dispatch({ type: 'blocked' });
  });
}

function positionNowMs() {
  return local.startMs + Math.round(audio.currentTime * 1000);
}

function reportPosition() {
  dispatch({ type: 'position', currentTimeMs: audio.currentTime * 1000 });
}

function armTap() {
  if (tapArmed) return;
  tapArmed = true;
  document.addEventListener(
    'pointerdown',
    () => {
      tapArmed = false;
      unlockAudio();
      if (hasSource()) playAudio();
      player.resume().catch(() => {});
    },
    { once: true },
  );
}

function run(action) {
  switch (action.do) {
    case 'load':
      audio.src = action.url;
      if (!action.paused) playAudio();
      mediaSessionOn();
      break;
    case 'play':
      playAudio();
      break;
    case 'pause':
      audio.pause();
      break;
    case 'stop':
      audio.pause();
      audio.removeAttribute('src');
      audio.load();
      mediaSessionOff();
      break;
    case 'send':
      send(action.msg);
      break;
    case 'pauseServer':
      player.pause().catch(() => {});
      ui().then((m) => m.toast('Tap anywhere to play here'));
      armTap();
      break;
    case 'adopt':
      mediaSessionOn();
      break;
  }
}

function dispatch(msg) {
  const r = reduce(local, { ...msg, hasSource: hasSource(), elementPaused: audio.paused });
  local = r.local;
  r.actions.forEach(run);
}

// ---------- Media Session ----------
let sessionOn = false;

function mediaSessionOn() {
  if (!('mediaSession' in navigator)) return;
  sessionOn = true;
  const handlers = {
    play: () => player.resume().catch(() => {}),
    pause: () => player.pause().catch(() => {}),
    nexttrack: () => ui().then((m) => m.skip()),
    previoustrack: () => ui().then((m) => m.previous()),
  };
  for (const [name, fn] of Object.entries(handlers)) {
    try {
      navigator.mediaSession.setActionHandler(name, fn);
    } catch (_) {}
  }
  paintMediaSession();
}

function mediaSessionOff() {
  if (!('mediaSession' in navigator)) return;
  sessionOn = false;
  navigator.mediaSession.metadata = null;
  for (const name of ['play', 'pause', 'nexttrack', 'previoustrack']) {
    try {
      navigator.mediaSession.setActionHandler(name, null);
    } catch (_) {}
  }
}

function paintMediaSession() {
  if (!sessionOn) return;
  const cur = store.state.current;
  if (!cur) {
    navigator.mediaSession.metadata = null;
    return;
  }
  const { title, artists, album, artworkUrl } = cur.song;
  navigator.mediaSession.metadata = new MediaMetadata({
    title,
    artist: artists || '',
    album: album || '',
    artwork: artworkUrl ? [{ src: '/api/artwork?url=' + encodeURIComponent(artworkUrl) }] : [],
  });
}

/** What the element holds, for `hello`. An unadopted chain has no generation: the hub restarts it at the stream start. */
function loadedNow() {
  if (!hasSource() || local.errored) return null;
  const positionMs = local.generation != null ? positionNowMs() : Math.round(audio.currentTime * 1000);
  return { generation: local.generation, positionMs, playing: !audio.paused };
}

/** Call once: wires the audio element, the WebSocket `device.*` messages and the `hello` handshake. */
export function startDevicePlayer() {
  if (audio) return;
  audio = new Audio();
  local = { deviceId: deviceId(), generation: null, startMs: 0, chainedAfter: null, lastPositionMs: 0, errored: false, wantPlaying: false, retried: false };

  audio.addEventListener('playing', () => {
    reportPosition();
    clearInterval(positionTimer);
    positionTimer = setInterval(reportPosition, POSITION_EVERY_MS);
  });
  const stopTimer = () => clearInterval(positionTimer);
  audio.addEventListener('pause', stopTimer);
  audio.addEventListener('ended', () => {
    stopTimer();
    if (hasSource()) dispatch({ type: 'ended' });
  });
  audio.addEventListener('error', () => {
    stopTimer();
    if (hasSource()) dispatch({ type: 'error', reason: (audio.error && audio.error.message) || 'playback error' });
  });

  document.addEventListener('pointerdown', unlockAudio, { once: true });

  store.on('device', (msg) => dispatch(msg));
  store.on('state', paintMediaSession);
  setHello(() => ({
    type: 'hello',
    deviceId: local.deviceId,
    name: outputName(navigator.userAgent),
    loaded: loadedNow(),
  }));
}
