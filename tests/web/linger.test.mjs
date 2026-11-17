// Run with: node --test 'tests/web/*.test.mjs'
import { mock, test } from 'node:test';
import assert from 'node:assert/strict';
import { lingeringList, withLingering } from '../../web/js/linger.js';

const s = (videoId) => ({ videoId });
const ids = (songs) => songs.map((x) => x.videoId);

test('no lingering songs: the new list as is', () => {
  assert.deepEqual(ids(withLingering([s('a'), s('b')], [s('b'), s('c')], new Set())), ['b', 'c']);
});

test('a lingering song keeps its place', () => {
  const shown = [s('a'), s('b'), s('c')];
  assert.deepEqual(ids(withLingering(shown, [s('a'), s('c')], new Set(['b']))), ['a', 'b', 'c']);
});

test('a lingering first or last song keeps its place', () => {
  const shown = [s('a'), s('b'), s('c')];
  assert.deepEqual(ids(withLingering(shown, [s('b'), s('c')], new Set(['a']))), ['a', 'b', 'c']);
  assert.deepEqual(ids(withLingering(shown, [s('a'), s('b')], new Set(['c']))), ['a', 'b', 'c']);
});

test('several lingering songs in a row keep their order', () => {
  const shown = [s('a'), s('b'), s('c'), s('d')];
  assert.deepEqual(ids(withLingering(shown, [s('a'), s('d')], new Set(['b', 'c']))), ['a', 'b', 'c', 'd']);
});

test('a song added meanwhile goes where the new list puts it', () => {
  const shown = [s('a'), s('b'), s('c')];
  assert.deepEqual(ids(withLingering(shown, [s('n'), s('a'), s('c')], new Set(['b']))), ['n', 'a', 'b', 'c']);
});

test('the last favorite lingering alone', () => {
  assert.deepEqual(ids(withLingering([s('a')], [], new Set(['a']))), ['a']);
});

test('a song back in the list is not doubled', () => {
  const shown = [s('a'), s('b'), s('c')];
  assert.deepEqual(ids(withLingering(shown, [s('b'), s('a'), s('c')], new Set(['b']))), ['b', 'a', 'c']);
});

test('a lingering song whose time is up is dropped', () => {
  const shown = [s('a'), s('b'), s('c')];
  assert.deepEqual(ids(withLingering(shown, [s('a'), s('c')], new Set())), ['a', 'c']);
});

// ---------- lingeringList: the timers ----------

function setup(initial) {
  mock.timers.enable({ apis: ['setTimeout'] });
  let changes = 0;
  const list = lingeringList(initial, 4000, () => changes++);
  return { list, changes: () => changes };
}

test('a removed song stays 4 s, then goes', (t) => {
  t.after(() => mock.timers.reset());
  const { list, changes } = setup([s('a'), s('b'), s('c')]);
  list.set([s('a'), s('c')]);
  assert.deepEqual(ids(list.shown), ['a', 'b', 'c']);
  assert.ok(list.isLeaving('b'));
  mock.timers.tick(3999);
  assert.deepEqual(ids(list.shown), ['a', 'b', 'c']);
  mock.timers.tick(1);
  assert.deepEqual(ids(list.shown), ['a', 'c']);
  assert.ok(!list.isLeaving('b'));
  assert.equal(changes(), 2);
  mock.timers.tick(10000);
  assert.equal(changes(), 2, 'no timer left behind');
});

test('a song added back before its time is up stays for good', (t) => {
  t.after(() => mock.timers.reset());
  const { list, changes } = setup([s('a'), s('b')]);
  list.set([s('a')]);
  mock.timers.tick(1000);
  list.set([s('b'), s('a')]);
  assert.deepEqual(ids(list.shown), ['b', 'a']);
  assert.ok(!list.isLeaving('b'));
  mock.timers.tick(10000);
  assert.deepEqual(ids(list.shown), ['b', 'a']);
  assert.equal(changes(), 2);
});

test('each removed song has its own 4 s', (t) => {
  t.after(() => mock.timers.reset());
  const { list } = setup([s('a'), s('b'), s('c')]);
  list.set([s('a'), s('c')]);
  mock.timers.tick(2000);
  list.set([s('a')]);
  assert.deepEqual(ids(list.shown), ['a', 'b', 'c']);
  mock.timers.tick(2000);
  assert.deepEqual(ids(list.shown), ['a', 'c']);
  mock.timers.tick(2000);
  assert.deepEqual(ids(list.shown), ['a']);
});

test('stop clears the timers', (t) => {
  t.after(() => mock.timers.reset());
  const { list, changes } = setup([s('a'), s('b')]);
  list.set([s('a')]);
  list.stop();
  mock.timers.tick(10000);
  assert.equal(changes(), 1);
});
