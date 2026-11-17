// Cover colour of the current song, shared by the whole page: Now playing's background and glow
// (--tint, --glow) and the sidebar / player bar wash (--chrome-tint). Set on <html> so every
// page and the bars see the same colour; the transitions live in app.css.

/** The page with no cover colour: grey Now playing background, no glow, no wash on the bars. */
export const NEUTRAL = Object.freeze({
  '--tint': 'hsl(240 4% 24%)',
  '--glow': 'rgb(0 0 0 / 0%)',
  '--chrome-tint': 'transparent',
});

/** CSS variables for a `{color, glow}` cover colour, or NEUTRAL for null. */
export function tintVars(c) {
  if (!c) return { ...NEUTRAL };
  return { '--tint': c.color, '--glow': c.glow + '73', '--chrome-tint': c.color };
}

/**
 * Keeps `style` (normally document.documentElement.style) on the current song's cover colour.
 * `colorOf(url)` resolves `{color, glow}` or null (see color.js).
 */
export function followCoverTint(store, colorOf, style) {
  let shown; // artworkUrl the variables belong to (null = nothing playing)
  const apply = (vars) => Object.entries(vars).forEach(([k, v]) => style.setProperty(k, v));
  const paint = () => {
    const cur = store.state.current;
    const url = cur ? cur.song.artworkUrl || null : null;
    if (url === shown) return;
    shown = url;
    apply(NEUTRAL);
    if (!url) return;
    colorOf(url).then((c) => {
      if (shown === url && c) apply(tintVars(c));
    });
  };
  paint();
  return store.on('state', paint);
}
