// Run with: node --test tests/web/
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { formatUptime, isNewer, relativeTime } from '../../web/js/health-format.js';

const now = Date.parse('2026-11-23T12:00:00Z');
const ago = (secs) => new Date(now - secs * 1000).toISOString();

test('relative time buckets', () => {
  assert.equal(relativeTime(ago(5), now), 'just now');
  assert.equal(relativeTime(ago(59), now), 'just now');
  assert.equal(relativeTime(ago(180), now), '3 min ago');
  assert.equal(relativeTime(ago(2 * 3600 + 60), now), '2 h ago');
  assert.equal(relativeTime(ago(86400), now), '1 day ago');
  assert.equal(relativeTime(ago(3 * 86400), now), '3 days ago');
  assert.equal(relativeTime(ago(-30), now), 'just now'); // clock skew: future times clamp
  assert.equal(relativeTime('nope', now), '');
});

test('uptime format', () => {
  assert.equal(formatUptime(10), 'under a minute');
  assert.equal(formatUptime(300), '5 min');
  assert.equal(formatUptime(2 * 3600 + 5 * 60), '2 h 5 min');
  assert.equal(formatUptime(3600), '1 h 0 min');
  assert.equal(formatUptime(3 * 86400 + 4 * 3600), '3 d 4 h');
  assert.equal(formatUptime(undefined), 'under a minute');
});

test('version compare matches the server rule', () => {
  assert.equal(isNewer('2026.10.26.1', '2026.10.26'), true);
  assert.equal(isNewer('2026.10.26', '2026.10.26.1'), false);
  assert.equal(isNewer('2026.11.2', '2026.10.26.1'), true);
  assert.equal(isNewer('2026.11.2', '2026.11.2'), false);
  assert.equal(isNewer('2026.11.10', '2026.11.9'), true); // numeric, not lexical
  assert.equal(isNewer('2026.11.2', null), false);
  assert.equal(isNewer('2026.11.2rc1', '2026.10.1'), false);
  assert.equal(isNewer('', '2026.10.1'), false);
});
