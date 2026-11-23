// Run with: node --test tests/web/
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { formatBytes, offlineSentence, offlineWorking } from '../../web/js/offline-format.js';

const GB = 1024 ** 3;
const MB = 1024 ** 2;

test('formatBytes: MB under a gigabyte, GB with one decimal above', () => {
  assert.equal(formatBytes(0), '0 MB');
  assert.equal(formatBytes(undefined), '0 MB');
  assert.equal(formatBytes(640 * MB), '640 MB');
  assert.equal(formatBytes(1.2 * GB), '1.2 GB');
  assert.equal(formatBytes(20 * GB), '20 GB');
});

test('sentence: off', () => {
  assert.equal(offlineSentence({ enabled: false }), 'Play favorites without YouTube: keeps their audio on the server.');
  assert.equal(offlineSentence(null), 'Play favorites without YouTube: keeps their audio on the server.');
});

test('sentence: working, done, failures, full', () => {
  const base = { enabled: true, saved: 312, total: 540, bytes: 1.2 * GB, limitBytes: 20 * GB, full: false, downloading: null, failed: 0 };
  assert.equal(offlineSentence({ ...base, downloading: { videoId: 'a', title: 't' } }), 'Saving 312 of 540 · 1.2 GB');
  assert.equal(offlineSentence({ ...base, saved: 540, bytes: 4.1 * GB }), 'All 540 saved · 4.1 GB');
  assert.equal(offlineSentence({ ...base, saved: 538, failed: 2 }), '538 of 540 saved · 2 will retry later');
  assert.equal(offlineSentence({ ...base, saved: 480, full: true }), 'Storage limit reached (20 GB): 480 of 540 saved');
});

test('working: poll only while there is something to do', () => {
  const base = { enabled: true, saved: 1, total: 2, full: false, downloading: null };
  assert.equal(offlineWorking(base), true);
  assert.equal(offlineWorking({ ...base, saved: 2 }), false);
  assert.equal(offlineWorking({ ...base, full: true }), false);
  assert.equal(offlineWorking({ ...base, enabled: false }), false);
  assert.equal(offlineWorking({ ...base, saved: 2, downloading: { videoId: 'a' } }), true);
});
