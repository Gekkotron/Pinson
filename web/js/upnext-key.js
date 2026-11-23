// What the Up next list shows, as a string: the list is rebuilt only when this changes. A state
// message also comes for play/pause, seeks and position re-syncs, and a rebuild makes new <img>s
// that fade in again, so every cover would blink.

/** The current entry, the upcoming entries, the autoplay section and the crossfade row; not status or position. */
export function upNextKey(state) {
  return JSON.stringify([state.current, state.upcoming, state.autoplay, state.crossfadeSecs, state.crossfadeAvailable]);
}
