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
  const clearVideo = () => {
    const previous = video;
    video = null;
    if (hls) {
      hls.destroy();
      hls = null;
    }
    if (previous) {
      previous.pause();
      previous.removeAttribute('src');
      previous.load();
      previous.remove();
    }
  };
  const stop = () => {
    stopped = true;
    clearVideo();
    motion.removeEventListener?.('change', onMotion);
  };
  const onMotion = () => { if (motion.matches) stop(); };
  motion.addEventListener?.('change', onMotion);
  if (!motion.matches && song.artists && song.album) {
    (async () => {
      const url = await lookup(song.artists, song.album);
      if (!url || stopped) return;
      const createVideo = () => {
        const element = document.createElement('video');
        video = element;
        element.className = 'animated-cover';
        element.muted = true;
        element.loop = true;
        element.playsInline = true;
        element.setAttribute('aria-hidden', 'true');
        element.addEventListener('playing', () => element.classList.add('playing'), { once: true });
        box.append(element);
        return element;
      };
      const play = (element, onFailure = clearVideo) => {
        const fail = (reason) => {
          if (video !== element) return;
          console.debug('Animated artwork playback failed:', reason);
          onFailure();
        };
        element.addEventListener('error', () => fail(element.error?.message), { once: true });
        element.play().catch((error) => fail(error.message));
      };
      const playHls = async () => {
        const Hls = await loadHls();
        if (stopped || !Hls?.isSupported()) return;
        const element = createVideo();
        hls = new Hls({ capLevelToPlayerSize: true });
        hls.on(Hls.Events.ERROR, (_event, data) => {
          if (video !== element || !data.fatal) return;
          console.debug('Animated artwork HLS error:', data.details);
          clearVideo();
        });
        hls.loadSource(url);
        hls.attachMedia(element);
        play(element);
      };
      const native = createVideo();
      const fallback = () => {
        clearVideo();
        return playHls();
      };
      if (native.canPlayType('application/vnd.apple.mpegurl')) {
        native.src = url;
        play(native, fallback);
      } else {
        await fallback();
      }
    })();
  }
  return stop;
}
