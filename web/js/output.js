// "Where should the music play?": the output picker, and the rules for when it opens by itself
// (first run with no output chosen, or a song started with none). The list follows `store.output`.

import { output } from './api.js';
import { deviceId, outputName } from './device-id.js';
import { unlockAudio } from './device-player.js';
import { pickerOptions, rowIsCurrent, streamLink } from './output-pick.js';
import { store } from './store.js';
import { h, pillButton, sheet, toast } from './ui.js';

let picker = null; // the open sheet, or null

/** Opens the picker (no-op if it is already open). */
export function openOutputPicker() {
  if (picker) return;
  const list = h('div', { class: 'output-list' });
  const off = store.on('output', paint);
  const s = sheet(list, { title: 'Where should the music play?', cls: 'sheet-output' });
  picker = s;
  s.closed.then(() => {
    off();
    if (picker === s) picker = null;
  });
  paint();

  function paint() {
    const view = store.output;
    if (!view) {
      list.replaceChildren(h('p', { class: 'output-note', text: 'Loading…' }));
      return;
    }
    // A snapshot arrives whenever `reachable` changes: keep what is typed and where the cursor is.
    const old = [...list.querySelectorAll('input')];
    const drafts = new Map(old.map((i) => [i.dataset.kind, i.value]));
    const focused = old.find((i) => i === document.activeElement);
    const at = focused ? [focused.selectionStart, focused.selectionEnd] : null;
    // An output fixed at startup (such as log) may not be pickable: still show it as the current one.
    const opts = pickerOptions(view, deviceId());
    list.replaceChildren(...opts.map((opt) => row(view, opt, drafts.get(opt.kind))));
    if (focused) {
      const input = list.querySelector(`input[data-kind="${focused.dataset.kind}"]`);
      if (input) {
        input.focus({ preventScroll: true });
        input.setSelectionRange(at[0], at[1]);
      }
    }
  }
}

function row(view, opt, draft) {
  const cur = view.current;
  const isCurrent = rowIsCurrent(cur, opt, deviceId());
  const badges = [];
  if (isCurrent) {
    badges.push(h('span', { class: 'output-badge output-badge-on', text: 'Playing here' }));
    if (cur.id === view.startupId) badges.push(h('span', { class: 'output-badge', text: 'Default at startup' }));
    if (cur.reachable === false) badges.push(h('span', { class: 'output-badge output-badge-bad', text: "Can't reach it" }));
  }
  const head = h('div', { class: 'output-head' }, h('span', { class: 'output-name', text: opt.name }), ...badges);
  const cls = 'output-row' + (isCurrent ? ' current' : '');
  if (!opt.needsAddress) {
    if (isCurrent) return h('div', { class: cls }, head, cur.hint && h('p', { class: 'output-note', text: cur.hint }), cur.kind === 'stream' && linkLine());
    return h('div', { class: cls }, head, opt.note && h('p', { class: 'output-note', text: opt.note }), h('div', { class: 'output-form' }, pillButton('output', 'Use', () => choose(opt.address ? { kind: opt.kind, address: opt.address, name: opt.name } : { kind: opt.kind }))));
  }
  const input = h('input', {
    class: 'field-input output-address',
    type: 'text',
    name: 'address',
    value: draft ?? ((isCurrent && cur.address) || opt.defaultAddress || ''),
    placeholder: 'host:port',
    autocomplete: 'off',
    autocapitalize: 'off',
    spellcheck: 'false',
    'aria-label': `${opt.name} address`,
    dataset: { kind: opt.kind },
  });
  const use = h('button', { type: 'submit', class: 'pill pill-primary' }, 'Use');
  const form = h(
    'form',
    {
      class: 'output-form',
      novalidate: true,
      onsubmit: async (e) => {
        e.preventDefault();
        use.disabled = true;
        await choose({ kind: opt.kind, address: input.value.trim() });
        use.disabled = false;
      },
    },
    input,
    use,
  );
  return h('div', { class: cls }, head, !isCurrent && opt.note && h('p', { class: 'output-note', text: opt.note }), form);
}

/** The Stream URL link (tap opens it, long press copies it) and a Copy button. */
function linkLine() {
  const url = streamLink(location.origin);
  const a = h('a', { class: 'output-link', href: url, target: '_blank', rel: 'noopener', text: url });
  const copy = async () => {
    try {
      await navigator.clipboard.writeText(url);
      toast('Link copied');
    } catch {
      getSelection()?.selectAllChildren(a); // no clipboard (plain http): select it instead
    }
  };
  return h('div', { class: 'output-form' }, a, pillButton('output', 'Copy', copy));
}

/** Picks an output. For `browser` it names this device, and unlocks audio first (inside the tap, before any await). */
export async function choose(choice) {
  if (choice.kind === 'browser') {
    unlockAudio();
    choice = { kind: 'browser', address: deviceId(), name: outputName(navigator.userAgent) };
  }
  try {
    store.setOutput(await output.set(choice)); // the server also broadcasts it; this is right even if the socket lags
  } catch (err) {
    toast(err.message);
  }
}

// ---------- when it opens by itself ----------
let firstSeen = false;

/** Call once at start: opens the picker on a first run and when a song is started with no output. */
export function watchOutput() {
  store.on('output', (view) => {
    if (firstSeen) return;
    firstSeen = true;
    if (!view.current) openOutputPicker();
  });
  store.on('notice', (msg) => {
    if (msg.type === 'noOutput') openOutputPicker();
  });
}
