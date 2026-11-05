// A playlist of mine (`#/playlist/:id`): header with Play / Shuffle / Add to queue / Rename /
// Delete, then the songs, which can be dragged into a new order (mouse and touch).

import { playlists, queueAdd, queuePlay } from '../api.js';
import { go } from '../router.js';
import { store } from '../store.js';
import {
  confirmInPage,
  cover,
  emptyState,
  fmtTotal,
  h,
  listHeader,
  pillButton,
  plural,
  songMenu,
  songRow,
  toast,
} from '../ui.js';

/** Up to four different covers for the header mosaic. */
function artFor(pl) {
  const urls = [];
  for (const s of pl.songs) if (s.artworkUrl && !urls.includes(s.artworkUrl)) urls.push(s.artworkUrl);
  return urls.slice(0, 4);
}

const signature = (pl) => pl.name + '\n' + pl.songs.map((s) => s.videoId).join(',');

export function renderPlaylist(el, { params }) {
  const id = params.id;
  const fail = (err) => toast(err.message);
  const page = h('div', { class: 'page page-list' }, h('div', { class: 'search-status', role: 'status', text: 'Loading…' }));
  el.append(page);

  let pl = null; // the playlist as shown
  let alive = true;
  let deleting = false; // this tab is deleting it: the WS echo must not toast
  let busy = 0; // own writes in flight: ignore WS echoes until they settle
  let dragging = false;
  let editing = false; // the rename field is open
  let draft = '';
  let renameError = '';
  let focusRename = false;

  // ---------- loading and live refresh ----------
  playlists.get(id).then(
    (p) => {
      if (!alive) return;
      pl = p;
      paint();
      sync();
    },
    (err) => {
      if (!alive) return;
      page.replaceChildren(emptyState('music', err.status === 404 ? 'Playlist not found' : "Couldn't open this", err.message));
    },
  );

  function gone() {
    alive = false;
    go('#/home', { replace: true });
    toast('Playlist was deleted');
  }

  // Called on every `playlists` message, and when an own write or drag ends. The summary is a
  // hint: it may lag behind a playlist created a moment ago, so "absent" is checked with a fetch.
  async function sync() {
    if (!alive || !pl || deleting || busy || dragging) return;
    const sum = store.playlists.find((p) => p.id === id);
    if (sum && sum.updatedAt === pl.updatedAt) return;
    try {
      const fresh = await playlists.get(id);
      if (!alive || deleting || busy || dragging) return;
      apply(fresh);
    } catch (err) {
      if (alive && !deleting && err.status === 404) gone();
    }
  }

  function apply(fresh) {
    const changed = signature(fresh) !== signature(pl);
    pl = fresh;
    if (changed) paint();
  }

  // ---------- painting ----------
  let headEl = null;
  let bodyEl = null;

  function paint() {
    headEl = null;
    bodyEl = null;
    page.replaceChildren();
    paintHeader();
    paintBody();
  }

  function paintHeader() {
    const songs = pl.songs;
    const meta = [plural(songs.length, 'song'), fmtTotal(songs)].filter(Boolean).join(' · ');
    const actions = [];
    if (songs.length) {
      actions.push(
        pillButton('play', 'Play', () => queuePlay(pl.songs).catch(fail), { primary: true }),
        pillButton('shuffle', 'Shuffle', () =>
          queuePlay(pl.songs, { shuffle: true }).then(() => toast(`Shuffling ${plural(pl.songs.length, 'song')}`), fail),
        ),
        pillButton('addQueue', 'Add to queue', () =>
          queueAdd(pl.songs, 'end').then((r) => toast(`Added ${plural(r.count, 'song')} to the queue`), fail),
        ),
      );
    }
    if (!editing) actions.push(pillButton('edit', 'Rename', startRename));
    actions.push(pillButton('trash', 'Delete', remove));
    const header = listHeader({
      art: cover(null, { mosaic: artFor(pl), eager: true }),
      kicker: 'Playlist',
      title: pl.name,
      meta,
      actions,
    });
    if (editing) header.querySelector('.list-header-title').replaceWith(renameForm());
    if (headEl) headEl.replaceWith(header);
    else page.prepend(header);
    headEl = header;
    if (focusRename) {
      focusRename = false;
      const input = header.querySelector('.rename-input');
      input.focus({ preventScroll: true });
      input.select();
    }
  }

  function paintBody() {
    const songs = pl.songs;
    let body;
    if (songs.length) {
      body = h(
        'div',
        { class: 'song-list' },
        songs.map((song) =>
          songRow(song, {
            draggable: true,
            onPlay: () =>
              queuePlay(pl.songs, { startIndex: Math.max(0, pl.songs.findIndex((s) => s.videoId === song.videoId)) }).catch(fail),
            menu: () => songMenu(song, { inPlaylistId: id }),
          }),
        ),
      );
      enableReorder(body, {
        onState: (on) => {
          dragging = on;
          if (!on) sync();
        },
        onDrop: reorder,
      });
    } else {
      body = emptyState(
        'music',
        'This playlist is empty',
        'Add songs from search with the ⋯ menu on a song.',
        pillButton('search', 'Find songs', () => go('#/search')),
      );
    }
    if (bodyEl) bodyEl.replaceWith(body);
    else page.append(body);
    bodyEl = body;
  }

  // ---------- rename ----------
  function startRename() {
    editing = true;
    draft = pl.name;
    renameError = '';
    focusRename = true;
    paintHeader();
  }

  function stopRename() {
    editing = false;
    renameError = '';
    paintHeader();
  }

  function renameForm() {
    const input = h('input', {
      class: 'field-input rename-input',
      type: 'text',
      value: draft,
      autocomplete: 'off',
      'aria-label': 'Playlist name',
      'aria-invalid': renameError ? 'true' : null,
      oninput: () => (draft = input.value),
      onkeydown: (e) => {
        if (e.key !== 'Escape') return;
        // Only this field's edit is cancelled: nothing else may react to this Escape.
        e.preventDefault();
        e.stopPropagation();
        stopRename();
      },
    });
    const err = h('div', { class: 'field-error', role: 'alert', text: renameError });
    const save = h('button', { type: 'submit', class: 'pill pill-primary' }, 'Save');
    return h(
      'form',
      {
        class: 'rename-form',
        novalidate: true,
        onsubmit: (e) => {
          e.preventDefault();
          if (input.value === pl.name) return stopRename();
          save.disabled = true;
          busy++;
          playlists
            .rename(id, input.value)
            .then(
              (fresh) => {
                editing = false;
                renameError = '';
                if (alive) {
                  pl = fresh;
                  paint();
                }
              },
              (error) => {
                renameError = error.message;
                err.textContent = error.message;
                input.setAttribute('aria-invalid', 'true');
                save.disabled = false;
                input.focus({ preventScroll: true });
              },
            )
            .finally(() => {
              busy--;
              sync();
            });
        },
      },
      input,
      h('div', { class: 'rename-actions' }, h('button', { type: 'button', class: 'pill', onclick: stopRename }, 'Cancel'), save),
      err,
    );
  }

  // ---------- delete ----------
  async function remove() {
    const ok = await confirmInPage(`Delete “${pl.name}”? Its song list is lost. Favorites and the queue stay as they are.`);
    if (!ok || !alive) return;
    deleting = true;
    const name = pl.name;
    try {
      await playlists.remove(id);
    } catch (err) {
      deleting = false;
      // Already gone elsewhere: same outcome as a delete from this tab.
      if (err.status !== 404) return fail(err);
      deleting = true;
    }
    alive = false;
    go('#/home');
    toast(`Deleted “${name}”`);
  }

  // ---------- reorder ----------
  function reorder(ids) {
    const prev = pl.songs;
    const byId = new Map(prev.map((s) => [s.videoId, s]));
    pl = { ...pl, songs: ids.map((v) => byId.get(v)) };
    busy++;
    playlists
      .reorder(id, ids)
      .then(
        (fresh) => alive && apply(fresh),
        (err) => {
          if (!alive) return;
          pl = { ...pl, songs: prev };
          paintBody();
          toast(err.message);
        },
      )
      .finally(() => {
        busy--;
        sync();
      });
  }

  const off = store.on('playlists', sync);
  return () => {
    alive = false;
    off();
  };
}

// ---------- drag to reorder ----------

function scrollParent(node) {
  for (let p = node.parentElement; p; p = p.parentElement) {
    const o = getComputedStyle(p).overflowY;
    if ((o === 'auto' || o === 'scroll') && p.scrollHeight > p.clientHeight) return p;
  }
  return document.scrollingElement;
}

/**
 * Lets the rows of `list` be reordered by dragging their `.drag-handle` (pointer events: mouse,
 * touch and pen). While dragging, the row follows the pointer and the others slide out of the
 * way; on drop the DOM is reordered and `onDrop(videoIds)` gets the new order.
 */
function enableReorder(list, { onDrop, onState }) {
  // A drop must not also count as a tap on the row (which would start playing it).
  list.addEventListener(
    'click',
    (e) => {
      if (e.target.closest('.drag-handle')) e.stopPropagation();
    },
    true,
  );

  list.addEventListener('pointerdown', (e) => {
    const handle = e.target.closest('.drag-handle');
    if (!handle || (e.pointerType === 'mouse' && e.button !== 0)) return;
    e.preventDefault();
    begin(handle.closest('.song-row'), handle, e);
  });

  function begin(row, handle, e) {
    const rows = [...list.children];
    const from = rows.indexOf(row);
    const scroller = scrollParent(list);
    const startScroll = scroller.scrollTop;
    // Row positions in scroller content coordinates, so scrolling while dragging keeps working.
    const rects = rows.map((r) => {
      const b = r.getBoundingClientRect();
      return { top: b.top + startScroll, bottom: b.bottom + startScroll, mid: b.top + b.height / 2 + startScroll };
    });
    const height = rects[from].bottom - rects[from].top;
    const startY = e.clientY;
    let lastY = startY;
    let target = from;
    let active = true;
    let moved = false;
    let raf = 0;

    handle.setPointerCapture(e.pointerId);
    list.classList.add('reordering');
    document.body.classList.add('is-dragging');
    row.classList.add('dragging');
    onState(true);

    const place = () => {
      const delta = scroller.scrollTop - startScroll;
      const lo = rects[0].top - rects[from].top;
      const hi = rects[rows.length - 1].bottom - rects[from].bottom;
      const dy = Math.max(lo, Math.min(hi, lastY - startY + delta));
      row.style.transform = `translateY(${dy}px)`;
      const center = rects[from].mid + dy;
      target = rects.filter((r, j) => j !== from && r.mid < center).length;
      rows.forEach((r, j) => {
        if (j === from) return;
        let shift = 0;
        if (target > from && j > from && j <= target) shift = -height;
        else if (target < from && j >= target && j < from) shift = height;
        r.style.transform = shift ? `translateY(${shift}px)` : '';
      });
    };

    const autoscroll = () => {
      if (!active || !moved) return;
      const box = scroller === document.scrollingElement ? { top: 0, bottom: innerHeight } : scroller.getBoundingClientRect();
      const edge = 56;
      let speed = 0;
      if (lastY < box.top + edge) speed = -Math.ceil((box.top + edge - lastY) / 4);
      else if (lastY > box.bottom - edge) speed = Math.ceil((lastY - (box.bottom - edge)) / 4);
      if (speed) {
        scroller.scrollTop += speed;
        place();
      }
      raf = requestAnimationFrame(autoscroll);
    };
    raf = requestAnimationFrame(autoscroll);

    const move = (ev) => {
      if (ev.pointerId !== e.pointerId) return;
      lastY = ev.clientY;
      moved = true;
      place();
    };
    const finish = (commit) => {
      if (!active) return;
      active = false;
      cancelAnimationFrame(raf);
      handle.removeEventListener('pointermove', move);
      handle.removeEventListener('pointerup', up);
      handle.removeEventListener('pointercancel', cancel);
      handle.removeEventListener('lostpointercapture', cancel);
      // Dropping the classes and transforms in one go leaves the rows exactly where they now are.
      rows.forEach((r) => (r.style.transform = ''));
      list.classList.remove('reordering');
      document.body.classList.remove('is-dragging');
      row.classList.remove('dragging');
      let ids = null;
      if (commit && target !== from) {
        list.insertBefore(row, target > from ? rows[target].nextSibling : rows[target]);
        ids = [...list.children].map((r) => r.dataset.vid);
      }
      if (ids) onDrop(ids);
      onState(false);
    };
    const up = (ev) => ev.pointerId === e.pointerId && finish(true);
    const cancel = () => finish(false);
    handle.addEventListener('pointermove', move);
    handle.addEventListener('pointerup', up);
    handle.addEventListener('pointercancel', cancel);
    handle.addEventListener('lostpointercapture', cancel);
  }
}
