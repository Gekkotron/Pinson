// iOS can alternate between the screen height and a viewport shorter by the top safe area.
export function standaloneHeight(screenHeight, viewportHeight, safeTop) {
  const gap = screenHeight - viewportHeight;
  return safeTop > 0 && (Math.abs(gap) <= 1 || Math.abs(gap - safeTop) <= 1) ? screenHeight : null;
}

export function watchStandaloneViewport() {
  const mode = matchMedia('(display-mode: standalone) and (max-width: 899.98px)');
  const update = () => {
    const safeTop = parseFloat(getComputedStyle(document.documentElement).getPropertyValue('--safe-t')) || 0;
    const height = mode.matches ? standaloneHeight(screen.height, innerHeight, safeTop) : null;
    if (height == null) document.documentElement.style.removeProperty('--standalone-viewport-height');
    else document.documentElement.style.setProperty('--standalone-viewport-height', `${height}px`);
  };
  window.addEventListener('resize', update);
  window.addEventListener('pageshow', update);
  window.visualViewport?.addEventListener('resize', update);
  mode.addEventListener('change', update);
  update();
}
