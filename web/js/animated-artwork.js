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

// TV browsers often play video on a hardware layer under the page, which ignores rounded
// corners (border-radius, clip-path, masks). In canvas mode the video is copied onto a canvas,
// which is clipped like any element, while the video itself shrinks to a dot under it.
const BLACK_LEVEL = 12; // highest channel value still counted as black
// About 6 s at 30 fps: some covers open on black (seen ~1.5 s on a Philips TV), so only a long
// run of black frames means the browser can't copy its video.
const BLACK_FRAMES_MAX = 180;

// Copies each new video frame onto `surface` (centre-cropped like object-fit: cover). Calls
// `onShown` at the first frame that is not all black, `onFail` if none comes or drawing throws.
// Returns a function that stops copying. Driven by requestAnimationFrame, not
// requestVideoFrameCallback: a Philips (Titan OS) TV has the latter but never calls it.
// That TV's decoder also hands out black frames for a while after each jump back to the start
// of the loop, so after a seek or stall, black frames are skipped and the last picture stays.
function mirror(video, surface, { onShown, onFail }) {
  const context = surface.getContext('2d');
  const probe = document.createElement('canvas');
  probe.width = probe.height = 8;
  const probeContext = probe.getContext('2d', { willReadFrequently: true });
  let handle;
  let stopped = false;
  let shown = false;
  let settling = true; // before the first picture, and after each seek or stall
  let blackFrames = 0;
  let lastTime = -1;
  const isBlack = () => {
    probeContext.drawImage(video, 0, 0, probe.width, probe.height);
    const { data } = probeContext.getImageData(0, 0, probe.width, probe.height);
    for (let i = 0; i < data.length; i += 4) {
      if (data[i] > BLACK_LEVEL || data[i + 1] > BLACK_LEVEL || data[i + 2] > BLACK_LEVEL) return false;
    }
    return true;
  };
  const settle = () => { settling = true; };
  video.addEventListener('seeking', settle);
  video.addEventListener('waiting', settle);
  const schedule = () => { handle = requestAnimationFrame(draw); };
  function draw() {
    if (stopped) return;
    const { videoWidth: width, videoHeight: height } = video;
    if (width && height && video.currentTime !== lastTime) { // only new video frames
      lastTime = video.currentTime;
      try {
        const size = Math.round(surface.clientWidth * (window.devicePixelRatio || 1)) || width;
        if (surface.width !== size) {
          surface.width = surface.height = size; // resets the context settings
          context.imageSmoothingQuality = 'high'; // a 768–2160 px frame shrinks without grain
        }
        if (settling && isBlack()) {
          if (!shown && ++blackFrames >= BLACK_FRAMES_MAX) {
            onFail('only black frames reach the canvas');
            return;
          }
        } else {
          settling = false;
          const side = Math.min(width, height);
          context.drawImage(video, (width - side) / 2, (height - side) / 2, side, side, 0, 0, size, size);
          if (!shown) {
            shown = true;
            onShown();
          }
        }
      } catch (error) {
        onFail(error.message);
        return;
      }
    }
    schedule();
  }
  schedule();
  return () => {
    stopped = true;
    cancelAnimationFrame(handle);
    video.removeEventListener('seeking', settle);
    video.removeEventListener('waiting', settle);
  };
}

/**
 * Index of the smallest HLS level at least `size` pixels tall (the largest if none is), for
 * hls.js `autoLevelCapping`; -1 (no cap) without levels. The canvas source is only a dot, so
 * hls.js can't size the stream from the player and would otherwise pick up to 2160×2160.
 */
export function levelCap(levels, size) {
  let best = -1;
  levels.forEach((level, i) => {
    const current = levels[best];
    if (
      best < 0
      || (level.height >= size && (current.height < size || level.height < current.height))
      || (current.height < size && level.height > current.height)
    ) best = i;
  });
  return best;
}

/**
 * Plays the song's animated cover (if Picore has one) over the static artwork in `box`.
 * `canvas`: draw it through a canvas, for TV browsers whose video ignores rounded corners.
 * Returns a function that stops and removes it.
 */
export function animateCover(box, song, { canvas = false } = {}) {
  let stopped = false;
  let hls;
  let video;
  let surface;
  let stopMirror;
  const motion = window.matchMedia('(prefers-reduced-motion: reduce)');
  const clearVideo = () => {
    const previous = video;
    video = null;
    stopMirror?.();
    stopMirror = null;
    surface?.remove();
    surface = null;
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
        element.muted = true;
        element.loop = true;
        element.playsInline = true;
        element.setAttribute('aria-hidden', 'true');
        if (canvas) {
          element.className = 'animated-source';
          element.crossOrigin = 'anonymous'; // Apple allows it; an untainted canvas can be checked
          surface = document.createElement('canvas');
          surface.className = 'animated-cover';
          surface.setAttribute('aria-hidden', 'true');
          box.append(element, surface);
        } else {
          element.className = 'animated-cover';
          element.addEventListener('playing', () => element.classList.add('playing'), { once: true });
          box.append(element);
        }
        return element;
      };
      const play = (element, onFailure = clearVideo) => {
        const fail = (reason) => {
          if (video !== element) return;
          console.debug('Animated artwork playback failed:', reason);
          onFailure();
        };
        element.addEventListener('error', () => fail(element.error?.message), { once: true });
        if (canvas) {
          const target = surface;
          element.addEventListener('playing', () => {
            if (video !== element) return;
            stopMirror = mirror(element, target, { onShown: () => target.classList.add('playing'), onFail: fail });
          }, { once: true });
        }
        element.play().catch((error) => fail(error.message));
      };
      const playHls = async (onFailure = clearVideo) => {
        const Hls = await loadHls();
        if (stopped) return;
        if (!Hls?.isSupported()) {
          if (canvas) onFailure();
          return;
        }
        const element = createVideo();
        hls = new Hls({ capLevelToPlayerSize: !canvas });
        if (canvas) {
          // One fixed version: a TV's own player restarts each loop at 360 px and climbs to
          // 2160 px, and every switch restarts its decoder with seconds of black frames.
          const target = surface;
          const player = hls;
          hls.on(Hls.Events.MANIFEST_PARSED, () => {
            const size = Math.round(target.clientWidth * (window.devicePixelRatio || 1)) || 1080;
            const level = levelCap(player.levels, size);
            if (level >= 0) player.currentLevel = level;
          });
        }
        hls.on(Hls.Events.ERROR, (_event, data) => {
          if (video !== element || !data.fatal) return;
          console.debug('Animated artwork HLS error:', data.details);
          onFailure();
        });
        hls.loadSource(url);
        hls.attachMedia(element);
        play(element, onFailure);
      };
      const playNative = (onFailure) => {
        const element = createVideo();
        if (!element.canPlayType('application/vnd.apple.mpegurl')) {
          onFailure();
          return false;
        }
        element.src = url;
        play(element, onFailure);
        return true;
      };
      if (canvas) {
        // TV: hls.js first (fixed quality, clean loops), the TV's own player if it fails.
        await playHls(() => {
          clearVideo();
          if (!stopped) playNative(clearVideo);
        });
      } else {
        playNative(() => {
          clearVideo();
          return playHls();
        });
      }
    })();
  }
  return stop;
}
