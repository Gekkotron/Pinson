// Sleep timer helpers (no DOM): the server sends `sleep` as null, { endOfSong: true } or
// { remainingMs }; the client counts down from the moment the state arrived.

/** Absolute end time (ms since epoch) of a timed sleep message, else null. */
export function sleepEndsAt(sleep, now = Date.now()) {
  return sleep && typeof sleep.remainingMs === 'number' ? now + sleep.remainingMs : null;
}

/** Whole minutes left, rounded up, never below 1 while the timer is running. */
export function minutesLeft(endsAt, now = Date.now()) {
  return Math.max(1, Math.ceil((endsAt - now) / 60000));
}

/** What the button shows for the current timer: `{ badge, label }`, or null when no timer is active. */
export function sleepDisplay(sleep, endsAt, now = Date.now()) {
  if (!sleep) return null;
  if (sleep.endOfSong) return { badge: 'end', label: 'Sleep timer: stops after this song' };
  if (endsAt == null) return null;
  const m = minutesLeft(endsAt, now);
  return { badge: String(m), label: `Sleep timer: ${m} min left` };
}

export const SLEEP_MINUTES = [15, 30, 45, 60, 90];
