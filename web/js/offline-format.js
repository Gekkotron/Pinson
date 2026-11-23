// Pure text helpers for the Favorites "Keep on the server" row.

const MB = 1024 * 1024;
const GB = 1024 * MB;

/** "640 MB", "1.2 GB", "20 GB"; 1024-based. */
export function formatBytes(n) {
  const bytes = Number.isFinite(n) && n > 0 ? n : 0;
  if (bytes >= GB) return `${(bytes / GB).toFixed(1).replace(/\.0$/, '')} GB`;
  return `${Math.round(bytes / MB)} MB`;
}

export const OFFLINE_OFF_TEXT = 'Play favorites without YouTube: keeps their audio on the server.';

/** The status sentence for an `/api/offline` status object. */
export function offlineSentence(status) {
  if (!status || !status.enabled) return OFFLINE_OFF_TEXT;
  const { saved = 0, total = 0, bytes = 0, limitBytes = null, full = false, downloading = null, failed = 0 } = status;
  if (full) return `Storage limit reached (${formatBytes(limitBytes)}): ${saved} of ${total} saved`;
  if (!downloading && failed > 0) return `${saved} of ${total} saved · ${failed} will retry later`;
  if (total === 0) return 'No favorites to save yet';
  if (saved >= total && !downloading) return `All ${total} saved · ${formatBytes(bytes)}`;
  return `Saving ${saved} of ${total} · ${formatBytes(bytes)}`;
}

/** True while the page should keep polling the status. */
export function offlineWorking(status) {
  return !!status && status.enabled && (!!status.downloading || (status.saved < status.total && !status.full));
}
