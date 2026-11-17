// Run with: node --test 'tests/web/*.test.mjs'
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { followCoverTint, tintVars, NEUTRAL } from '../../web/js/tint.js';

const fakeStore = () => {
  const fns = [];
  const store = {
    state: { current: null },
    on: (ev, fn) => (ev === 'state' && fns.push(fn), () => {}),
    play(artworkUrl) {
      store.state = { current: artworkUrl === null ? null : { song: { videoId: artworkUrl, artworkUrl } } };
      fns.forEach((f) => f(store.state));
    },
  };
  return store;
};
const fakeStyle = () => {
  const vars = {};
  return { vars, setProperty: (k, v) => (vars[k] = v) };
};
const deferred = () => {
  let resolve;
  const promise = new Promise((r) => (resolve = r));
  return { promise, resolve };
};
const tick = () => new Promise((r) => setTimeout(r, 0));

test('tintVars maps a cover colour onto the page variables', () => {
  assert.deepEqual(tintVars({ color: '#336699', glow: '#88aadd' }), {
    '--tint': '#336699',
    '--glow': '#88aadd73',
    '--chrome-tint': '#336699',
  });
});

test('tintVars without a colour is neutral, and the bars get no tint', () => {
  assert.deepEqual(tintVars(null), NEUTRAL);
  assert.equal(NEUTRAL['--chrome-tint'], 'transparent');
});

test('the cover colour of the current song lands on the page', async () => {
  const store = fakeStore();
  const style = fakeStyle();
  followCoverTint(store, async () => ({ color: '#112233', glow: '#445566' }), style);
  assert.equal(style.vars['--chrome-tint'], 'transparent'); // nothing playing
  store.play('a.jpg');
  await tick();
  assert.equal(style.vars['--tint'], '#112233');
  assert.equal(style.vars['--chrome-tint'], '#112233');
  store.play(null);
  assert.deepEqual(style.vars, { ...NEUTRAL });
});

test('a late colour for an older song is dropped', async () => {
  const store = fakeStore();
  const style = fakeStyle();
  const slow = deferred();
  followCoverTint(store, (url) => (url === 'old.jpg' ? slow.promise : Promise.resolve({ color: '#00ff00', glow: '#00ff00' })), style);
  store.play('old.jpg');
  store.play('new.jpg');
  await tick();
  slow.resolve({ color: '#ff0000', glow: '#ff0000' });
  await tick();
  assert.equal(style.vars['--tint'], '#00ff00');
});

test('the same cover again does not flash back to neutral', async () => {
  const store = fakeStore();
  const style = fakeStyle();
  let calls = 0;
  followCoverTint(store, async () => (calls++, { color: '#123456', glow: '#123456' }), style);
  store.play('a.jpg');
  await tick();
  store.state = { current: { song: { videoId: 'other', artworkUrl: 'a.jpg' } } };
  store.play('a.jpg');
  assert.equal(style.vars['--tint'], '#123456');
  assert.equal(calls, 1);
});
