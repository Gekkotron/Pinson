// Run with: node --test 'tests/web/*.test.mjs'
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { reduce } from '../../web/js/device-player.js';
import { outputName } from '../../web/js/device-id.js';

const fresh = (over = {}) => ({
  deviceId: 'dev12345',
  generation: null,
  startMs: 0,
  chainedAfter: null,
  lastPositionMs: 0,
  errored: false,
  wantPlaying: false,
  retried: false,
  ...over,
});
const kinds = (r) => r.actions.map((a) => a.do);
const load = (generation, over = {}) => ({
  type: 'device.load',
  generation,
  url: `/api/device/stream/${generation}?device=dev12345&fromMs=0`,
  startMs: 0,
  chained: false,
  paused: false,
  ...over,
});

test('ended chains to next', () => {
  const r = reduce(fresh({ generation: 3 }), { type: 'ended' });
  assert.deepEqual(kinds(r), ['load']);
  assert.ok(r.actions[0].url.includes('/api/device/next?'));
  assert.ok(r.actions[0].url.includes('after=3'));
  assert.equal(r.local.generation, null);
  assert.equal(r.local.chainedAfter, 3);
});

test('chained load adopts generation', () => {
  const chained = reduce(fresh({ generation: 3 }), { type: 'ended' }).local;
  const r = reduce(chained, { type: 'device.load', generation: 4, chained: true, startMs: 0 });
  assert.ok(kinds(r).includes('adopt'));
  assert.ok(!kinds(r).includes('load'));
  assert.equal(r.local.generation, 4);
  assert.equal(r.local.chainedAfter, null);
});

test('chained load for same generation adopts without reload', () => {
  const r = reduce(fresh({ generation: 4, startMs: 1500 }), load(4, { chained: true, startMs: 9000 }));
  assert.ok(kinds(r).includes('adopt'));
  assert.ok(!kinds(r).includes('load'));
  assert.equal(r.local.startMs, 1500);
});

test('adopt honours paused and plays when the element is paused', () => {
  const local = fresh({ generation: 4 });
  const paused = reduce(local, load(4, { chained: true, paused: true }));
  assert.ok(kinds(paused).includes('pause'));
  const play = reduce(local, load(4, { chained: true, elementPaused: true }));
  assert.ok(kinds(play).includes('play'));
  const running = reduce(local, load(4, { chained: true, elementPaused: false }));
  assert.ok(!kinds(running).includes('play'));
});

test('chained load with nothing loaded loads url', () => {
  const r = reduce(fresh(), load(6, { chained: true, startMs: 2000 }));
  assert.deepEqual(kinds(r), ['load']);
  assert.ok(r.actions[0].url.includes('/api/device/stream/6'));
  assert.equal(r.local.generation, 6);
  assert.equal(r.local.startMs, 2000);
});

test('unchained load during chain reloads', () => {
  const chained = reduce(fresh({ generation: 3 }), { type: 'ended' }).local;
  const r = reduce(chained, load(5));
  assert.deepEqual(kinds(r), ['load']);
  assert.ok(r.actions[0].url.includes('/api/device/stream/5'));
  assert.equal(r.local.chainedAfter, null);
  assert.equal(r.local.generation, 5);
});

test('paused load does not play', () => {
  const r = reduce(fresh(), load(2, { paused: true }));
  assert.deepEqual(kinds(r), ['load']);
  assert.equal(r.actions[0].paused, true);
});

test('resume after error reloads at position', () => {
  const r = reduce(fresh({ generation: 4, lastPositionMs: 61000, errored: true }), { type: 'device.resume' });
  assert.deepEqual(kinds(r), ['load', 'play']);
  assert.ok(r.actions[0].url.includes('/api/device/stream/4'));
  assert.ok(r.actions[0].url.includes('fromMs=61000'));
  assert.equal(r.local.startMs, 61000);
  assert.equal(r.local.errored, false);
});

test('resume with a source just plays', () => {
  const r = reduce(fresh({ generation: 4 }), { type: 'device.resume', hasSource: true });
  assert.deepEqual(kinds(r), ['play']);
});

test('stop stops and resets', () => {
  const r = reduce(fresh({ generation: 4, startMs: 5, chainedAfter: 2, errored: true }), { type: 'device.stop' });
  assert.deepEqual(kinds(r), ['stop']);
  assert.equal(r.local.generation, null);
  assert.equal(r.local.startMs, 0);
  assert.equal(r.local.chainedAfter, null);
  assert.equal(r.local.errored, false);
});

test('first error reloads at position instead of failing', () => {
  const playing = fresh({ generation: 4, startMs: 1000, lastPositionMs: 61000, wantPlaying: true });
  const r = reduce(playing, { type: 'error', reason: 'boom' });
  assert.deepEqual(kinds(r), ['load']);
  assert.ok(r.actions[0].url.includes('/api/device/stream/4'));
  assert.ok(r.actions[0].url.includes('fromMs=61000'));
  assert.equal(r.actions[0].paused, false);
  assert.equal(r.local.retried, true);
  assert.equal(r.local.errored, false);
  assert.equal(r.local.startMs, 61000);
  const paused = reduce(fresh({ generation: 4, lastPositionMs: 5, wantPlaying: false }), { type: 'error' });
  assert.equal(paused.actions[0].paused, true);
});

test('second error sends device.failed', () => {
  const known = reduce(fresh({ generation: 4, retried: true }), { type: 'error', reason: 'boom' });
  assert.equal(known.local.errored, true);
  assert.deepEqual(known.actions, [{ do: 'send', msg: { type: 'device.failed', generation: 4, reason: 'boom' } }]);
  assert.deepEqual(reduce(fresh(), { type: 'error' }).actions, []);
});

test('position clears retried; a new load too', () => {
  assert.equal(reduce(fresh({ generation: 4, retried: true }), { type: 'position', currentTimeMs: 1 }).local.retried, false);
  assert.equal(reduce(fresh({ generation: 4, retried: true }), load(5)).local.retried, false);
});

test('wantPlaying follows load, play, pause, stop and blocked', () => {
  assert.equal(reduce(fresh(), load(2)).local.wantPlaying, true);
  assert.equal(reduce(fresh(), load(2, { paused: true })).local.wantPlaying, false);
  assert.equal(reduce(fresh({ wantPlaying: true }), { type: 'device.pause' }).local.wantPlaying, false);
  assert.equal(reduce(fresh({ generation: 1 }), { type: 'device.resume', hasSource: true }).local.wantPlaying, true);
  assert.equal(reduce(fresh({ wantPlaying: true }), { type: 'device.stop' }).local.wantPlaying, false);
  assert.equal(reduce(fresh({ wantPlaying: true }), { type: 'blocked' }).local.wantPlaying, false);
});

test('ended twice without adopt keeps chaining', () => {
  const first = reduce(fresh({ generation: 3 }), { type: 'ended' }).local;
  const second = reduce(first, { type: 'ended' });
  assert.deepEqual(kinds(second), ['load']);
  assert.ok(second.actions[0].url.includes('/api/device/next?'));
  assert.ok(!second.actions[0].url.includes('after='));
  assert.equal(second.local.chainedAfter, 3);
  const adopted = reduce(second.local, { type: 'device.load', generation: 6, chained: true, startMs: 0 });
  assert.ok(kinds(adopted).includes('adopt'));
});

test('position reports never carry a null generation', () => {
  const r = reduce(fresh({ generation: 4, startMs: 1000 }), { type: 'position', currentTimeMs: 2500 });
  assert.equal(r.local.lastPositionMs, 3500);
  assert.deepEqual(r.actions, [{ do: 'send', msg: { type: 'device.position', generation: 4, positionMs: 3500 } }]);
  assert.deepEqual(reduce(fresh(), { type: 'position', currentTimeMs: 10 }).actions, []);
});

test('blocked play pauses and arms a tap', () => {
  const r = reduce(fresh({ generation: 4 }), { type: 'blocked' });
  assert.deepEqual(r.actions, [{ do: 'pauseServer' }]);
});

test('outputName drops the Pinson prefix', () => {
  const pixel =
    'Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Mobile Safari/537.36';
  assert.equal(outputName(pixel), 'Chrome on Android');
});
