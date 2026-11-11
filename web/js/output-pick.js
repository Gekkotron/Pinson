// Pure rules for the "This device" output (no DOM, so node can test them).

/** True when `cur` (the server's current output) is the browser output of the device with this id. */
export function isThisDevice(cur, id) {
  return !!cur && cur.kind === 'browser' && cur.id === 'browser:' + id;
}

/** True when the music plays in a browser that is not this device. */
export function isOtherDevice(cur, id) {
  return !!cur && cur.kind === 'browser' && !isThisDevice(cur, id);
}

/** Is picker row `opt` the current output? `opt.other` marks the extra row for another browser. */
export function rowIsCurrent(cur, opt, id) {
  if (!cur) return false;
  if (opt.other) return true;
  if (opt.kind === 'browser') return isThisDevice(cur, id);
  return cur.kind === opt.kind;
}

/** The rows to show: the available outputs, plus the current one when it has no row (a fixed output, or another browser). */
export function pickerOptions(view, id) {
  const cur = view.current;
  if (!cur) return view.available;
  if (isOtherDevice(cur, id)) return [{ kind: 'browser', name: cur.name, needsAddress: false, other: true }, ...view.available];
  if (!view.available.some((o) => o.kind === cur.kind)) return [{ kind: cur.kind, name: cur.name, needsAddress: false }, ...view.available];
  return view.available;
}

/** The Stream URL output's link on this server (`origin` is the page's origin). */
export function streamLink(origin) {
  return origin.replace(/\/+$/, '') + '/stream.mp3';
}
