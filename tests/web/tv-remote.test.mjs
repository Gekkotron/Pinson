// Run with: node --test 'tests/web/*.test.mjs'
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { remoteAction } from '../../web/js/tv-remote.js';

const key = (k, keyCode, extra = {}) => remoteAction({ key: k, keyCode, ...extra });
const code = (keyCode) => remoteAction({ key: 'Unidentified', keyCode });

test('colour keys (Philips, LG, Samsung, HbbTV codes): red previous, green pause, yellow play, blue next', () => {
  assert.equal(code(403), 'previous');
  assert.equal(code(404), 'pause');
  assert.equal(code(405), 'play');
  assert.equal(code(406), 'next');
});

test('colour keys by name (browsers that report one)', () => {
  assert.equal(key('ColorF0Red', 403), 'previous');
  assert.equal(key('ColorF1Green', 404), 'pause');
  assert.equal(key('ColorF2Yellow', 405), 'play');
  assert.equal(key('ColorF3Blue', 406), 'next');
});

test('arrows, OK and channel keys', () => {
  assert.equal(key('ArrowUp', 38), 'bigger');
  assert.equal(key('ArrowDown', 40), 'smaller');
  assert.equal(key('ArrowRight', 39), 'next');
  assert.equal(key('ArrowLeft', 37), 'previous');
  assert.equal(key('Enter', 13), 'playPause');
  assert.equal(code(427), 'bigger');
  assert.equal(code(428), 'smaller');
});

test('media keys by name and by code', () => {
  assert.equal(key('MediaPlayPause', 179), 'playPause');
  assert.equal(key('MediaTrackNext', 176), 'next');
  assert.equal(key('MediaTrackPrevious', 177), 'previous');
  assert.equal(code(415), 'play');
  assert.equal(code(19), 'pause');
  assert.equal(code(425), 'next');
  assert.equal(code(424), 'previous');
  assert.equal(code(10252), 'playPause');
});

test('volume, letters, space and modified keys are left alone', () => {
  assert.equal(key('AudioVolumeUp', 175), null);
  assert.equal(key('a', 65), null);
  assert.equal(key(' ', 32), null);
  assert.equal(key('ArrowUp', 38, { ctrlKey: true }), null);
  assert.equal(code(461), null); // webOS Back: the browser's own
});
