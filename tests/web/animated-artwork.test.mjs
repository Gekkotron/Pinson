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

test('a native media error and play rejection trigger one HLS fallback', async () => {
  const url = 'https://example.com/fallback.m3u8';
  let rejectNativePlay;
  let attached;
  let destroyed = 0;
  let loads = 0;
  const videos = [];
  class FakeHls {
    static Events = { ERROR: 'error' };
    static isSupported() { return true; }
    on() {}
    loadSource(source) { assert.equal(source, url); }
    attachMedia(video) { attached = video; }
    destroy() { destroyed++; }
  }
  globalThis.window = {
    matchMedia: () => ({ matches: false, addEventListener() {}, removeEventListener() {} }),
    Hls: FakeHls,
  };
  globalThis.fetch = async () => ({ ok: true, json: async () => ({ url }) });
  globalThis.document = {
    createElement(tag) {
      if (tag === 'script') return {};
      return Object.assign(new EventTarget(), {
        canPlayType: () => 'maybe',
        play() {
          return videos[0] === this
            ? new Promise((_resolve, reject) => { rejectNativePlay = reject; })
            : Promise.resolve();
        },
        pause() {},
        load() {},
        setAttribute() {},
        removeAttribute() {},
        remove() { this.removed = true; },
      });
    },
    head: { append(script) { loads++; script.onload(); } },
  };
  const stop = animateCover({ append(video) { videos.push(video); } }, { artists: 'Artist fallback', album: 'Album fallback' });
  await tick();
  assert.equal(videos[0].src, url);
  assert.equal(loads, 0);
  videos[0].dispatchEvent(new Event('error'));
  rejectNativePlay(new Error('No supported source'));
  await tick();
  assert.equal(loads, 1);
  assert.equal(videos.length, 2);
  assert.equal(videos[0].removed, true);
  assert.equal(attached, videos[1]);
  stop();
  assert.equal(videos[1].removed, true);
  assert.equal(destroyed, 1);
});
