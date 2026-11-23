import { test } from 'node:test';
import assert from 'node:assert/strict';
import { smartIcon } from '../../web/js/smart.js';

test('each smart list has its own icon', () => {
  assert.equal(smartIcon('on-repeat'), 'repeat');
  assert.equal(smartIcon('forgotten-favorites'), 'heart');
  assert.equal(smartIcon('heard-once'), 'history');
});

test('an unknown list falls back to the music icon', () => {
  assert.equal(smartIcon('nope'), 'music');
});
