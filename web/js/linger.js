// Songs that just left a list stay on screen a few seconds, so a mis-tapped ♥ can be tapped again.

/**
 * `next` with the `lingering` songs (videoIds) of `shown` kept where they were: each goes right
 * after the song it followed in `shown`, or first if none. `shown` is what is on screen now.
 */
export function withLingering(shown, next, lingering) {
  const out = next.slice();
  const inOut = new Set(next.map((s) => s.videoId));
  let prevId = null; // the last song of `shown` that is in `out`
  for (const song of shown) {
    const id = song.videoId;
    if (!inOut.has(id) && lingering.has(id)) {
      const at = prevId ? out.findIndex((s) => s.videoId === prevId) + 1 : 0;
      out.splice(at, 0, song);
      inOut.add(id);
    }
    if (inOut.has(id)) prevId = id;
  }
  return out;
}

/**
 * A list whose removed songs stay `ms` before they go. `set(next)` takes the new list; `shown` is
 * what to show and `isLeaving(videoId)` tells a lingering song. `onChange` runs after each `set`
 * and when a song's time is up. `stop()` drops the timers.
 */
export function lingeringList(initial, ms, onChange) {
  let next = initial;
  let shown = initial;
  const leaving = new Map(); // videoId → timer
  const refresh = () => {
    shown = withLingering(shown, next, leaving);
    onChange();
  };
  return {
    get shown() {
      return shown;
    },
    isLeaving: (id) => leaving.has(id),
    set(list) {
      next = list;
      const ids = new Set(list.map((s) => s.videoId));
      for (const [id, timer] of leaving) {
        if (!ids.has(id)) continue;
        clearTimeout(timer); // back in the list
        leaving.delete(id);
      }
      for (const { videoId: id } of shown) {
        if (ids.has(id) || leaving.has(id)) continue;
        const timer = setTimeout(() => {
          leaving.delete(id);
          refresh();
        }, ms);
        leaving.set(id, timer);
      }
      refresh();
    },
    stop() {
      for (const timer of leaving.values()) clearTimeout(timer);
      leaving.clear();
    },
  };
}
