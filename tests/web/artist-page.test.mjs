import { test } from 'node:test';
import assert from 'node:assert/strict';
import { TOP_SONGS, artistNameHref, visibleSongs } from '../../web/js/artist-page.js';

test('artistNameHref encodes the name', () => {
  assert.equal(artistNameHref('AC/DC & Co'), '#/artist-name/AC%2FDC%20%26%20Co');
});

test('visibleSongs shows the first ten until expanded', () => {
  const songs = Array.from({ length: 14 }, (_, i) => ({ videoId: 'v' + i }));
  assert.equal(visibleSongs(songs, false).length, TOP_SONGS);
  assert.equal(visibleSongs(songs, true).length, 14);
  assert.equal(visibleSongs(songs.slice(0, 3), false).length, 3);
});
