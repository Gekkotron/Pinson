// Run with: TZ=Europe/Paris node --test tests/web/
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { dayLabel, groupByDay, playTime } from '../../web/js/history-days.js';

const now = new Date('2026-11-06T12:00:00+01:00');
let nextId = 1000;
const play = (videoId, iso) => ({ id: nextId--, playedAt: iso, song: { videoId, title: videoId, artists: 'X' } });
const vids = (day) => day.plays.map((p) => p.song.videoId);

test('labels: Today, Yesterday, then the weekday and date', () => {
  const days = groupByDay(
    [play('A', '2026-11-06T10:00:00+01:00'), play('B', '2026-11-05T10:00:00+01:00'), play('C', '2026-11-03T10:00:00+01:00')],
    now,
  );
  assert.deepEqual(days.map((d) => d.label), ['Today', 'Yesterday', 'Tuesday 3 November']);
  assert.deepEqual(days.map((d) => d.key), ['2026-11-06', '2026-11-05', '2026-11-03']);
});

test('a date in another year carries the year', () => {
  assert.match(dayLabel(new Date('2025-12-24T10:00:00+01:00'), now), /2025/);
  assert.doesNotMatch(dayLabel(new Date('2026-10-24T10:00:00+02:00'), now), /2026/);
});

test('collapses repeats within a day, keeping the newest play', () => {
  const a1 = play('A', '2026-11-06T21:40:00+01:00');
  const b = play('B', '2026-11-06T21:34:00+01:00');
  const a2 = play('A', '2026-11-06T09:00:00+01:00');
  const [day] = groupByDay([a1, b, a2], now);
  assert.deepEqual(day.plays, [a1, b]);
});

test('a repeat on another day is kept', () => {
  const days = groupByDay([play('A', '2026-11-06T10:00:00+01:00'), play('A', '2026-11-05T10:00:00+01:00')], now);
  assert.deepEqual(days.map(vids), [['A'], ['A']]);
});

test('a repeat split across two pages shows once', () => {
  const page1 = [play('A', '2026-11-06T21:40:00+01:00'), play('B', '2026-11-06T21:34:00+01:00')];
  const page2 = [play('A', '2026-11-06T09:00:00+01:00'), play('C', '2026-11-06T08:00:00+01:00')];
  const days = groupByDay([...page1, ...page2], now);
  assert.equal(days.length, 1);
  assert.deepEqual(vids(days[0]), ['A', 'B', 'C']);
});

test('groups by local midnight, not UTC', () => {
  // 23:30 UTC on 5 Nov is 00:30 on 6 Nov in Paris.
  const [day] = groupByDay([play('A', '2026-11-05T23:30:00Z')], now);
  assert.equal(day.label, 'Today');
  assert.equal(day.key, '2026-11-06');
});

test('playTime is HH:MM, 24-hour, local', () => {
  assert.equal(playTime(new Date('2026-11-03T18:12:00Z')), '19:12');
  assert.equal(playTime(new Date('2026-11-03T08:05:00Z')), '09:05');
});

test('no plays, no days', () => {
  assert.deepEqual(groupByDay([], now), []);
});
