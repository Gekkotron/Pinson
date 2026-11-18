// Run with: node --test 'tests/web/*.test.mjs'
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { dropIndex } from '../../web/js/reorder.js';

// Four 64 px rows, mids at 32, 96, 160, 224.
const mids = [32, 96, 160, 224];

test('a row dragged past the bottom (centre clamped onto the last mid) lands last', () => {
  assert.equal(dropIndex(mids, 0, 224), 3);
});

test('a row dragged past the top (centre clamped onto the first mid) lands first', () => {
  assert.equal(dropIndex(mids, 3, 32), 0);
});

test('a row left where it was stays put', () => {
  assert.equal(dropIndex(mids, 1, 96), 1);
});

test('crossing a neighbour’s mid swaps with it, either way', () => {
  assert.equal(dropIndex(mids, 1, 161), 2);
  assert.equal(dropIndex(mids, 1, 159), 1);
  assert.equal(dropIndex(mids, 2, 95), 1);
  assert.equal(dropIndex(mids, 2, 97), 2);
});
