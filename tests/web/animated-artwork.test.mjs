import { test } from 'node:test';
import assert from 'node:assert/strict';
import { animateCover, levelCap } from '../../web/js/animated-artwork.js';

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

// TV mode runs on its own copy of the module (it keeps the hls.js it loads), with a fake page
// where screen refreshes are pushed by hand and the canvas probe reads `page.pixel`.
const tv = await import('../../web/js/animated-artwork.js?tv');
let page;

class TvHls {
  static Events = { ERROR: 'hlsError', MANIFEST_PARSED: 'hlsManifestParsed' };
  static isSupported() { return page.hlsSupported; }
  constructor(config) {
    this.config = config;
    this.handlers = {};
    this.levels = page.levels;
    this.currentLevel = -1;
    page.hls = this;
  }
  on(name, handler) { this.handlers[name] = handler; }
  trigger(name, data = {}) { this.handlers[name]?.(name, data); }
  loadSource(source) { this.source = source; }
  attachMedia(video) { video.hls = this; }
  destroy() { this.destroyed = true; }
}

function canvasPage({ native = true, hlsSupported = true } = {}) {
  page = { videos: [], canvases: [], pixel: 0, frameCallbacks: [], native, hlsSupported,
    levels: [{ height: 360 }, { height: 768 }, { height: 2160 }] };
  const classList = () => {
    const names = new Set();
    return { add: (n) => names.add(n), contains: (n) => names.has(n) };
  };
  globalThis.window = {
    matchMedia: () => ({ matches: false, addEventListener() {}, removeEventListener() {} }),
    devicePixelRatio: 1,
  };
  globalThis.document = {
    createElement(tag) {
      if (tag === 'script') return {};
      if (tag === 'canvas') {
        const canvas = {
          tag, width: 300, height: 150, clientWidth: 475, classList: classList(), draws: 0,
          setAttribute() {},
          remove() { this.removed = true; },
          context: null,
          getContext() {
            this.context ??= {
              drawImage: () => { canvas.draws++; },
              getImageData: (_x, _y, w, h) => ({ data: new Uint8ClampedArray(w * h * 4).fill(page.pixel) }),
            };
            return this.context;
          },
        };
        page.canvases.push(canvas);
        return canvas;
      }
      const video = Object.assign(new EventTarget(), {
        tag, videoWidth: 768, videoHeight: 768, currentTime: 0, src: '', classList: classList(),
        canPlayType: () => (page.native ? 'maybe' : ''),
        play: () => Promise.resolve(),
        pause() {},
        load() {},
        setAttribute() {},
        removeAttribute() {},
        remove() { this.removed = true; },
        // Like the Philips TV: present, but never called for its hardware video.
        requestVideoFrameCallback() { return 1; },
      });
      page.videos.push(video);
      return video;
    },
    head: { append(script) { window.Hls = TvHls; script.onload(); } },
  };
  globalThis.requestAnimationFrame = (cb) => { page.frameCallbacks.push(cb); return page.frameCallbacks.length; };
  globalThis.cancelAnimationFrame = () => {};
  globalThis.fetch = async () => ({ ok: true, json: async () => ({ url: 'https://example.com/tv.m3u8' }) });
  page.box = { children: [], append(...els) { this.children.push(...els); } };
  // One screen refresh; `advance: false` repeats the same video frame.
  page.frame = ({ advance = true } = {}) => {
    for (const video of page.videos) if (advance) video.currentTime += 1 / 30;
    const pending = page.frameCallbacks.splice(0);
    for (const cb of pending) cb();
  };
  page.surface = () => page.canvases.filter((c) => page.box.children.includes(c)).at(-1);
  return page;
}

test('TV mode draws the video on a canvas and shows it once a frame has colour', async () => {
  canvasPage();
  const stop = tv.animateCover(page.box, { artists: 'Artist tv', album: 'Album tv' }, { canvas: true });
  await tick();
  await tick();
  const [video] = page.videos;
  const surface = page.surface();
  assert.equal(video.className, 'animated-source');
  assert.equal(video.crossOrigin, 'anonymous');
  assert.equal(surface.className, 'animated-cover');
  assert.ok(page.box.children.indexOf(surface) > page.box.children.indexOf(video), 'canvas sits above the video');
  video.dispatchEvent(new Event('playing'));
  page.frame(); // a black opening frame: skipped, the still cover stays
  assert.equal(surface.draws, 0);
  assert.equal(surface.classList.contains('playing'), false);
  page.pixel = 200;
  page.frame();
  assert.equal(surface.draws, 1, 'draws without frame callbacks');
  assert.equal(surface.classList.contains('playing'), true);
  assert.equal(surface.width, 475, 'canvas sized to the cover');
  assert.equal(surface.context.imageSmoothingQuality, 'high', 'big video frames are scaled down smoothly');
  page.frame({ advance: false });
  assert.equal(surface.draws, 1, 'an unchanged video frame is not drawn again');
  assert.equal(video.classList.contains('playing'), false, 'the video itself stays hidden');
  stop();
  assert.equal(surface.removed, true);
  assert.equal(video.removed, true);
});

test('TV mode keeps the last picture while the video restarts its loop', async () => {
  canvasPage();
  tv.animateCover(page.box, { artists: 'Artist loop', album: 'Album loop' }, { canvas: true });
  await tick();
  await tick();
  const [video] = page.videos;
  const surface = page.surface();
  video.dispatchEvent(new Event('playing'));
  page.pixel = 200;
  page.frame();
  assert.equal(surface.classList.contains('playing'), true);
  const shown = surface.draws;
  // The loop jumps back to the start: the TV decoder hands out black frames for a while.
  video.dispatchEvent(new Event('seeking'));
  page.pixel = 0;
  for (let i = 0; i < 300; i++) page.frame();
  assert.equal(surface.draws, shown, 'black frames after the jump are not drawn');
  assert.equal(surface.removed, undefined, 'no give-up once shown');
  page.pixel = 200;
  page.frame();
  assert.equal(surface.draws, shown + 1, 'drawing resumes with the first real frame');
  // Without a jump or stall, frames are drawn as they come (dark scenes included).
  page.pixel = 0;
  page.frame();
  assert.equal(surface.draws, shown + 2);
});

test('TV mode plays one fixed version with hls.js, so loops never switch quality', async () => {
  canvasPage();
  tv.animateCover(page.box, { artists: 'Artist level', album: 'Album level' }, { canvas: true });
  await tick();
  await tick();
  assert.equal(page.videos.length, 1, 'hls.js first, even where native HLS plays');
  assert.equal(page.hls.source, 'https://example.com/tv.m3u8');
  assert.equal(page.hls.config.capLevelToPlayerSize, false);
  page.hls.trigger(TvHls.Events.MANIFEST_PARSED);
  assert.equal(page.hls.currentLevel, 1, '768: the smallest that fills a 475 px cover');
});

test('TV mode falls back to native HLS when hls.js only draws black', async () => {
  canvasPage();
  tv.animateCover(page.box, { artists: 'Artist fallback', album: 'Album fallback' }, { canvas: true });
  await tick();
  await tick();
  const [hlsVideo] = page.videos;
  hlsVideo.dispatchEvent(new Event('playing'));
  for (let i = 0; i < 150; i++) page.frame(); // 5 s of black: still trying
  assert.ok(page.frameCallbacks.length > 0, 'still drawing after 5 s of black');
  for (let i = 0; i < 100; i++) page.frame();
  assert.equal(hlsVideo.removed, true);
  assert.equal(page.hls.destroyed, true);
  assert.equal(page.videos.length, 2);
  const native = page.videos[1];
  assert.equal(native.src, 'https://example.com/tv.m3u8');
  assert.equal(native.crossOrigin, 'anonymous');
  assert.equal(page.surface().removed, undefined, 'a fresh canvas for the native video');
});

test('TV mode keeps the still cover when every player only draws black', async () => {
  canvasPage({ hlsSupported: false });
  tv.animateCover(page.box, { artists: 'Artist black', album: 'Album black' }, { canvas: true });
  await tick();
  await tick();
  assert.equal(page.videos.length, 1, 'native HLS when hls.js is not supported');
  const [video] = page.videos;
  video.dispatchEvent(new Event('playing'));
  for (let i = 0; i < 1000 && page.frameCallbacks.length; i++) page.frame();
  assert.ok(page.canvases.filter((c) => page.box.children.includes(c)).every((c) => c.removed && !c.classList.contains('playing')));
  assert.equal(video.removed, true);
  assert.equal(page.frameCallbacks.length, 0, 'stopped drawing');
  assert.equal(page.videos.length, 1, 'nothing left to try');
});

test('TV mode streams the smallest cover version that still fills the canvas', () => {
  const levels = [{ height: 768 }, { height: 360 }, { height: 2160 }, { height: 1080 }];
  assert.equal(levelCap(levels, 500), 0); // 768
  assert.equal(levelCap(levels, 300), 1); // 360
  assert.equal(levelCap(levels, 900), 3); // 1080
  assert.equal(levelCap(levels, 4000), 2); // nothing big enough: the largest
  assert.equal(levelCap([], 500), -1); // no levels: no cap
});
