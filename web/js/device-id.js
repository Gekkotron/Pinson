// This browser's identity as a "This device" output: a random id kept across visits, and a readable name.

import { deviceName } from './device-name.js';

const KEY = 'pinson.deviceId';
let memory = null; // fallback when storage throws

function randomId() {
  const bytes = new Uint8Array(8);
  crypto.getRandomValues(bytes);
  return Array.from(bytes, (b) => b.toString(16).padStart(2, '0')).join('');
}

/** 16 hex characters, stable per browser profile. */
export function deviceId() {
  try {
    let id = localStorage.getItem(KEY);
    if (!id) {
      id = memory || randomId();
      localStorage.setItem(KEY, id);
    }
    return id;
  } catch (_) {
    return (memory ||= randomId());
  }
}

/** "Chrome on Android": the device name without Nidoo's "Pinson – " prefix. */
export function outputName(userAgent) {
  return deviceName(userAgent).replace(/^Pinson – /, '');
}
