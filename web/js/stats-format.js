// Pure helpers for the "Your stats" page.

export const PERIODS = [
  { key: 'week', label: '7 days', empty: 'Nothing in the last 7 days' },
  { key: 'month', label: '30 days', empty: 'Nothing in the last 30 days' },
  { key: 'year', label: '12 months', empty: 'Nothing in the last 12 months' },
  { key: 'all', label: 'All time', empty: 'Nothing played yet' },
];
export const DEFAULT_PERIOD = 'month';

export const isPeriod = (key) => PERIODS.some((p) => p.key === key);

/** Title for a period with no plays. */
export function emptyTitle(key) {
  return (PERIODS.find((p) => p.key === key) || PERIODS[3]).empty;
}

/** 90120 → "≈ 25 h"; 2700 → "45 min"; 0 → "0 min". */
export function formatListened(secs) {
  const minutes = Math.round(Math.max(0, Number(secs) || 0) / 60);
  if (minutes < 60) return `${minutes} min`;
  return `≈ ${Math.round(minutes / 60)} h`;
}

/** 24 counts → 24 bar heights in percent of the tallest (0 when nothing was played). */
export function barHeights(byHour) {
  const counts = Array.from({ length: 24 }, (_, i) => Math.max(0, Number(byHour && byHour[i]) || 0));
  const max = Math.max(...counts);
  if (max === 0) return counts.map(() => 0);
  return counts.map((c) => (c === 0 ? 0 : Math.max(4, (c / max) * 100)));
}

/** Accessible label for one bar: "7 h: 12 plays". */
export function barLabel(hour, plays) {
  return `${hour} h: ${plays} ${plays === 1 ? 'play' : 'plays'}`;
}
