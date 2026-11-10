// Live server state from the WebSocket (`/api/ws`). The socket is the source of truth:
// every `state`, `favorites` and `playlists` message is a full snapshot.

const NO_AUTOPLAY = Object.freeze({ enabled: false, basedOn: null, songs: [] });

const IDLE = Object.freeze({
  current: null,
  status: 'stopped',
  positionMs: 0,
  positionRunning: false,
  upcoming: [],
  autoplay: NO_AUTOPLAY,
  version: -1,
});

const listeners = new Map(); // event -> Set<fn>

export const store = {
  /** Latest StateView (see docs/api.md). */
  state: IDLE,
  /** Favorite songs, newest first. */
  favorites: [],
  /** PlaylistSummary[], most recently updated first. */
  playlists: [],
  /** Latest OutputView (see docs/api.md), or null until the first `output` message. */
  output: null,
  /** True while the WebSocket is open. */
  connected: false,

  /** Subscribe to "state" | "favorites" | "playlists" | "output" | "notice" | "connection" | "device". Returns an unsubscribe function. */
  on(event, fn) {
    if (!listeners.has(event)) listeners.set(event, new Set());
    listeners.get(event).add(fn);
    return () => listeners.get(event).delete(fn);
  },

  isFavorite(videoId) {
    return favoriteIds.has(videoId);
  },

  /** Replaces the favorites list (server snapshot, or an optimistic local change). */
  setFavorites(songs) {
    store.favorites = songs;
    favoriteIds = new Set(songs.map((s) => s.videoId));
    emit('favorites', songs);
  },

  /** Replaces the output view (server snapshot, or the answer to a PUT /api/output). */
  setOutput(view) {
    store.output = { current: view.current || null, startupId: view.startupId || null, available: view.available || [] };
    emit('output', store.output);
  },

  positionMs,
};

let favoriteIds = new Set();
let receivedAt = 0;

function emit(event, payload) {
  for (const fn of listeners.get(event) || []) {
    try {
      fn(payload);
    } catch (err) {
      console.error('store listener failed', event, err);
    }
  }
}

/** Playback position now, extrapolated only while the speakers are actually advancing. */
export function positionMs() {
  const s = store.state;
  let pos = s.positionMs || 0;
  if (s.status === 'playing' && s.positionRunning) pos += performance.now() - receivedAt;
  const secs = s.current && s.current.song.durationSecs;
  return secs ? Math.min(pos, secs * 1000) : pos;
}

// ---------- WebSocket ----------
let backoff = 1000;
let firstAfterConnect = true;
let started = false;
let socket = null;
let buildHello = null;

/** Registers the builder of the `hello` message sent first on every (re)connect. */
export function setHello(fn) {
  buildHello = fn;
}

/** Sends a JSON message to the server; does nothing while the socket is not open. */
export function send(msg) {
  if (socket && socket.readyState === WebSocket.OPEN) socket.send(JSON.stringify(msg));
}

/** Opens the socket once; reconnects forever with 1 s → ×2 → 10 s backoff. */
export function connect() {
  if (started) return;
  started = true;
  open();
}

function open() {
  const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
  const ws = new WebSocket(proto + '//' + location.host + '/api/ws');
  socket = ws;
  ws.onopen = () => {
    if (buildHello) send(buildHello());
    backoff = 1000;
    firstAfterConnect = true;
    setConnected(true);
  };
  ws.onmessage = (ev) => {
    let msg;
    try {
      msg = JSON.parse(ev.data);
    } catch (_) {
      return;
    }
    if (msg.type === 'state') applyState(msg);
    else if (msg.type === 'favorites') store.setFavorites(msg.songs || []);
    else if (msg.type === 'playlists') {
      store.playlists = msg.playlists || [];
      emit('playlists', store.playlists);
    } else if (msg.type === 'output') {
      store.setOutput(msg);
    } else if (typeof msg.type === 'string' && msg.type.startsWith('device.')) {
      emit('device', msg);
    } else if (msg.type === 'songFailed' || msg.type === 'autoplayFailed' || msg.type === 'noOutput') emit('notice', msg);
    // Unknown types are ignored (docs/api.md).
  };
  ws.onclose = () => {
    setConnected(false);
    setTimeout(open, backoff);
    backoff = Math.min(backoff * 2, 10000);
  };
  ws.onerror = () => ws.close();
}

let announced = null; // last value sent to "connection" listeners (null: none yet)

function setConnected(on) {
  store.connected = on;
  if (announced === on) return;
  announced = on;
  emit('connection', on);
}

function applyState(msg) {
  // Versions restart when the server restarts, so the first state after a (re)connect always wins.
  if (!firstAfterConnect && msg.version <= store.state.version) return;
  firstAfterConnect = false;
  receivedAt = performance.now();
  store.state = {
    current: msg.current || null,
    status: msg.status || 'stopped',
    positionMs: msg.positionMs || 0,
    positionRunning: !!msg.positionRunning,
    upcoming: msg.upcoming || [],
    autoplay: msg.autoplay || NO_AUTOPLAY,
    version: msg.version,
  };
  emit('state', store.state);
}
