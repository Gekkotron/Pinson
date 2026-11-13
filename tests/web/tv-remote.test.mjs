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

// ---------- custom mapping (the "Set up your remote" steps) ----------
import { keyId, keyLabel, isIgnoredKey } from '../../web/js/tv-remote.js';

test('a key is identified by its name, or by its code when the TV gives no name', () => {
  assert.equal(keyId({ key: 'ColorF2Yellow', keyCode: 405 }), 'key:ColorF2Yellow');
  assert.equal(keyId({ key: 'Unidentified', keyCode: 405 }), 'code:405');
  assert.equal(keyId({ keyCode: 13 }), 'code:13');
});

test('the custom mapping wins over the built-in keys, which stay as a fallback', () => {
  const custom = { 'code:405': 'next', 'key:Enter': 'bigger' };
  assert.equal(remoteAction({ key: 'Unidentified', keyCode: 405 }, custom), 'next');
  assert.equal(remoteAction({ key: 'Enter', keyCode: 13 }, custom), 'bigger');
  assert.equal(remoteAction({ key: 'Unidentified', keyCode: 406 }, custom), 'next');
  assert.equal(remoteAction({ key: 'Unidentified', keyCode: 999 }, custom), null);
  assert.equal(remoteAction({ key: 'Unidentified', keyCode: 405, ctrlKey: true }, custom), null);
});

test('button labels read like the remote', () => {
  assert.equal(keyLabel({ key: 'Unidentified', keyCode: 403 }), 'red key (403)');
  assert.equal(keyLabel({ key: 'ColorF3Blue', keyCode: 406 }), 'blue key (406)');
  assert.equal(keyLabel({ key: 'ArrowUp', keyCode: 38 }), 'ArrowUp (38)');
  assert.equal(keyLabel({ key: 'Enter', keyCode: 13 }), 'OK / Enter (13)');
  assert.equal(keyLabel({ key: 'Unidentified', keyCode: 461 }), 'key 461');
});

test('volume and modifier keys are never mapped', () => {
  assert.equal(isIgnoredKey({ key: 'AudioVolumeUp', keyCode: 175 }), true);
  assert.equal(isIgnoredKey({ key: 'Unidentified', keyCode: 447 }), true);
  assert.equal(isIgnoredKey({ key: 'Shift', keyCode: 16 }), true);
  assert.equal(isIgnoredKey({ key: 'Unidentified', keyCode: 405 }), false);
});
