// Animated Apple Music covers are optional: the YouTube artwork stays underneath.
const lookups = new Map();
let hlsLoading;

function lookup(artist, album) {
  const key = JSON.stringify([artist, album]);
  if (!lookups.has(key)) {
    const params = new URLSearchParams({ artist, album });
    const pending = fetch('/api/artwork/animated?' + params)
      .then((r) => { if (!r.ok) throw new Error('Artwork lookup failed'); return r.json(); })
      .then((data) => data.url || null)
      .catch(() => { lookups.delete(key); return null; });
    lookups.set(key, pending);
  }
  return lookups.get(key);
}

function loadHls() {
  if (!hlsLoading) hlsLoading = new Promise((resolve, reject) => {
    const script = document.createElement('script');
    script.src = '/vendor/hls-1.6.15.min.js';
    script.onload = () => resolve(window.Hls);
    script.onerror = reject;
    document.head.append(script);
  }).catch(() => { hlsLoading = null; return null; });
  return hlsLoading;
}

export function animateCover(box, song) {
  let stopped = false;
  let hls;
  let video;
  const motion = window.matchMedia('(prefers-reduced-motion: reduce)');
  const stop = () => {
    stopped = true;
    if (hls) hls.destroy();
    if (video) { video.pause(); video.removeAttribute('src'); video.load(); video.remove(); }
    motion.removeEventListener?.('change', onMotion);
  };
  const onMotion = () => { if (motion.matches) stop(); };
  motion.addEventListener?.('change', onMotion);
  if (!motion.matches && song.artists && song.album) {
    (async () => {
      const url = await lookup(song.artists, song.album);
      if (!url || stopped) return;
      video = document.createElement('video');
      video.className = 'animated-cover';
      video.muted = true;
      video.loop = true;
      video.playsInline = true;
      video.setAttribute('aria-hidden', 'true');
      video.addEventListener('playing', () => video?.classList.add('playing'), { once: true });
      video.addEventListener('error', () => { console.debug('Animated artwork video error:', video?.error?.message); video?.remove(); });
      box.append(video);
      if (video.canPlayType('application/vnd.apple.mpegurl')) video.src = url;
      else {
        const Hls = await loadHls();
        if (stopped) return;
        if (!Hls || !Hls.isSupported()) { video.remove(); return; }
        hls = new Hls({ capLevelToPlayerSize: true });
        hls.on(Hls.Events.ERROR, (_event, data) => { if (data.fatal) { console.debug('Animated artwork HLS error:', data.details); hls.destroy(); hls = null; video.remove(); } });
        hls.loadSource(url);
        hls.attachMedia(video);
      }
      video.play().catch((error) => { console.debug('Animated artwork playback failed:', error.message); video?.remove(); });
    })();
  }
  return stop;
}
