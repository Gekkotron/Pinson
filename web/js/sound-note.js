// The note shown while the browser blocks this device's sound, and the events that may unblock it.
// Pure (no browser globals) so node can test it.

import { deviceName } from './device-name.js';

// Events that count as a user gesture for audio. A touch's pointerdown does not: it is its
// pointerup, touchend or click that lets the page start sound.
export const UNLOCK_GESTURES = ['keydown', 'pointerup', 'touchend', 'click'];

/** What to ask for: a TV remote's OK, a tap on a touch screen, else either. */
export function soundNoteText(userAgent, coarsePointer) {
  if (deviceName(userAgent) === 'Pinson – TV') return 'Press OK to start sound';
  if (coarsePointer) return 'Tap to start sound';
  return 'Press OK or click to start sound';
}
