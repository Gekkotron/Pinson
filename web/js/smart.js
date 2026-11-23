// Pure helpers for the smart playlists on the Library page.

const ICONS = { 'on-repeat': 'repeat', 'forgotten-favorites': 'heart', 'heard-once': 'history' };

/** The icon name for a smart list id (a note for an unknown one). */
export function smartIcon(id) {
  return ICONS[id] || 'music';
}
