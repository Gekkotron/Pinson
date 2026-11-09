// Run with: node --test 'tests/web/*.test.mjs'
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { deviceName } from '../../web/js/device-name.js';

const UA = {
  chromeMac: 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36',
  safariIphone: 'Mozilla/5.0 (iPhone; CPU iPhone OS 17_5 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Mobile/15E148 Safari/604.1',
  firefoxWin: 'Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:132.0) Gecko/20100101 Firefox/132.0',
  chromeAndroid: 'Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Mobile Safari/537.36',
  edgeWin: 'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36 Edg/130.0.0.0',
  chromeIos: 'Mozilla/5.0 (iPhone; CPU iPhone OS 17_5 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) CriOS/130.0.0.0 Mobile/15E148 Safari/604.1',
  chromeLinux: 'Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36',
  safariIpad: 'Mozilla/5.0 (iPad; CPU OS 17_5 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Mobile/15E148 Safari/604.1',
};

test('desktop and phone browsers: "<Browser> on <OS>"', () => {
  assert.equal(deviceName(UA.chromeMac), 'Pinson – Chrome on Mac');
  assert.equal(deviceName(UA.safariIphone), 'Pinson – Safari on iPhone');
  assert.equal(deviceName(UA.firefoxWin), 'Pinson – Firefox on Windows');
  assert.equal(deviceName(UA.chromeAndroid), 'Pinson – Chrome on Android');
  assert.equal(deviceName(UA.safariIpad), 'Pinson – Safari on iPad');
  assert.equal(deviceName(UA.chromeLinux), 'Pinson – Chrome on Linux');
});

test('Edge and Chrome on iOS are not mistaken for Chrome or Safari', () => {
  assert.equal(deviceName(UA.edgeWin), 'Pinson – Edge on Windows');
  assert.equal(deviceName(UA.chromeIos), 'Pinson – Chrome on iPhone');
});

test('TV browsers are all "Pinson – TV"', () => {
  const tvs = [
    'Mozilla/5.0 (SMART-TV; Linux; Tizen 7.0) AppleWebKit/537.36 (KHTML, like Gecko) 94.0.4606.31/7.0 TV Safari/537.36',
    'Mozilla/5.0 (Web0S; Linux/SmartTV) AppleWebKit/537.36 Chrome/108.0.0.0 Safari/537.36 WebAppManager',
    'Mozilla/5.0 (Linux; NetCast; U) AppleWebKit/537.31 (KHTML, like Gecko) Chrome/26.0.1410.0 Safari/537.31 SmartTV/6.0',
    'Mozilla/5.0 (Linux; Android 9; BRAVIA 4K GB) AppleWebKit/537.36 Chrome/120.0.0.0 Safari/537.36',
    'Mozilla/5.0 (Linux; Android 11; Android TV) AppleWebKit/537.36 Chrome/120.0.0.0 Safari/537.36',
    'Mozilla/5.0 (Linux; Android 10; GoogleTV) AppleWebKit/537.36 Chrome/120.0.0.0 Safari/537.36',
    'Mozilla/5.0 (Linux; Android 9; AFTMM Build/PS7233) AppleWebKit/537.36 Chrome/88.0 Mobile Safari/537.36',
    'Mozilla/5.0 (Linux; HbbTV/1.5.1 (+DRM; Samsung; SmartTV2020;)) AppleWebKit/537.36',
    'Mozilla/5.0 (Linux; VIDAA 6) AppleWebKit/537.36 Chrome/94.0.4606.74 Safari/537.36',
    'Mozilla/5.0 (X11; Linux armv7l) AppleWebKit/537.36 Chrome/105.0.0.0 Safari/537.36 CrKey/1.56.500000',
  ];
  for (const ua of tvs) assert.equal(deviceName(ua), 'Pinson – TV', ua);
});

test('unknown or empty user agents fall back to "Pinson – Browser"', () => {
  assert.equal(deviceName(''), 'Pinson – Browser');
  assert.equal(deviceName(undefined), 'Pinson – Browser');
  assert.equal(deviceName('curl/8.0'), 'Pinson – Browser');
});

test('a known browser on an unknown system drops the "on"', () => {
  assert.equal(deviceName('Mozilla/5.0 (Plan9) Firefox/130.0'), 'Pinson – Firefox');
});
