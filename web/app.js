'use strict';
// Home music phone page. All playback state comes from the WebSocket.

const $ = (id) => document.getElementById(id);
const el = (tag, cls, text) => {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text != null) e.textContent = text;
  return e;
};

const st = { current: null, status: 'stopped', positionMs: 0, upcoming: [], version: -1, receivedAt: 0 };
let results = null;   // null = show queue/favorites; otherwise {loading, error, songs}
let favView = false;  // favorites view open (shown whenever no search is active)
let favs = [];        // newest first; the WebSocket is the source of truth
let favIds = new Set();
let searchSeq = 0;
let debounceTimer = 0;
let toastTimer = 0;
let progressTimer = 0;

// ---------- helpers ----------
function fmtTime(ms) {
  const s = Math.max(0, Math.floor(ms / 1000));
  return Math.floor(s / 60) + ':' + String(s % 60).padStart(2, '0');
}

function artwork(song) {
  const box = el('div', 'art');
  setArt(box, song);
  return box;
}

function setArt(box, song) {
  box.replaceChildren();
  if (song && song.artworkUrl) {
    const img = el('img');
    img.loading = 'lazy';
    img.referrerPolicy = 'no-referrer';
    img.alt = '';
    img.src = song.artworkUrl;
    img.onerror = () => img.remove();
    box.appendChild(img);
  }
}

function toast(text, ms) {
  const t = $('toast');
  t.textContent = text;
  t.classList.add('show');
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => t.classList.remove('show'), ms);
}

async function post(path, body) {
  try {
    const opts = { method: 'POST' };
    if (body) { opts.headers = JSON_HEADERS; opts.body = JSON.stringify(body); }
    return await fetch(path, opts);
  } catch (_) { return null; }
}

const JSON_HEADERS = { 'Content-Type': 'application/json' };

const player = (cmd) => post('/api/player/' + cmd);
// Name the song the user saw, so a skip that races the song's end can't skip the next one too.
const playerOnCurrent = (cmd) =>
  post('/api/player/' + cmd + (st.current ? '?entryId=' + st.current.id : ''));
const toggle = () => player(st.status === 'playing' ? 'pause' : 'resume');

// ---------- rendering ----------
function textBlock(song, withAlbum) {
  const g = el('div', 'grow');
  g.append(el('div', 't', song.title));
  const sub = withAlbum && song.album ? song.artists + ' · ' + song.album : song.artists;
  g.append(el('div', 's', sub));
  return g;
}

function updateHeart(btn, song) {
  const on = favIds.has(song.videoId);
  btn.textContent = on ? '♥\uFE0E' : '♡\uFE0E';
  btn.classList.toggle('on', on);
  btn.setAttribute('aria-pressed', String(on));
  btn.setAttribute('aria-label', (on ? 'Remove ' + song.title + ' from' : 'Add ' + song.title + ' to') + ' favorites');
}

function heartBtn(song) {
  const b = el('button', 'icon heart');
  b.type = 'button';
  updateHeart(b, song);
  b.onclick = () => toggleFav(song);
  return b;
}

function playNextBtn(song) {
  const add = el('button', 'btn hot', 'Play next');
  add.type = 'button';
  add.setAttribute('aria-label', 'Play next: ' + song.title);
  add.onclick = () => playNext(song, add);
  return add;
}

function renderList() {
  const list = $('list');
  list.replaceChildren();
  const showResults = results !== null;
  const showFavs = !showResults && favView;
  $('section').textContent = showResults ? 'Results' : showFavs ? 'Favorites · ' + favs.length : 'Up next';
  $('section').hidden = !showResults && !showFavs && !st.upcoming.length;
  if (showResults) return renderResults(list);
  if (showFavs) return renderFavs(list);
  if (!st.current && !st.upcoming.length) {
    const e = el('div', 'empty');
    e.append(el('b', null, 'Nothing playing'), 'Search for a song and tap Play next');
    list.append(e);
    return;
  }
  for (const entry of st.upcoming) {
    const row = el('div', 'row');
    const rm = el('button', 'icon', '✕');
    rm.type = 'button';
    rm.setAttribute('aria-label', 'Remove ' + entry.song.title);
    rm.onclick = () => fetch('/api/queue/' + entry.id, { method: 'DELETE' }).catch(() => {});
    row.append(artwork(entry.song), textBlock(entry.song), heartBtn(entry.song), rm);
    list.append(row);
  }
}

function renderResults(list) {
  if (results.loading) return list.append(el('div', 'msg', 'Searching…'));
  if (results.error) return list.append(el('div', 'msg err', results.error));
  if (!results.songs.length) return list.append(el('div', 'msg', 'No results'));
  for (const song of results.songs) {
    const row = el('div', 'row');
    row.append(artwork(song), textBlock(song, true), heartBtn(song), playNextBtn(song));
    list.append(row);
  }
}

function renderFavs(list) {
  if (!favs.length) {
    const e = el('div', 'empty');
    e.append(el('b', null, 'No favorites yet'), 'Tap ♡ on a song to save it');
    return list.append(e);
  }
  const shuffle = el('button', 'btn hot wide', 'Shuffle all');
  shuffle.type = 'button';
  shuffle.onclick = () => shuffleFavs(shuffle);
  list.append(shuffle);
  for (const song of favs) {
    const row = el('div', 'row');
    row.append(artwork(song), textBlock(song), heartBtn(song), playNextBtn(song));
    list.append(row);
  }
}

function renderPlayer() {
  const cur = st.current && st.current.song;
  $('mini').hidden = !cur;
  if (!cur) { closeFull(); return; }
  const playing = st.status === 'playing';
  const icon = playing ? '⏸' : '▶';
  const label = playing ? 'Pause' : 'Play';
  for (const id of ['mini-toggle', 'full-toggle']) {
    $(id).textContent = icon;
    $(id).setAttribute('aria-label', label);
  }
  $('mini-title').textContent = cur.title;
  $('mini-artist').textContent = cur.artists;
  setArt($('mini-art'), cur);
  $('full-title').textContent = cur.title;
  $('full-artist').textContent = cur.artists;
  $('full-album').textContent = cur.album || '';
  setArt($('full-art'), cur);
  updateHeart($('full-heart'), cur);
  updateProgress();
}

function render() { renderList(); renderPlayer(); }

// ---------- progress ----------
function updateProgress() {
  const cur = st.current && st.current.song;
  if (!cur || $('full').hidden) return;
  const dur = cur.durationSecs ? cur.durationSecs * 1000 : 0;
  let pos = st.positionMs;
  if (st.status === 'playing') pos += performance.now() - st.receivedAt;
  if (dur) pos = Math.min(pos, dur);
  $('t-cur').textContent = fmtTime(pos);
  $('t-tot').textContent = dur ? fmtTime(dur) : '--:--';
  $('prog-fill').style.width = dur ? (100 * pos / dur) + '%' : '0%';
}

function openFull() {
  if (!st.current) return;
  $('full').hidden = false;
  renderPlayer();
  clearInterval(progressTimer);
  progressTimer = setInterval(updateProgress, 250);
  $('full-close').focus();
}

function closeFull() {
  if ($('full').hidden) return;
  $('full').hidden = true;
  clearInterval(progressTimer);
}

// ---------- search ----------
async function runSearch() {
  clearTimeout(debounceTimer);
  const q = $('search').value.trim();
  if (!q) return clearSearch();
  const seq = ++searchSeq;
  results = { loading: true };
  renderList();
  let next;
  try {
    const r = await fetch('/api/search?q=' + encodeURIComponent(q));
    const data = await r.json().catch(() => null);
    next = r.ok && Array.isArray(data)
      ? { songs: data }
      : { error: (data && data.error) || 'Search failed (' + r.status + ')' };
  } catch (_) {
    next = { error: 'Search failed: server unreachable' };
  }
  if (seq !== searchSeq) return; // a newer search superseded this one
  results = next;
  renderList();
}

function clearSearch() {
  clearTimeout(debounceTimer);
  searchSeq++;
  results = null;
  $('search').value = '';
  $('clear').hidden = true;
  renderList();
}

async function playNext(song, btn) {
  btn.disabled = true;
  const r = await post('/api/queue/next', song);
  if (r && r.ok) toast('Added: ' + song.title, 2000);
  else toast('Could not add: ' + song.title, 2000);
  setTimeout(() => { btn.disabled = false; }, 800);
}

// ---------- favorites ----------
function setFavs(songs) {
  favs = songs;
  favIds = new Set(songs.map((s) => s.videoId));
  renderList();
  renderPlayer();
}

// Optimistic; the server's "favorites" message overrides it.
async function toggleFav(song) {
  const add = !favIds.has(song.videoId);
  const apply = (on) => setFavs(on ? [song, ...favs] : favs.filter((s) => s.videoId !== song.videoId));
  apply(add);
  const opts = add ? { method: 'PUT', headers: JSON_HEADERS, body: JSON.stringify(song) } : { method: 'DELETE' };
  let ok = false;
  try { ok = (await fetch('/api/favorites/' + encodeURIComponent(song.videoId), opts)).ok; } catch (_) {}
  if (!ok) {
    if (favIds.has(song.videoId) === add) apply(!add);
    toast('Could not update favorites', 2000);
  }
}

function setFavView(open) {
  favView = open;
  $('favs-toggle').textContent = open ? '♥\uFE0E' : '♡\uFE0E';
  $('favs-toggle').classList.toggle('on', open);
  $('favs-toggle').setAttribute('aria-pressed', String(open));
  renderList();
}

async function shuffleFavs(btn) {
  btn.disabled = true;
  const r = await post('/api/favorites/shuffle');
  const data = r ? await r.json().catch(() => null) : null;
  if (r && r.ok && data) {
    toast('Shuffling ' + data.count + (data.count === 1 ? ' favorite' : ' favorites'), 2000);
    setFavView(false);
  } else {
    toast((data && data.error) || 'Could not shuffle', 2000);
    btn.disabled = false;
  }
}

// ---------- WebSocket ----------
let ws = null;
let backoff = 1000;
let firstAfterConnect = true;

function connect() {
  const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
  ws = new WebSocket(proto + '//' + location.host + '/api/ws');
  ws.onopen = () => {
    backoff = 1000;
    firstAfterConnect = true;
    $('banner').hidden = true;
  };
  ws.onmessage = (ev) => {
    let msg;
    try { msg = JSON.parse(ev.data); } catch (_) { return; }
    if (msg.type === 'state') applyState(msg);
    else if (msg.type === 'favorites') setFavs(msg.songs || []);
    else if (msg.type === 'songFailed') toast('Skipped ' + msg.title + ': ' + msg.reason, 4000);
  };
  ws.onclose = () => {
    $('banner').hidden = false;
    setTimeout(connect, backoff);
    backoff = Math.min(backoff * 2, 10000);
  };
  ws.onerror = () => ws.close();
}

function applyState(msg) {
  // The server may have restarted, so accept any version right after a reconnect.
  if (!firstAfterConnect && msg.version < st.version) return;
  firstAfterConnect = false;
  st.version = msg.version;
  st.current = msg.current;
  st.status = msg.status;
  st.positionMs = msg.positionMs || 0;
  st.receivedAt = performance.now();
  st.upcoming = msg.upcoming || [];
  render();
}

// ---------- wiring ----------
$('search').addEventListener('input', () => {
  const has = $('search').value.trim() !== '';
  $('clear').hidden = !$('search').value;
  clearTimeout(debounceTimer);
  if (!has) return clearSearch();
  debounceTimer = setTimeout(runSearch, 300);
});
$('search').addEventListener('keydown', (e) => {
  if (e.key === 'Enter') { e.preventDefault(); runSearch(); $('search').blur(); }
});
$('clear').onclick = () => { clearSearch(); $('search').focus(); };
$('favs-toggle').onclick = () => {
  const wasSearching = results !== null;
  if (wasSearching) clearSearch();
  setFavView(wasSearching || !favView);
};
$('full-heart').onclick = () => { if (st.current) toggleFav(st.current.song); };
$('mini-toggle').onclick = toggle;
$('full-toggle').onclick = toggle;
$('mini-skip').onclick = () => playerOnCurrent('skip');
$('full-skip').onclick = () => playerOnCurrent('skip');
$('full-prev').onclick = () => playerOnCurrent('previous');
$('full-close').onclick = closeFull;
$('mini-open').onclick = openFull;
$('mini-open').addEventListener('keydown', (e) => {
  if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); openFull(); }
});
document.addEventListener('keydown', (e) => {
  if (e.key !== 'Escape') return;
  if (!$('full').hidden) closeFull();
  else if (results !== null || $('search').value) clearSearch();
  else if (favView) setFavView(false);
});

render();
connect();
