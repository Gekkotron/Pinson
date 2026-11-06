// Recently played: plays grouped by the viewer's local day. Pure (no DOM, no imports) so
// `node --test tests/web/` can run it. Plays come from GET /api/history: {id, playedAt, song}.

const pad = (n) => String(n).padStart(2, '0');

/** The local calendar day of `date`, as "YYYY-MM-DD". */
function localKey(date) {
  return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())}`;
}

/** "Today", "Yesterday", or e.g. "Tuesday 3 November" (with the year when it isn't `now`'s). */
export function dayLabel(date, now = new Date()) {
  const key = localKey(date);
  if (key === localKey(now)) return 'Today';
  const yesterday = new Date(now.getFullYear(), now.getMonth(), now.getDate() - 1);
  if (key === localKey(yesterday)) return 'Yesterday';
  const opts = { weekday: 'long', day: 'numeric', month: 'long' };
  if (date.getFullYear() !== now.getFullYear()) opts.year = 'numeric';
  return date.toLocaleDateString('en-GB', opts);
}

/** The local time of a play, "HH:MM" (24-hour). */
export function playTime(date) {
  return date.toLocaleTimeString('en-GB', { hour: '2-digit', minute: '2-digit' });
}

/**
 * Groups plays (newest first, any number of pages concatenated) into local days, newest first:
 * [{key, label, plays}]. Within a day only the newest play of each song (by videoId) is kept.
 */
export function groupByDay(plays, now = new Date()) {
  const days = [];
  let day = null;
  for (const p of plays) {
    const date = new Date(p.playedAt);
    const key = localKey(date);
    if (!day || day.key !== key) {
      day = { key, label: dayLabel(date, now), plays: [], seen: new Set() };
      days.push(day);
    }
    if (day.seen.has(p.song.videoId)) continue;
    day.seen.add(p.song.videoId);
    day.plays.push(p);
  }
  return days.map(({ key, label, plays: dayPlays }) => ({ key, label, plays: dayPlays }));
}
