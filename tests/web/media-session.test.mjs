import { test } from 'node:test';
import assert from 'node:assert/strict';
import { startDevicePlayer } from '../../web/js/device-player.js';
import { connect } from '../../web/js/store.js';

class FakeAudio {
  constructor() { this.paused = true; this.ended = false; this.currentTime = 0; this.events = {}; this.src = ''; }
  addEventListener(name, fn) { this.events[name] = fn; }
  getAttribute(name) { return name === 'src' ? this.src : null; }
  removeAttribute(name) { if (name === 'src') this.src = ''; }
  load() {}
  play() { this.paused = false; this.events.playing(); return Promise.resolve(); }
  pause() { this.paused = true; this.events.pause(); }
}

test('media session follows local audio and cleans up on stop', (t) => {
  t.after(() => { if (globalThis.testAudio && !testAudio.paused) testAudio.pause(); });
  const handlers = new Map();
  const calls = [];
  const session = { metadata: null, playbackState: 'none', position: null, setActionHandler(name, fn) { calls.push(name); handlers.set(name, fn); }, setPositionState(position) { this.position = position || null; } };
  Object.defineProperty(globalThis, 'navigator', { value: { mediaSession: session, userAgent: '' }, configurable: true });
  globalThis.MediaMetadata = class { constructor(data) { Object.assign(this, data); } };
  globalThis.Audio = class extends FakeAudio { constructor() { super(); globalThis.testAudio = this; } };
  globalThis.document = { addEventListener() {} };
  globalThis.localStorage = { getItem() { return 'test-device'; }, setItem() {} };
  globalThis.location = { protocol: 'http:', host: 'localhost' };
  globalThis.WebSocket = class { constructor() { globalThis.testSocket = this; } send() {} };
  startDevicePlayer();
  connect();
  const receive = (msg) => testSocket.onmessage({ data: JSON.stringify(msg) });
  receive({ type: 'state', version: 1, positionMs: 12000, current: { id: 1, song: { title: 'Title', artists: 'Artist', album: 'Album', durationSecs: 180, artworkUrl: 'https://yt3.googleusercontent.com/cover=w1200-h1200' } } });
  receive({ type: 'device.load', generation: 1, startMs: 12000, url: '/stream', paused: false });
  assert.deepEqual([...handlers.keys()], ['play', 'pause', 'nexttrack', 'previoustrack']);
  assert.equal(session.playbackState, 'playing');
  assert.deepEqual(session.position, { duration: 180, position: 12, playbackRate: 1 });
  assert.equal(session.metadata.title, 'Title');
  assert.equal(session.metadata.artist, 'Artist');
  assert.equal(session.metadata.album, 'Album');
  assert.equal(session.metadata.artwork[0].src, 'https://yt3.googleusercontent.com/cover=w512-h512');
  receive({ type: 'device.load', generation: 2, startMs: 0, url: '/stream2', paused: true });
  assert.equal(calls.length, 4);
  receive({ type: 'device.pause' });
  assert.equal(session.playbackState, 'paused');
  receive({ type: 'state', version: 2, current: { id: 2, song: { title: 'Next' } } });
  assert.equal(session.metadata.title, 'Next');
  assert.deepEqual(session.metadata.artwork, []);
  assert.equal(session.position, null);
  receive({ type: 'device.resume' });
  assert.equal(session.playbackState, 'playing');
  receive({ type: 'device.stop' });
  assert.equal(session.playbackState, 'none');
  assert.equal(session.metadata, null);
  assert.equal(session.position, null);
  assert.ok([...handlers.values()].every((handler) => handler === null));
});
