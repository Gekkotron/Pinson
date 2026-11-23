import { test } from 'node:test';
import assert from 'node:assert/strict';
import { firstArtist, splitArtists } from '../../web/js/hidden.js';

test('splits on comma, ampersand, feat. and ft. only', () => {
  assert.deepEqual(splitArtists('A, B & C feat. D ft. E'), ['A', 'B', 'C', 'D', 'E']);
});

test('does not split on " x " or " and "', () => {
  assert.deepEqual(splitArtists('Bonobo x Totally Enormous'), ['Bonobo x Totally Enormous']);
  assert.deepEqual(splitArtists('Simon and Garfunkel'), ['Simon and Garfunkel']);
});

test('trims and drops empties', () => {
  assert.deepEqual(splitArtists('  A ,  , B  '), ['A', 'B']);
  assert.deepEqual(splitArtists(''), []);
  assert.deepEqual(splitArtists(undefined), []);
});

test('first artist', () => {
  assert.equal(firstArtist('Daft Punk & Pharrell Williams'), 'Daft Punk');
  assert.equal(firstArtist('Solo'), 'Solo');
  assert.equal(firstArtist(''), '');
});
