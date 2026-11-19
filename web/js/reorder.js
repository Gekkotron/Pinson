// Drag to reorder the rows of a `.song-list` (playlists, favorites).

/**
 * The order to save after a drop: `dropped` (videoIds as now on screen) kept to the songs still in
 * `songs`, after any of `songs` the drop didn't list (hearted elsewhere mid-drag: a new heart goes
 * first), so the set matches exactly what the server has.
 */
export function orderIds(dropped, songs) {
  const current = new Set(songs.map((s) => s.videoId));
  const kept = dropped.filter((id) => current.has(id));
  const seen = new Set(kept);
  const added = songs.map((s) => s.videoId).filter((id) => !seen.has(id));
  return [...added, ...kept];
}

/**
 * Where a dragged row lands: its index once dropped, from the rows' resting mids, its own index
 * `from` and the current centre of the dragged row. It passes a neighbour when its centre crosses
 * that neighbour's mid. The drag is clamped to the list, so past either end the centre sits exactly
 * on the end row's mid: that counts as crossing it (`<=` below), or the last slot couldn't be had.
 */
export function dropIndex(mids, from, center) {
  return mids.filter((mid, j) => (j < from ? mid < center : j > from && mid <= center)).length;
}

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
 * way; on drop the DOM is reordered and `onDrop(videoIds, row)` gets the new order and the moved row.
 */
export function enableReorder(list, { onDrop, onState }) {
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
    const mids = rects.map((r) => r.mid);
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
      target = dropIndex(mids, from, center);
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
      if (ids) onDrop(ids, row);
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
