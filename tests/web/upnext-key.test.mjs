// Run with: node --test 'tests/web/*.test.mjs'
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { upNextKey } from '../../web/js/upnext-key.js';

const song = (videoId, title = videoId) => ({ videoId, title, artists: 'A', artworkUrl: `https://img/${videoId}` });
const state = (over = {}) => ({
  current: { id: 1, song: song('a') },
  status: 'playing',
  positionMs: 1000,
  positionRunning: true,
  upcoming: [{ id: 2, song: song('b') }, { id: 3, song: song('c') }],
  autoplay: { enabled: true, basedOn: song('a'), songs: [song('x')] },
  crossfadeSecs: 6,
  version: 7,
  ...over,
});

test('play/pause, position and version changes keep the same key', () => {
  const k = upNextKey(state());
  assert.equal(upNextKey(state({ status: 'paused', positionRunning: false })), k);
  assert.equal(upNextKey(state({ positionMs: 42_000, version: 8 })), k);
});

test('queue, current song and autoplay changes give a new key', () => {
  const k = upNextKey(state());
  assert.notEqual(upNextKey(state({ current: { id: 9, song: song('z') } })), k);
  assert.notEqual(upNextKey(state({ upcoming: [{ id: 3, song: song('c') }] })), k);
  assert.notEqual(upNextKey(state({ upcoming: [{ id: 3, song: song('c') }, { id: 2, song: song('b') }] })), k);
  assert.notEqual(upNextKey(state({ autoplay: { enabled: false, basedOn: null, songs: [] } })), k);
  assert.notEqual(upNextKey(state({ autoplay: { enabled: true, basedOn: song('a'), songs: [song('y')] } })), k);
});

test('a crossfade change gives a new key', () => {
  assert.notEqual(upNextKey(state({ crossfadeSecs: 12 })), upNextKey(state()));
});

test('nothing playing has a key too', () => {
  assert.equal(typeof upNextKey(state({ current: null, upcoming: [] })), 'string');
});
