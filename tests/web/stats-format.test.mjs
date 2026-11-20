// Run with: TZ=Europe/Paris node --test 'tests/web/*.test.mjs'
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { barHeights, barLabel, emptyTitle, formatListened, isPeriod } from '../../web/js/stats-format.js';

test('listened time: minutes under an hour, rounded hours above', () => {
  assert.equal(formatListened(0), '0 min');
  assert.equal(formatListened(2700), '45 min');
  assert.equal(formatListened(3600), '≈ 1 h');
  assert.equal(formatListened(90120), '≈ 25 h');
  assert.equal(formatListened(3590), '≈ 1 h'); // 59.8 min rounds up to the hour
  assert.equal(formatListened(undefined), '0 min');
});

test('bar heights are relative to the tallest hour', () => {
  const counts = new Array(24).fill(0);
  counts[7] = 12;
  counts[20] = 6;
  const bars = barHeights(counts);
  assert.equal(bars.length, 24);
  assert.equal(bars[7], 100);
  assert.equal(bars[20], 50);
  assert.equal(bars[0], 0);
});

test('a rarely played hour stays visible; all zeros stay flat', () => {
  const counts = new Array(24).fill(0);
  counts[3] = 1;
  counts[4] = 1000;
  assert.equal(barHeights(counts)[3], 4);
  assert.deepEqual(barHeights(new Array(24).fill(0)), new Array(24).fill(0));
  assert.deepEqual(barHeights(null), new Array(24).fill(0));
});

test('bar label and period helpers', () => {
  assert.equal(barLabel(7, 12), '7 h: 12 plays');
  assert.equal(barLabel(0, 1), '0 h: 1 play');
  assert.equal(emptyTitle('week'), 'Nothing in the last 7 days');
  assert.equal(emptyTitle('all'), 'Nothing played yet');
  assert.ok(isPeriod('year'));
  assert.ok(!isPeriod('decade'));
});
