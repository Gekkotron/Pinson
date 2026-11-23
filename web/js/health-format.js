// Pure helpers for the Health page (`#/health`): relative times, uptime, yt-dlp version compare.

/** "just now" / "3 min ago" / "2 h ago" / "3 days ago" for an ISO time, against `now` (ms). */
export function relativeTime(iso, now = Date.now()) {
  const t = Date.parse(iso);
  if (Number.isNaN(t)) return '';
  const secs = Math.max(0, Math.floor((now - t) / 1000));
  if (secs < 60) return 'just now';
  const mins = Math.floor(secs / 60);
  if (mins < 60) return `${mins} min ago`;
  const hours = Math.floor(mins / 60);
  if (hours < 24) return `${hours} h ago`;
  const days = Math.floor(hours / 24);
  return `${days} ${days === 1 ? 'day' : 'days'} ago`;
}

/** 7500 → "2 h 5 min"; under an hour "5 min"; under a minute "under a minute"; a day or more "3 d 4 h". */
export function formatUptime(secs) {
  const total = Math.max(0, Math.floor(Number(secs) || 0));
  const mins = Math.floor(total / 60);
  if (mins < 1) return 'under a minute';
  const hours = Math.floor(mins / 60);
  if (hours < 1) return `${mins} min`;
  const days = Math.floor(hours / 24);
  if (days >= 1) return `${days} d ${hours % 24} h`;
  return `${hours} h ${mins % 60} min`;
}

function parts(v) {
  if (typeof v !== 'string' || !v) return null;
  const out = v.split('.').map((p) => (/^\d+$/.test(p) ? Number(p) : NaN));
  return out.some(Number.isNaN) ? null : out;
}

/** True when version `a` is strictly newer than `b` (dot-separated numeric parts; anything else is false). */
export function isNewer(a, b) {
  const pa = parts(a);
  const pb = parts(b);
  if (!pa || !pb) return false;
  for (let i = 0; i < Math.max(pa.length, pb.length); i++) {
    const x = pa[i] ?? 0;
    const y = pb[i] ?? 0;
    if (x !== y) return x > y;
  }
  return false;
}
