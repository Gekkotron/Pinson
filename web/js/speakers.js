// Speaker popup: one row per approved Nidoo satellite with a volume slider and a mute button.
// The list is fetched on open and polled while the popup is open; a row the user is touching
// keeps its local value until the change has reached the server.

import { ApiError, speakers } from './api.js';
import { deviceSoundAvailable, deviceSoundOn, setDeviceSound, startDeviceSound, stopDeviceSound } from './device-sound.js';
import { icon, setIcon } from './icons.js';
import { emptyState, h, iconButton, pillButton, sheet, toast } from './ui.js';

const POLL_MS = 3000;
const THROTTLE_MS = 150;

let available = null;

/**
 * False only when the server has no speaker control (404); an unreachable hub still shows the buttons.
 * Asked once per page load: whether the engine relays speaker control does not change while it runs.
 */
export function speakersAvailable() {
  if (!available) {
    available = speakers.list().then(
      () => true,
      (err) => !(err instanceof ApiError && err.status === 404),
    );
  }
  return available;
}

/** "This device" row: a switch that makes this browser a speaker too. Hidden until the server says it can. */
function deviceRow() {
  const input = h('input', { type: 'checkbox', role: 'switch', class: 'switch-input', id: 'device-sound-switch' });
  input.checked = deviceSoundOn();
  const hint = h('p', { class: 'device-hint', text: 'Approve it once in Nidoo; it then plays with the other speakers.' });
  hint.hidden = !input.checked;
  input.addEventListener('change', () => {
    setDeviceSound(input.checked);
    hint.hidden = !input.checked;
    if (input.checked) startDeviceSound();
    else stopDeviceSound();
  });
  const row = h(
    'div',
    { class: 'device-row' },
    h('div', { class: 'speaker-head' }, h('span', { class: 'speaker-name', text: 'This device' })),
    h(
      'label',
      { class: 'switch-line', for: 'device-sound-switch' },
      h('span', { class: 'switch-label', text: 'Play sound on this device' }),
      input,
      h('span', { class: 'switch', 'aria-hidden': 'true' }),
    ),
    hint,
  );
  row.hidden = true;
  deviceSoundAvailable().then((ok) => (row.hidden = !ok));
  return row;
}

export function openSpeakers() {
  const rows = new Map(); // id -> row
  const listEl = h('div', { class: 'speaker-list' });
  const body = h('div', { class: 'speaker-body' });
  let alive = true;
  let seq = 0;

  const busy = (r) => r.dragging || r.timer || r.pendingVol != null || r.inflight > 0;
  const anyBusy = () => [...rows.values()].some(busy);

  function paint(r) {
    const { name } = r.server;
    const online = r.server.connected;
    r.el.classList.toggle('muted', r.muted);
    r.el.classList.toggle('offline', !online);
    r.name.textContent = name;
    r.note.hidden = online;
    r.slider.value = String(r.volume);
    r.slider.disabled = !online;
    r.slider.setAttribute('aria-label', `${name} volume`);
    r.slider.setAttribute('aria-valuetext', `${r.volume}%`);
    r.value.textContent = String(r.volume);
    r.mute.disabled = !online;
    const label = r.muted ? `Unmute ${name}` : `Mute ${name}`;
    r.mute.setAttribute('aria-label', label);
    r.mute.title = label;
    setIcon(r.mute.querySelector('svg'), r.muted ? 'speakerOff' : 'speaker');
  }

  function adopt(r, sp) {
    r.server = { ...sp };
    r.volume = sp.volume;
    r.muted = sp.muted;
    paint(r);
  }

  /** Sends `change`; on failure puts the server's last known values back and says why. */
  async function push(r, change) {
    r.inflight++;
    r.rev++;
    try {
      await speakers.set(r.id, change);
      Object.assign(r.server, change);
    } catch (err) {
      for (const key of Object.keys(change)) r[key === 'volume' ? 'volume' : 'muted'] = r.server[key];
      if ('volume' in change) r.pendingVol = null;
      paint(r);
      toast(err.message);
      reload();
    } finally {
      r.inflight--;
    }
  }

  function flush(r) {
    r.timer = 0;
    if (r.pendingVol == null || r.volBusy) return;
    const volume = r.pendingVol;
    r.pendingVol = null;
    r.lastSent = performance.now();
    r.volBusy = true;
    push(r, { volume }).finally(() => {
      r.volBusy = false;
      if (r.pendingVol != null) schedule(r);
    });
  }

  // At most one volume call per THROTTLE_MS, in order, and always ending with the latest value.
  function schedule(r) {
    if (r.timer || r.volBusy) return;
    const wait = Math.max(0, THROTTLE_MS - (performance.now() - r.lastSent));
    if (wait === 0) flush(r);
    else r.timer = setTimeout(() => flush(r), wait);
  }

  function makeRow(sp) {
    const r = { id: sp.id, rev: 0, inflight: 0, volBusy: false, timer: 0, pendingVol: null, lastSent: -Infinity, dragging: false };
    r.name = h('span', { class: 'speaker-name' });
    r.note = h('span', { class: 'speaker-note', text: 'Offline' });
    r.value = h('span', { class: 'speaker-vol', 'aria-hidden': 'true' });
    r.mute = iconButton('speaker', '', () => {
      r.muted = !r.muted;
      paint(r);
      push(r, { muted: r.muted });
    });
    r.slider = h('input', { class: 'speaker-slider', type: 'range', min: '0', max: '100', step: '1' });
    const endDrag = () => {
      r.dragging = false;
    };
    r.slider.addEventListener('pointerdown', () => {
      r.dragging = true;
      document.addEventListener('pointerup', endDrag, { once: true });
      document.addEventListener('pointercancel', endDrag, { once: true });
    });
    r.slider.addEventListener('input', () => {
      r.volume = Number(r.slider.value);
      r.pendingVol = r.volume;
      r.value.textContent = String(r.volume);
      r.slider.setAttribute('aria-valuetext', `${r.volume}%`);
      schedule(r);
    });
    r.slider.addEventListener('change', () => {
      clearTimeout(r.timer);
      r.timer = 0;
      r.pendingVol = Number(r.slider.value);
      flush(r);
    });
    r.slider.addEventListener('blur', endDrag);
    r.el = h(
      'div',
      { class: 'speaker-row' },
      h('div', { class: 'speaker-head' }, r.name, r.note),
      h('div', { class: 'speaker-controls' }, r.mute, r.slider, r.value),
    );
    adopt(r, sp);
    return r;
  }

  function show(node) {
    if (body.firstChild !== node || body.childNodes.length !== 1) body.replaceChildren(node);
  }

  function showState(node) {
    rows.clear();
    listEl.replaceChildren();
    show(node);
  }

  function render(list, revs) {
    if (!list.length) {
      showState(emptyState('speaker', 'No speakers yet', 'Approve them in Nidoo.'));
      return;
    }
    show(listEl);
    const ids = new Set(list.map((sp) => sp.id));
    for (const [id, r] of rows) {
      if (!ids.has(id)) {
        clearTimeout(r.timer);
        r.el.remove();
        rows.delete(id);
      }
    }
    for (const sp of list) {
      const r = rows.get(sp.id);
      if (!r) {
        const fresh = makeRow(sp);
        rows.set(sp.id, fresh);
        listEl.append(fresh.el);
      } else if (!busy(r) && r.rev === revs.get(sp.id)) adopt(r, sp);
    }
    const els = list.map((sp) => rows.get(sp.id).el);
    if (!anyBusy() && els.some((el, i) => listEl.children[i] !== el)) listEl.replaceChildren(...els);
  }

  async function reload() {
    const mine = ++seq;
    const revs = new Map([...rows].map(([id, r]) => [id, r.rev]));
    let list;
    try {
      list = await speakers.list();
    } catch (_) {
      if (!alive || mine !== seq || anyBusy()) return;
      showState(emptyState('speakerOff', "Can't reach nidoo-sound", '', pillButton('refresh', 'Retry', reload)));
      return;
    }
    if (!alive || mine !== seq) return;
    render(list, revs);
  }

  const s = sheet(h('div', {}, deviceRow(), body), { title: 'Speakers', cls: 'sheet-speakers' });
  const poll = setInterval(reload, POLL_MS);
  s.closed.then(() => {
    alive = false;
    clearInterval(poll);
    for (const r of rows.values()) clearTimeout(r.timer);
  });
  reload();
}
