import { test } from 'node:test';
import assert from 'node:assert/strict';
import { standaloneHeight } from '../../web/js/standalone-viewport.js';

test('keeps the same height when the viewport changes by the top safe area', () => {
  assert.equal(standaloneHeight(874, 812, 62), 874);
  assert.equal(standaloneHeight(874, 874, 62), 874);
  assert.equal(standaloneHeight(874, 550, 62), null);
  assert.equal(standaloneHeight(874, 812, 0), null);
});
