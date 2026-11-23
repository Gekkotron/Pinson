// Run with: node --test 'tests/web/*.test.mjs'
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { nextRepeat, repeatLabel } from '../../web/js/repeat.js';

test('nextRepeat cycles off, all, one, off', () => {
  assert.equal(nextRepeat('off'), 'all');
  assert.equal(nextRepeat('all'), 'one');
  assert.equal(nextRepeat('one'), 'off');
});

test('nextRepeat treats an unknown mode as off', () => {
  assert.equal(nextRepeat('bogus'), 'all');
  assert.equal(nextRepeat(undefined), 'all');
});

test('repeatLabel names each mode', () => {
  assert.equal(repeatLabel('off'), 'Repeat off');
  assert.equal(repeatLabel('all'), 'Repeat all');
  assert.equal(repeatLabel('one'), 'Repeat one');
  assert.equal(repeatLabel(undefined), 'Repeat off');
});
