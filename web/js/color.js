// Dominant cover colour, for the now-playing tint and glow.

import { artworkAt, artworkProxyUrl } from './api.js';

const cache = new Map(); // artworkUrl -> Promise<[r,g,b] | null>
const SIZE = 32;

/** Dominant colour of a cover (via the same-origin proxy and a 32×32 canvas), or null. Cached per URL. */
export function dominantColor(url) {
  if (!url) return Promise.resolve(null);
  if (!cache.has(url)) cache.set(url, extract(url).catch(() => null));
  return cache.get(url);
}

async function extract(url) {
  const img = new Image();
  img.decoding = 'async';
  img.src = artworkProxyUrl(artworkAt(url, 120));
  await img.decode();
  const canvas = document.createElement('canvas');
  canvas.width = canvas.height = SIZE;
  const ctx = canvas.getContext('2d', { willReadFrequently: true });
  ctx.drawImage(img, 0, 0, SIZE, SIZE);
  const px = ctx.getImageData(0, 0, SIZE, SIZE).data;

  // Group colourful pixels by hue (20° buckets), weighted towards saturated mid-tones, and
  // average the heaviest group. Mostly grey/black/white covers fall back to the mean colour.
  const buckets = Array.from({ length: 18 }, () => ({ w: 0, r: 0, g: 0, b: 0 }));
  const mean = [0, 0, 0];
  let n = 0;
  let colourful = 0;
  for (let i = 0; i < px.length; i += 4) {
    if (px[i + 3] < 128) continue;
    const rgb = [px[i], px[i + 1], px[i + 2]];
    mean[0] += rgb[0]; mean[1] += rgb[1]; mean[2] += rgb[2]; n++;
    const [hue, sat, light] = toHsl(rgb);
    if (sat < 0.25 || light < 0.1 || light > 0.92) continue;
    const w = sat * (1 - Math.abs(light - 0.5));
    const bucket = buckets[Math.floor(hue / 20) % 18];
    bucket.w += w; bucket.r += rgb[0] * w; bucket.g += rgb[1] * w; bucket.b += rgb[2] * w;
    colourful++;
  }
  if (!n) return null;
  if (colourful < n * 0.03) return mean.map((v) => Math.round(v / n));
  const best = buckets.reduce((a, b) => (b.w > a.w ? b : a));
  return [best.r / best.w, best.g / best.w, best.b / best.w].map(Math.round);
}

/** A background-safe version of `rgb` (kept dark and not too loud, so white text stays readable), as a CSS colour. */
export function tintColor(rgb) {
  if (!rgb) return null;
  const [h, s, l] = toHsl(rgb);
  const sat = Math.min(s, 0.62);
  const light = Math.max(0.2, Math.min(l, 0.36));
  return `hsl(${Math.round(h)} ${Math.round(sat * 100)}% ${Math.round(light * 100)}%)`;
}

function toHsl([r, g, b]) {
  r /= 255; g /= 255; b /= 255;
  const max = Math.max(r, g, b), min = Math.min(r, g, b);
  const l = (max + min) / 2;
  if (max === min) return [0, 0, l];
  const d = max - min;
  const s = l > 0.5 ? d / (2 - max - min) : d / (max + min);
  let h;
  if (max === r) h = (g - b) / d + (g < b ? 6 : 0);
  else if (max === g) h = (b - r) / d + 2;
  else h = (r - g) / d + 4;
  return [h * 60, s, l];
}
