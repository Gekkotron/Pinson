// Repeat mode helpers: "off" | "all" | "one".

const NEXT = { off: 'all', all: 'one', one: 'off' };
const LABELS = { off: 'Repeat off', all: 'Repeat all', one: 'Repeat one' };

/** The mode a tap on the repeat button switches to (an unknown mode counts as off). */
export function nextRepeat(mode) {
  return NEXT[mode] || 'all';
}

/** Accessible name of the repeat button for `mode`. */
export function repeatLabel(mode) {
  return LABELS[mode] || LABELS.off;
}
