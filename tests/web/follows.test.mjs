// Run with: node --test tests/web/
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { kindLabel, newBadge, releaseSubtitle } from '../../web/js/follows.js';

test('release subtitle: artist, kind and year', () => {
  assert.equal(releaseSubtitle({ artistName: 'Daft Punk', kind: 'single', year: '2026' }), 'Daft Punk · Single · 2026');
  assert.equal(releaseSubtitle({ artistName: 'Daft Punk', kind: 'album', year: '2026' }), 'Daft Punk · Album · 2026');
});

test('release subtitle leaves out missing parts', () => {
  assert.equal(releaseSubtitle({ artistName: 'Daft Punk', kind: 'album', year: null }), 'Daft Punk · Album');
  assert.equal(releaseSubtitle({ kind: 'single', year: '2025' }), 'Single · 2025');
});

test('unknown kinds read as albums', () => {
  assert.equal(kindLabel('ep'), 'Album');
  assert.equal(kindLabel('single'), 'Single');
});

test('new badge only for a positive count', () => {
  assert.equal(newBadge(3), '3 new');
  assert.equal(newBadge(1), '1 new');
  assert.equal(newBadge(0), '');
  assert.equal(newBadge(undefined), '');
  assert.equal(newBadge(-2), '');
});
