// The "Sleep timer" sheet: pick a duration, End of song, or turn the timer off.

import { sleepOff, sleepSet } from './api.js';
import { icon } from './icons.js';
import { store } from './store.js';
import { SLEEP_MINUTES, sleepDisplay } from './sleep-timer.js';
import { h, iconButton, sheet, toast } from './ui.js';

/** Opens the sheet; a tap on a row sends the request and closes it. */
export function openSleep() {
  const active = store.state.sleep;
  let panel = null;
  const row = (label, request, { checked = false, danger = false, iconName } = {}) =>
    h(
      'button',
      {
        type: 'button',
        class: 'menu-item' + (danger ? ' danger' : ''),
        onclick: async () => {
          panel.close();
          try {
            await request();
          } catch (err) {
            toast(err.message);
          }
        },
      },
      iconName && icon(iconName, { size: 22 }),
      h('span', { text: label }),
      checked && h('span', { class: 'menu-check', 'aria-hidden': 'true', text: '✓' }),
    );
  const list = h('div', { class: 'menu' });
  for (const m of SLEEP_MINUTES) list.append(row(`${m} min`, () => sleepSet({ minutes: m })));
  list.append(row('End of song', () => sleepSet({ endOfSong: true }), { checked: !!(active && active.endOfSong) }));
  if (active) list.append(row('Turn off', sleepOff, { danger: true }));
  panel = sheet(list, { title: 'Sleep timer', cls: 'sheet-menu' });
}

/**
 * A moon button that opens the sheet. While a timer runs it is lit and a badge shows the minutes
 * left (or "end"); one interval keeps the badge fresh and stops when the timer is off.
 * Returns `{ el, dispose }`; `dispose` stops listening (for views that are torn down).
 */
export function sleepButton({ size, cls }) {
  const badge = h('span', { class: 'sleep-badge', 'aria-hidden': 'true' });
  const btn = iconButton('moon', 'Sleep timer', openSleep, { size, cls: `sleep-btn ${cls}` });
  btn.append(badge);
  let tick = null;
  const paint = () => {
    const d = sleepDisplay(store.state.sleep, store.state.sleepEndsAt);
    btn.classList.toggle('on', !!d);
    badge.hidden = !d;
    btn.setAttribute('aria-label', d ? d.label : 'Sleep timer');
    btn.title = d ? d.label : 'Sleep timer';
    if (d) {
      badge.textContent = d.badge;
      if (!tick) tick = setInterval(paint, 20000);
    } else if (tick) {
      clearInterval(tick);
      tick = null;
    }
  };
  paint();
  const off = store.on('state', paint);
  return {
    el: btn,
    dispose() {
      off();
      if (tick) clearInterval(tick);
      tick = null;
    },
  };
}
