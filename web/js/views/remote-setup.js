// "Set up your remote": on the TV lyrics page, asks for the button of each action in turn and saves
// the answers on this device. It opens by itself the first time, and again from the remote icon in
// the size pill. Buttons the TV browser keeps for itself never reach the page, so every step can be
// skipped with the pointer.

import { isIgnoredKey, keyId, keyLabel, markRemoteSetupSeen, saveCustomKeys } from '../tv-remote.js';
import { h, toast } from '../ui.js';

const STEPS = [
  { action: 'playPause', label: 'Play / pause' },
  { action: 'next', label: 'Next song' },
  { action: 'previous', label: 'Previous song' },
  { action: 'bigger', label: 'Bigger text' },
  { action: 'smaller', label: 'Smaller text' },
];

/**
 * Opens the setup over the page. `onSaved(map)` gets the new mapping (`{ [keyId]: action }`) when
 * the last step is done; cancelling keeps the old one. Returns a function that closes it.
 */
export function openRemoteSetup(onSaved) {
  const map = {};
  const names = {}; // keyId -> step label, to catch one button used twice
  const done = []; // lines of the summary
  let step = 0;

  const kicker = h('p', { class: 'remote-setup-kicker' });
  const ask = h('h2', { class: 'remote-setup-ask' });
  const got = h('p', { class: 'remote-setup-got', role: 'status' });
  const list = h('ul', { class: 'remote-setup-list' });
  const skipBtn = h('button', { type: 'button', class: 'pill', onclick: () => next() }, 'Skip');
  const cancelBtn = h('button', { type: 'button', class: 'pill', onclick: () => close(false) }, 'Cancel');
  const card = h(
    'div',
    { class: 'remote-setup-card' },
    kicker,
    ask,
    h('p', { class: 'remote-setup-hint', text: 'Nothing happens? Your TV keeps that button for itself: choose Skip.' }),
    got,
    list,
    h('div', { class: 'remote-setup-actions' }, skipBtn, cancelBtn),
  );
  const overlay = h('div', { class: 'remote-setup', role: 'dialog', 'aria-modal': 'true', 'aria-label': 'Set up your remote' }, card);

  const paint = () => {
    kicker.textContent = `Set up your remote · ${step + 1} / ${STEPS.length}`;
    ask.replaceChildren('Press the button for ', h('strong', { text: STEPS[step].label }));
    list.replaceChildren(...done.map((line) => h('li', { text: line })));
  };

  const next = () => {
    step += 1;
    got.textContent = '';
    if (step >= STEPS.length) return close(true);
    paint();
  };

  // Capture phase on window: the page's own remote handling and shortcuts never see these keys.
  const onKey = (e) => {
    if (e.metaKey || e.ctrlKey || e.altKey || isIgnoredKey(e)) return;
    e.preventDefault();
    e.stopImmediatePropagation();
    if (e.repeat) return;
    if (e.key === 'Escape' || e.keyCode === 27) return close(false);
    const id = keyId(e);
    if (names[id]) {
      got.textContent = `That button is already “${names[id]}”. Press another one, or Skip.`;
      return;
    }
    map[id] = STEPS[step].action;
    names[id] = STEPS[step].label;
    done.push(`${STEPS[step].label}: ${keyLabel(e)}`);
    next();
  };

  let closed = false;
  const close = (finished) => {
    if (closed) return;
    closed = true;
    window.removeEventListener('keydown', onKey, true);
    overlay.remove();
    markRemoteSetupSeen();
    if (finished) {
      saveCustomKeys(map);
      onSaved(map);
      toast(Object.keys(map).length ? 'Remote ready' : 'No buttons set: the built-in ones still work');
    }
  };

  window.addEventListener('keydown', onKey, true);
  paint();
  document.body.append(overlay);
  return () => close(false);
}
