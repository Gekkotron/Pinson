import { test } from 'node:test';
import assert from 'node:assert/strict';
import { animateCover } from '../../web/js/animated-artwork.js';

const tick = () => new Promise((resolve) => setImmediate(resolve));

test('late lookup cannot add a video after the song changes', async () => {
  let respond;
  globalThis.window = { matchMedia: () => ({ matches: false, addEventListener() {}, removeEventListener() {} }) };
  globalThis.fetch = () => new Promise((resolve) => { respond = resolve; });
  const box = { append() { assert.fail('stale artwork was attached'); } };
  const stop = animateCover(box, { artists: 'Artist race', album: 'Album race' });
  stop();
  respond({ ok: true, json: async () => ({ url: 'https://example.com/video.m3u8' }) });
  await tick();
});

test('reduced motion skips the artwork lookup', () => {
  globalThis.window = { matchMedia: () => ({ matches: true, addEventListener() {}, removeEventListener() {} }) };
  globalThis.fetch = () => assert.fail('looked up artwork despite reduced motion');
  animateCover({}, { artists: 'Artist', album: 'Album' })();
});
