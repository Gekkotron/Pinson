// Run with: node --test 'tests/web/*.test.mjs'
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { orderIds } from '../../web/js/reorder.js';

const s = (videoId) => ({ videoId });

test('a plain reorder is kept as dropped', () => {
  assert.deepEqual(orderIds(['c', 'a', 'b'], [s('a'), s('b'), s('c')]), ['c', 'a', 'b']);
});

test('a lingering (no longer favorite) song is left out', () => {
  assert.deepEqual(orderIds(['b', 'x', 'a'], [s('a'), s('b')]), ['b', 'a']);
});

test('a favorite hearted elsewhere mid-drag (missing from the drop) goes first, like any new heart', () => {
  assert.deepEqual(orderIds(['b', 'a'], [s('z'), s('a'), s('b')]), ['z', 'b', 'a']);
});
