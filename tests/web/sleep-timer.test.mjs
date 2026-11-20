// Run with: node --test 'tests/web/*.test.mjs'
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { minutesLeft, sleepDisplay, sleepEndsAt } from '../../web/js/sleep-timer.js';

test('sleepEndsAt: only a timed timer has an end', () => {
  assert.equal(sleepEndsAt(null, 1000), null);
  assert.equal(sleepEndsAt({ endOfSong: true }, 1000), null);
  assert.equal(sleepEndsAt({ remainingMs: 5000 }, 1000), 6000);
});

test('minutesLeft rounds up and stays at 1 minute or more', () => {
  assert.equal(minutesLeft(60_001, 0), 2);
  assert.equal(minutesLeft(60_000, 0), 1);
  assert.equal(minutesLeft(10, 0), 1);
  assert.equal(minutesLeft(0, 5000), 1);
});

test('sleepDisplay: off, end of song and minutes', () => {
  assert.equal(sleepDisplay(null, null, 0), null);
  assert.equal(sleepDisplay({ endOfSong: true }, null, 0).badge, 'end');
  const d = sleepDisplay({ remainingMs: 1380000 }, 1380000, 0);
  assert.equal(d.badge, '23');
  assert.equal(d.label, 'Sleep timer: 23 min left');
});
