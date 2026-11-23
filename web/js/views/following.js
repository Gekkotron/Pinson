// Artists you follow (`#/following`): round photo, name, "N new" badge and an Unfollow pill per row.

import { follows, unfollow } from '../api.js';
import { newBadge } from '../follows.js';
import { cover, emptyState, h, toast } from '../ui.js';

const EMPTY = ['music', 'No artists followed yet', 'Follow artists from their page to see their new releases on Home.'];

export function renderFollowing(el) {
  const list = h('div', { class: 'follow-list' });
  const page = h(
    'div',
    { class: 'page page-following' },
    h('h1', { class: 'page-title', text: 'Artists you follow' }),
    h('div', { class: 'search-status', role: 'status', text: 'Loading…' }),
  );
  el.append(page);
  const ctl = new AbortController();
  let alive = true;

  const showEmpty = () => page.replaceChildren(page.firstChild, emptyState(...EMPTY));

  function row(a) {
    const badge = newBadge(a.newCount);
    const btn = h('button', { type: 'button', class: 'pill', text: 'Unfollow' });
    const item = h(
      'div',
      { class: 'follow-row' },
      h(
        'a',
        { class: 'follow-link', href: '#/artist/' + encodeURIComponent(a.id) },
        h('div', { class: 'follow-art' }, cover(a.artworkUrl, { size: 56 })),
        h('div', { class: 'follow-name', text: a.name }),
        badge && h('span', { class: 'new-badge', text: badge }),
      ),
      btn,
    );
    btn.addEventListener('click', () => {
      btn.disabled = true;
      unfollow(a.id).then(
        () => {
          item.remove();
          toast(`Unfollowed ${a.name}`);
          if (alive && !list.children.length) showEmpty();
        },
        (err) => {
          btn.disabled = false;
          toast(err.message);
        },
      );
    });
    return item;
  }

  follows(ctl.signal).then(
    (answer) => {
      if (!alive) return;
      const artists = (answer && answer.artists) || [];
      if (!artists.length) return showEmpty();
      list.replaceChildren(...artists.map(row));
      page.replaceChildren(page.firstChild, list);
    },
    (err) => alive && page.replaceChildren(page.firstChild, emptyState('music', "Couldn't load your artists", err.message)),
  );
  return () => {
    alive = false;
    ctl.abort();
  };
}
