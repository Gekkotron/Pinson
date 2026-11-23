// Run with: node --test 'tests/web/*.test.mjs'
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { artworkAt, artworkFallback, rememberShown, shownSrc } from '../../web/js/artwork.js';

const HQ = 'https://i.ytimg.com/vi/GP7aP67qQjQ/hqdefault.jpg?sqp=-oaymwEWCJADEOEBIAQqCggAEOADGC0guwJIWg&rs=AMzJL3lq';
const LH3 = 'https://lh3.googleusercontent.com/abc=w544-h544-l90-rj';

test('Google covers are resized as before', () => {
  assert.equal(artworkAt(LH3, 120), 'https://lh3.googleusercontent.com/abc=w120-h120-l90-rj');
  assert.equal(artworkAt('https://yt3.googleusercontent.com/x=w60-h60', 1200), 'https://yt3.googleusercontent.com/x=w1200-h1200');
  assert.equal(artworkFallback(LH3), null);
});

test('a big video cover asks for the 1280x720 frame', () => {
  assert.equal(artworkAt(HQ, 1200), 'https://i.ytimg.com/vi/GP7aP67qQjQ/maxresdefault.jpg');
  assert.equal(artworkAt('https://i.ytimg.com/vi/ki0Ocze98U8/sddefault.jpg?sqp=x', 600), 'https://i.ytimg.com/vi/ki0Ocze98U8/maxresdefault.jpg');
});

test('a small video cover uses the bar-free 320x180 frame', () => {
  assert.equal(artworkAt(HQ, 120), 'https://i.ytimg.com/vi/GP7aP67qQjQ/mqdefault.jpg');
  assert.equal(artworkAt(HQ, 320), 'https://i.ytimg.com/vi/GP7aP67qQjQ/mqdefault.jpg');
});

test('the fallback for a missing 1280x720 frame is the 320x180 one', () => {
  assert.equal(artworkFallback('https://i.ytimg.com/vi/ki0Ocze98U8/maxresdefault.jpg'), 'https://i.ytimg.com/vi/ki0Ocze98U8/mqdefault.jpg');
  assert.equal(artworkFallback('https://i.ytimg.com/vi/ki0Ocze98U8/mqdefault.jpg'), null);
});

test('other or empty URLs are left alone', () => {
  for (const u of ['https://i.ytimg.com/an_webp/x/mqdefault_6s.webp', 'https://example.com/a.jpg', '', null]) {
    assert.equal(artworkAt(u, 1200), u);
    assert.equal(artworkFallback(u), null);
  }
});

test('a cover shown once is remembered with the URL that loaded', () => {
  const big = 'https://i.ytimg.com/vi/ki0Ocze98U8/maxresdefault.jpg';
  const small = 'https://i.ytimg.com/vi/ki0Ocze98U8/mqdefault.jpg';
  assert.equal(shownSrc(big), null);
  rememberShown(big, small); // the 1280x720 frame was missing
  assert.equal(shownSrc(big), small);
  rememberShown(LH3, LH3);
  assert.equal(shownSrc(LH3), LH3);
  rememberShown('', LH3);
  assert.equal(shownSrc(''), null);
});
