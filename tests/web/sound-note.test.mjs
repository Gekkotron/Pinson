// Run with: node --test 'tests/web/*.test.mjs'
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { soundNoteText, UNLOCK_GESTURES } from '../../web/js/sound-note.js';

const PHONE = 'Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Mobile Safari/537.36';
const IPHONE = 'Mozilla/5.0 (iPhone; CPU iPhone OS 17_5 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Mobile/15E148 Safari/604.1';
const TV = 'Mozilla/5.0 (SMART-TV; Linux; Tizen 7.0) AppleWebKit/537.36 (KHTML, like Gecko) 94.0.4606.31/7.0 TV Safari/537.36';
const LAPTOP = 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36';

test('a touch screen is asked to tap, not to press OK', () => {
  assert.equal(soundNoteText(PHONE, true), 'Tap to start sound');
  assert.equal(soundNoteText(IPHONE, true), 'Tap to start sound');
});

test('a TV keeps "Press OK", even if it reports a coarse pointer', () => {
  assert.equal(soundNoteText(TV, false), 'Press OK to start sound');
  assert.equal(soundNoteText(TV, true), 'Press OK to start sound');
});

test('any other browser (a mouse, or a TV we do not recognise) gets both', () => {
  assert.equal(soundNoteText(LAPTOP, false), 'Press OK or click to start sound');
});

test('unlock waits for events that let a page start sound', () => {
  // A touch's pointerdown does not count as a user gesture for audio; its pointerup, touchend and click do.
  assert.ok(!UNLOCK_GESTURES.includes('pointerdown'));
  for (const type of ['keydown', 'pointerup', 'touchend', 'click']) assert.ok(UNLOCK_GESTURES.includes(type), type);
});
