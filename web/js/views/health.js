// Health (`#/health`): playback check, yt-dlp version + update, server facts, recent problems.
// The server does the checking and updating; this page paints GET /api/health and polls while an update runs.

import { health, healthCheck, updateYtDlp } from '../api.js';
import { formatUptime, isNewer, relativeTime } from '../health-format.js';
import { icon } from '../icons.js';
import { h, toast } from '../ui.js';

const POLL_MS = 2000;

const absolute = (iso) => {
  const t = new Date(iso);
  return Number.isNaN(t.getTime()) ? '' : t.toLocaleString();
};

const when = (iso) => h('span', { title: absolute(iso), text: relativeTime(iso) });

const row = (label, ...value) =>
  h('div', { class: 'health-row' }, h('div', { class: 'health-label', text: label }), h('div', { class: 'health-value' }, ...value));

const card = (title, ...children) =>
  h('section', { class: 'health-card' }, h('h2', { class: 'history-day', text: title }), h('div', { class: 'health-body' }, ...children));

export function renderHealth(el) {
  let data = null;
  let checking = false;
  let lastCheck = undefined; // a result from this visit's button; wins over the GET one
  let timer = null;
  let sawRunning = false;
  let gone = false;
  let abort = null;

  const body = h('div', { class: 'health-cards', 'aria-live': 'polite' });
  el.append(h('div', { class: 'page page-list page-health' }, h('h1', { class: 'page-title', text: 'Health' }), body));

  function checkCard() {
    const last = lastCheck !== undefined ? lastCheck : data.check;
    let result = null;
    if (checking) {
      result = h('div', { class: 'health-result', role: 'status', text: 'Checking…' });
    } else if (last) {
      const line = last.ok ? `OK: ${last.title || last.videoId} (${last.ms} ms)` : `Failed: ${last.error || 'unknown error'}`;
      result = h(
        'div',
        { class: 'health-result ' + (last.ok ? 'health-ok' : 'health-bad') },
        h('div', { text: line }),
        h('div', { class: 'health-muted' }, when(last.at)),
      );
    } else {
      result = h('div', { class: 'health-result health-muted', text: 'Not checked yet.' });
    }
    return card(
      'Playback check',
      h('p', { class: 'health-muted', text: 'Asks YouTube for one song with yt-dlp, without downloading it.' }),
      h('button', { type: 'button', class: 'pill pill-primary', disabled: checking, onclick: runCheck }, checking ? 'Checking…' : 'Check now'),
      result,
    );
  }

  function ytDlpCard() {
    const y = data.ytDlp;
    const upd = y.update || { state: 'idle' };
    const running = upd.state === 'running';
    const newer = isNewer(y.latest, y.version);
    const children = [
      row('In use', y.version ? y.version : 'not found', newer && h('span', { class: 'health-badge', text: `Update available: ${y.latest}` })),
      y.latest && row('Latest', y.latest),
      row('Binary', h('span', { class: 'health-mono', text: y.path || 'unknown' }), y.managed && h('span', { class: 'health-muted', text: " (Pinson's own copy)" })),
      h(
        'button',
        { type: 'button', class: 'pill ' + (newer ? 'pill-primary' : ''), disabled: running, onclick: runUpdate },
        running ? 'Updating…' : 'Update yt-dlp',
      ),
    ];
    if (running) children.push(h('div', { class: 'health-result', role: 'status', text: 'Updating yt-dlp, this can take a minute…' }));
    if (upd.state === 'failed') {
      children.push(
        h(
          'div',
          { class: 'health-result health-bad' },
          h('div', { text: `Update failed: ${upd.error || 'unknown error'}` }),
          upd.at && h('div', { class: 'health-muted' }, when(upd.at)),
          Array.isArray(upd.log) && upd.log.length > 0 && h('details', { class: 'health-log' }, h('summary', { text: 'Log' }), h('pre', { text: upd.log.join('\n') })),
        ),
      );
    }
    return card('yt-dlp', ...children);
  }

  function serverCard() {
    const p = data.pinson;
    const out = data.output;
    return card(
      'Server',
      row('Pinson', p.version),
      row('Started', when(p.startedAt)),
      row('Uptime', formatUptime(p.uptimeSecs)),
      row('Output', out && out.name ? out.name : 'None chosen'),
      ...(data.tools || []).map((t) =>
        row(
          t.name,
          t.version ? t.version : 'not found',
          !t.version && t.name === 'deno' && h('div', { class: 'health-muted', text: 'yt-dlp needs it to play YouTube' }),
        ),
      ),
    );
  }

  function problemsCard() {
    const list = data.problems || [];
    if (!list.length) return card('Recent problems', h('div', { class: 'health-muted', text: 'No problems since Pinson started.' }));
    return card(
      'Recent problems',
      h(
        'div',
        { class: 'health-problems' },
        list.map((p) =>
          h(
            'div',
            { class: 'health-problem' },
            h(
              'div',
              { class: 'health-problem-head' },
              h('span', { class: 'health-muted' }, when(p.at)),
              h('span', { class: 'health-level health-level-' + p.level, text: p.level }),
              p.count > 1 && h('span', { class: 'health-count', text: `×${p.count}` }),
            ),
            h('div', { class: 'health-problem-msg', text: p.message }),
            p.target && h('div', { class: 'health-muted health-mono', text: p.target }),
          ),
        ),
      ),
    );
  }

  function paint() {
    body.replaceChildren(checkCard(), ytDlpCard(), serverCard(), problemsCard());
  }

  function schedule() {
    clearTimeout(timer);
    timer = null;
    if (gone || !data || data.ytDlp.update?.state !== 'running') return;
    timer = setTimeout(poll, POLL_MS);
  }

  function apply(next) {
    const before = data && data.ytDlp.update ? data.ytDlp.update.state : null;
    data = next;
    const upd = data.ytDlp.update || { state: 'idle' };
    if (upd.state === 'running') sawRunning = true;
    else if (sawRunning && (upd.state === 'done' || upd.state === 'failed')) {
      sawRunning = false;
      if (upd.state === 'done') toast(upd.switched ? `yt-dlp updated to ${upd.version}` : 'yt-dlp is already the newest');
      else toast(`yt-dlp update failed: ${upd.error || 'unknown error'}`);
    } else if (before !== 'running') sawRunning = false;
    paint();
    schedule();
  }

  function poll() {
    timer = null;
    if (abort) abort.abort();
    abort = new AbortController();
    health(abort.signal).then(
      (next) => {
        if (!gone) apply(next);
      },
      (err) => {
        if (gone || err.name === 'AbortError') return;
        schedule(); // transient failure: keep polling
      },
    );
  }

  function runCheck() {
    if (checking) return;
    checking = true;
    paint();
    healthCheck().then(
      (res) => {
        checking = false;
        lastCheck = res;
        if (!gone) paint();
      },
      (err) => {
        checking = false;
        lastCheck = { ok: false, error: err.message, at: new Date().toISOString() };
        if (!gone) paint();
      },
    );
  }

  function runUpdate() {
    updateYtDlp().then(
      (upd) => {
        if (gone) return;
        sawRunning = true;
        data.ytDlp.update = upd && upd.state ? upd : { state: 'running' };
        paint();
        schedule();
      },
      (err) => {
        if (gone) return;
        toast(err.message);
        if (err.status === 409) poll(); // someone else started it: follow along
      },
    );
  }

  abort = new AbortController();
  body.replaceChildren(h('div', { class: 'search-status', role: 'status', text: 'Loading…' }));
  health(abort.signal).then(
    (first) => {
      if (!gone) apply(first);
    },
    (err) => {
      if (gone || err.name === 'AbortError') return;
      toast(err.message);
      body.replaceChildren(h('div', { class: 'search-status search-error', text: `Could not load health: ${err.message}` }));
    },
  );

  return () => {
    gone = true;
    clearTimeout(timer);
    if (abort) abort.abort();
  };
}
