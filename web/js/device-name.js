// The name this browser shows in Nidoo's speaker list. Pure (no browser globals) so node can test it.

const TV = /SMART-?TV|Tizen|Web0S|webOS|NetCast|BRAVIA|Android TV|GoogleTV|\bAFT[A-Z0-9]+\b|HbbTV|VIDAA|CrKey/i;

// First match wins: Edge and Opera also say "Chrome", and Chrome also says "Safari".
const BROWSERS = [
  [/Edg(?:e|A|iOS)?\//, 'Edge'],
  [/OPR\/|Opera/, 'Opera'],
  [/Firefox\/|FxiOS\//, 'Firefox'],
  [/Chrome\/|CriOS\//, 'Chrome'],
  [/Safari\//, 'Safari'],
];

// iPhone/iPad UAs also say "Mac OS X", Android UAs also say "Linux", ChromeOS UAs also say "Linux".
const SYSTEMS = [
  [/iPhone|iPod/, 'iPhone'],
  [/iPad/, 'iPad'],
  [/Android/, 'Android'],
  [/CrOS/, 'ChromeOS'],
  [/Windows/, 'Windows'],
  [/Macintosh|Mac OS X/, 'Mac'],
  [/Linux|X11/, 'Linux'],
];

const find = (table, ua) => {
  const hit = table.find(([re]) => re.test(ua));
  return hit ? hit[1] : null;
};

/** "Pinson – TV" for a TV browser, else "Pinson – Chrome on Mac"; "Pinson – Browser" when nothing is recognised. */
export function deviceName(userAgent) {
  const ua = String(userAgent || '');
  if (TV.test(ua)) return 'Pinson – TV';
  const browser = find(BROWSERS, ua);
  const system = find(SYSTEMS, ua);
  if (!browser) return 'Pinson – Browser';
  return system ? `Pinson – ${browser} on ${system}` : `Pinson – ${browser}`;
}
