//! The "play next" queue (spec §9).
//!
//! `play_next` inserts at `next_insert` inside `upcoming` and moves the
//! insertion point forward, so songs added with Play next keep their order.
//! The insertion point resets whenever `current` changes.

use std::collections::{HashSet, VecDeque};

use serde::{Deserialize, Serialize};

use crate::song::Song;

pub type EntryId = u64;

/// How many played entries `back` can step through; the oldest are dropped.
const MAX_BACK: usize = 100;

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueEntry {
    pub id: EntryId,
    pub song: Song,
}

#[derive(Debug, PartialEq)]
pub struct PlayNextOutcome {
    pub id: EntryId,
    /// True if the song became current (the queue was idle).
    pub started: bool,
}

/// Where `add_many` puts songs.
#[derive(Clone, Copy, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AddAt {
    /// Right after the current song, after earlier Play next songs.
    Next,
    /// At the very end of the queue.
    End,
}

#[derive(Debug, PartialEq)]
pub struct AddOutcome {
    pub count: usize,
    /// True if the first song became current (the queue was idle).
    pub started: bool,
}

#[derive(Debug, Default)]
pub struct Queue {
    current: Option<QueueEntry>,
    upcoming: VecDeque<QueueEntry>,
    /// Entries that were current, oldest first. Failed songs aren't kept.
    played: VecDeque<QueueEntry>,
    next_insert: usize,
    next_id: EntryId,
    /// The songs Repeat all loops: what was queued since the queue was last
    /// replaced. Autoplay suggestions and loop copies never enter it.
    cycle: Vec<Song>,
}

impl Queue {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn current(&self) -> Option<&QueueEntry> {
        self.current.as_ref()
    }

    pub fn upcoming(&self) -> &VecDeque<QueueEntry> {
        &self.upcoming
    }

    pub fn play_next(&mut self, song: Song) -> PlayNextOutcome {
        if self.current.is_none() {
            self.cycle.clear();
        }
        self.cycle.push(song.clone());
        self.insert_next(song)
    }

    fn insert_next(&mut self, song: Song) -> PlayNextOutcome {
        self.next_id += 1;
        let entry = QueueEntry {
            id: self.next_id,
            song,
        };
        if self.current.is_none() {
            self.current = Some(entry);
            self.next_insert = 0;
            return PlayNextOutcome {
                id: self.next_id,
                started: true,
            };
        }
        let at = self.next_insert.min(self.upcoming.len());
        self.upcoming.insert(at, entry);
        self.next_insert = at + 1;
        PlayNextOutcome {
            id: self.next_id,
            started: false,
        }
    }

    /// Adds songs in order, each with a fresh entry id. `Next` follows the
    /// `play_next` rule; `End` leaves the insertion point alone. When idle
    /// the first song becomes current instead. The songs join the cycle.
    pub fn add_many(&mut self, songs: Vec<Song>, at: AddAt) -> AddOutcome {
        if self.current.is_none() {
            self.cycle.clear();
        }
        self.cycle.extend(songs.iter().cloned());
        self.insert_many(songs, at)
    }

    /// Adds one autoplay suggestion at the end. It stays out of the cycle.
    pub fn add_suggestion(&mut self, song: Song) {
        self.insert_many(vec![song], AddAt::End);
    }

    /// Appends one entry per cycle song to the end of `upcoming` and returns
    /// their ids. Nothing is added when the cycle is empty.
    pub fn append_cycle(&mut self) -> Vec<EntryId> {
        let mut ids = Vec::with_capacity(self.cycle.len());
        for song in self.cycle.clone() {
            self.next_id += 1;
            ids.push(self.next_id);
            self.upcoming.push_back(QueueEntry {
                id: self.next_id,
                song,
            });
        }
        ids
    }

    /// Drops the upcoming entries named in `ids` (loop copies). The cycle is
    /// left alone.
    pub fn drop_upcoming(&mut self, ids: &HashSet<EntryId>) {
        let mut index = 0;
        self.upcoming.retain(|e| {
            let keep = !ids.contains(&e.id);
            if !keep && index < self.next_insert {
                self.next_insert -= 1;
            }
            index += 1;
            keep
        });
    }

    /// Whether Repeat all has songs to loop.
    pub fn has_cycle(&self) -> bool {
        !self.cycle.is_empty()
    }

    fn insert_many(&mut self, songs: Vec<Song>, at: AddAt) -> AddOutcome {
        let count = songs.len();
        let mut started = false;
        for song in songs {
            if self.current.is_none() {
                self.next_id += 1;
                self.current = Some(QueueEntry {
                    id: self.next_id,
                    song,
                });
                self.next_insert = 0;
                started = true;
                continue;
            }
            match at {
                AddAt::Next => {
                    self.insert_next(song);
                }
                AddAt::End => {
                    self.next_id += 1;
                    self.upcoming.push_back(QueueEntry {
                        id: self.next_id,
                        song,
                    });
                }
            }
        }
        AddOutcome { count, started }
    }

    /// Replaces the whole queue: the first song becomes current, the rest
    /// upcoming in order, all with fresh entry ids. Returns the new current
    /// id, or `None` (queue unchanged) when `songs` is empty.
    pub fn play_all(&mut self, songs: Vec<Song>) -> Option<EntryId> {
        if songs.is_empty() {
            return None;
        }
        let mut entries = songs.into_iter().map(|song| {
            self.next_id += 1;
            QueueEntry {
                id: self.next_id,
                song,
            }
        });
        let current = entries.next();
        let upcoming: VecDeque<QueueEntry> = entries.collect();
        self.cycle = current
            .iter()
            .chain(upcoming.iter())
            .map(|e| e.song.clone())
            .collect();
        self.retire_current();
        self.current = current;
        self.upcoming = upcoming;
        self.next_insert = 0;
        self.current.as_ref().map(|e| e.id)
    }

    /// Removes an upcoming entry. The current entry can't be removed.
    pub fn remove(&mut self, id: EntryId) -> bool {
        let Some(index) = self.upcoming.iter().position(|e| e.id == id) else {
            return false;
        };
        if let Some(removed) = self.upcoming.remove(index)
            && let Some(at) = self
                .cycle
                .iter()
                .position(|s| s.video_id == removed.song.video_id)
        {
            self.cycle.remove(at);
        }
        if index < self.next_insert {
            self.next_insert -= 1;
        }
        true
    }

    /// Moves an upcoming entry to just after the upcoming entry `after`, or
    /// to the top of `upcoming` when `after` is `None`. Naming a neighbour
    /// rather than an index keeps the move right if the queue advanced
    /// meanwhile. Returns false, with nothing changed, when either id isn't
    /// upcoming. The Play next insertion point keeps pointing just past the
    /// songs that were added with Play next, wherever they now are.
    pub fn move_entry(&mut self, id: EntryId, after: Option<EntryId>) -> bool {
        let Some(from) = self.upcoming.iter().position(|e| e.id == id) else {
            return false;
        };
        if after == Some(id) {
            return true;
        }
        if after.is_some_and(|a| !self.upcoming.iter().any(|e| e.id == a)) {
            return false;
        }
        let entry = self.upcoming.remove(from).expect("index found above");
        if from < self.next_insert {
            self.next_insert -= 1;
        }
        let to = match after {
            None => 0,
            Some(anchor) => {
                self.upcoming
                    .iter()
                    .position(|e| e.id == anchor)
                    .expect("checked above")
                    + 1
            }
        };
        self.upcoming.insert(to, entry);
        if to < self.next_insert {
            self.next_insert += 1;
        }
        true
    }

    /// Makes a fresh entry for `song` the current one. `upcoming` is kept.
    /// The cycle becomes `song` followed by the upcoming songs.
    pub fn replace_current(&mut self, song: Song) -> EntryId {
        self.retire_current();
        self.cycle = std::iter::once(song.clone())
            .chain(self.upcoming.iter().map(|e| e.song.clone()))
            .collect();
        self.next_id += 1;
        self.current = Some(QueueEntry {
            id: self.next_id,
            song,
        });
        self.next_insert = 0;
        self.next_id
    }

    /// Moves the head of `upcoming` into `current`; `None` means idle.
    pub fn advance(&mut self) -> Option<&QueueEntry> {
        self.retire_current();
        self.step()
    }

    /// Like `advance`, but the old current entry (a song that failed to
    /// open) is not kept for `back`.
    pub fn advance_discarding(&mut self) -> Option<&QueueEntry> {
        self.current = None;
        self.step()
    }

    /// Steps back to the most recently played entry, keeping its id. The
    /// entry we leave becomes the head of `upcoming`. Returns false, with
    /// nothing changed, when there is no history.
    pub fn back(&mut self) -> bool {
        let Some(previous) = self.played.pop_back() else {
            return false;
        };
        if let Some(left) = self.current.replace(previous) {
            self.upcoming.push_front(left);
        }
        self.next_insert = 0;
        true
    }

    fn step(&mut self) -> Option<&QueueEntry> {
        self.current = self.upcoming.pop_front();
        self.next_insert = 0;
        self.current.as_ref()
    }

    /// Moves `current` into the back-history.
    fn retire_current(&mut self) {
        if let Some(entry) = self.current.take() {
            self.played.push_back(entry);
            if self.played.len() > MAX_BACK {
                self.played.pop_front();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn song(id: &str) -> Song {
        Song {
            video_id: id.into(),
            title: id.into(),
            artists: "artist".into(),
            album: None,
            duration_secs: None,
            artwork_url: None,
        }
    }

    fn upcoming_ids(q: &Queue) -> Vec<&str> {
        q.upcoming()
            .iter()
            .map(|e| e.song.video_id.as_str())
            .collect()
    }

    fn current_id(q: &Queue) -> Option<&str> {
        q.current().map(|e| e.song.video_id.as_str())
    }

    #[test]
    fn first_add_starts_playing() {
        let mut q = Queue::new();
        let out = q.play_next(song("A"));
        assert!(out.started);
        assert_eq!(current_id(&q), Some("A"));
        assert_eq!(q.current().unwrap().id, out.id);
    }

    #[test]
    fn play_next_order_is_fifo_after_current() {
        let mut q = Queue::new();
        q.play_next(song("A"));
        assert!(!q.play_next(song("X")).started);
        assert!(!q.play_next(song("Y")).started);
        assert_eq!(current_id(&q), Some("A"));
        assert_eq!(upcoming_ids(&q), ["X", "Y"]);
    }

    #[test]
    fn play_next_goes_before_older_items_after_advance() {
        let mut q = Queue::new();
        q.play_next(song("A"));
        q.play_next(song("X"));
        q.play_next(song("Y"));
        assert_eq!(
            q.advance().map(|e| e.song.video_id.clone()).as_deref(),
            Some("X")
        );
        q.play_next(song("Z"));
        assert_eq!(upcoming_ids(&q), ["Z", "Y"]);
    }

    #[test]
    fn same_song_twice_gets_distinct_ids() {
        let mut q = Queue::new();
        let a = q.play_next(song("A"));
        let b = q.play_next(song("A"));
        assert_ne!(a.id, b.id);
        assert!(b.id > a.id);
    }

    #[test]
    fn remove_upcoming_by_id() {
        let mut q = Queue::new();
        q.play_next(song("A"));
        let x = q.play_next(song("X"));
        q.play_next(song("Y"));
        assert!(q.remove(x.id));
        assert_eq!(upcoming_ids(&q), ["Y"]);
    }

    #[test]
    fn remove_unknown() {
        let mut q = Queue::new();
        q.play_next(song("A"));
        q.play_next(song("X"));
        assert!(!q.remove(999));
        assert_eq!(upcoming_ids(&q), ["X"]);
    }

    #[test]
    fn remove_current_is_rejected() {
        let mut q = Queue::new();
        let a = q.play_next(song("A"));
        q.play_next(song("X"));
        assert!(!q.remove(a.id));
        assert_eq!(current_id(&q), Some("A"));
        assert_eq!(upcoming_ids(&q), ["X"]);
    }

    #[test]
    fn remove_before_insert_point_keeps_order() {
        let mut q = Queue::new();
        q.play_next(song("A"));
        let x = q.play_next(song("X"));
        q.play_next(song("Y"));
        assert!(q.remove(x.id));
        q.play_next(song("Z"));
        assert_eq!(upcoming_ids(&q), ["Y", "Z"]);
    }

    #[test]
    fn move_to_top_and_after_another() {
        let mut q = Queue::new();
        q.play_next(song("A"));
        q.add_many(vec![song("B"), song("C"), song("D")], AddAt::End);
        let ids: Vec<EntryId> = q.upcoming().iter().map(|e| e.id).collect();
        assert!(q.move_entry(ids[2], None));
        assert_eq!(upcoming_ids(&q), ["D", "B", "C"]);
        assert!(q.move_entry(ids[0], Some(ids[1])));
        assert_eq!(upcoming_ids(&q), ["D", "C", "B"]);
        assert!(q.move_entry(ids[2], Some(ids[0])));
        assert_eq!(upcoming_ids(&q), ["C", "B", "D"]);
    }

    #[test]
    fn move_after_itself_changes_nothing() {
        let mut q = Queue::new();
        q.play_next(song("A"));
        let x = q.play_next(song("X"));
        q.play_next(song("Y"));
        assert!(q.move_entry(x.id, Some(x.id)));
        assert_eq!(upcoming_ids(&q), ["X", "Y"]);
    }

    #[test]
    fn move_unknown_current_or_bad_anchor_is_rejected() {
        let mut q = Queue::new();
        let a = q.play_next(song("A"));
        let x = q.play_next(song("X"));
        q.play_next(song("Y"));
        assert!(!q.move_entry(999, None));
        assert!(!q.move_entry(a.id, None));
        assert!(!q.move_entry(x.id, Some(a.id)));
        assert!(!q.move_entry(x.id, Some(999)));
        assert_eq!(upcoming_ids(&q), ["X", "Y"]);
    }

    #[test]
    fn move_keeps_play_next_after_the_play_next_block() {
        // X, Y came with Play next; B with Add to queue. Moving B to the top
        // puts it ahead of the block; the next Play next still follows Y.
        let mut q = Queue::new();
        q.play_next(song("A"));
        q.add_many(vec![song("B")], AddAt::End);
        let x = q.play_next(song("X"));
        q.play_next(song("Y"));
        assert_eq!(upcoming_ids(&q), ["X", "Y", "B"]);
        let b = q.upcoming()[2].id;
        assert!(q.move_entry(b, None));
        q.play_next(song("Z"));
        assert_eq!(upcoming_ids(&q), ["B", "X", "Y", "Z"]);
        // Moving X out of the block, to the end, shrinks it.
        assert!(q.move_entry(x.id, Some(q.upcoming()[3].id)));
        q.play_next(song("W"));
        assert_eq!(upcoming_ids(&q), ["B", "Y", "Z", "W", "X"]);
    }

    #[test]
    fn play_all_replaces_everything() {
        let mut q = Queue::new();
        let a = q.play_next(song("A"));
        q.play_next(song("X"));
        let id = q.play_all(vec![song("B"), song("C"), song("D")]).unwrap();
        assert_eq!(current_id(&q), Some("B"));
        assert_eq!(q.current().unwrap().id, id);
        assert_eq!(upcoming_ids(&q), ["C", "D"]);
        assert!(id > a.id, "fresh entry ids");
    }

    #[test]
    fn play_all_on_idle_queue() {
        let mut q = Queue::new();
        assert!(q.play_all(vec![song("B")]).is_some());
        assert_eq!(current_id(&q), Some("B"));
        assert!(q.upcoming().is_empty());
    }

    #[test]
    fn play_all_empty_changes_nothing() {
        let mut q = Queue::new();
        q.play_next(song("A"));
        q.play_next(song("X"));
        assert_eq!(q.play_all(Vec::new()), None);
        assert_eq!(current_id(&q), Some("A"));
        assert_eq!(upcoming_ids(&q), ["X"]);
    }

    #[test]
    fn play_all_resets_insertion_point() {
        let mut q = Queue::new();
        q.play_next(song("A"));
        q.play_next(song("X"));
        q.play_all(vec![song("B"), song("C")]);
        q.play_next(song("Z"));
        assert_eq!(upcoming_ids(&q), ["Z", "C"]);
    }

    #[test]
    fn advance_on_last_goes_idle() {
        let mut q = Queue::new();
        q.play_next(song("A"));
        assert!(q.advance().is_none());
        assert!(q.current().is_none());
        // Adding after idle starts playing again.
        assert!(q.play_next(song("B")).started);
    }

    #[test]
    fn replace_current_keeps_upcoming() {
        let mut q = Queue::new();
        let a = q.play_next(song("A"));
        q.play_next(song("B"));
        q.play_next(song("C"));
        let id = q.replace_current(song("X"));
        assert_eq!(current_id(&q), Some("X"));
        assert_eq!(q.current().unwrap().id, id);
        assert!(id > a.id, "fresh entry id");
        assert_eq!(upcoming_ids(&q), ["B", "C"]);
        q.play_next(song("Z"));
        assert_eq!(upcoming_ids(&q), ["Z", "B", "C"]);
    }

    #[test]
    fn add_many_next_keeps_order_after_earlier_play_next() {
        let mut q = Queue::new();
        q.play_next(song("A"));
        q.play_next(song("X"));
        let out = q.add_many(vec![song("Y"), song("Z")], AddAt::Next);
        assert_eq!(
            out,
            AddOutcome {
                count: 2,
                started: false
            }
        );
        assert_eq!(upcoming_ids(&q), ["X", "Y", "Z"]);
        q.play_next(song("W"));
        assert_eq!(upcoming_ids(&q), ["X", "Y", "Z", "W"]);
    }

    #[test]
    fn add_many_end_appends_and_leaves_insert_point() {
        let mut q = Queue::new();
        q.play_next(song("A"));
        q.play_next(song("B"));
        q.add_many(vec![song("Y")], AddAt::End);
        assert_eq!(upcoming_ids(&q), ["B", "Y"]);
        q.play_next(song("X"));
        assert_eq!(upcoming_ids(&q), ["B", "X", "Y"]);
    }

    #[test]
    fn add_many_when_idle_starts_first_song() {
        let mut q = Queue::new();
        let out = q.add_many(vec![song("Y"), song("Z")], AddAt::End);
        assert_eq!(
            out,
            AddOutcome {
                count: 2,
                started: true
            }
        );
        assert_eq!(current_id(&q), Some("Y"));
        assert_eq!(upcoming_ids(&q), ["Z"]);
    }

    #[test]
    fn add_many_assigns_fresh_ids() {
        let mut q = Queue::new();
        let a = q.play_next(song("A"));
        q.add_many(vec![song("A"), song("A")], AddAt::End);
        q.add_many(vec![song("A")], AddAt::Next);
        let mut ids: Vec<EntryId> = q.upcoming().iter().map(|e| e.id).collect();
        ids.sort_unstable();
        assert!(ids[0] > a.id);
        assert!(ids.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn back_without_history_changes_nothing() {
        let mut q = Queue::new();
        q.play_next(song("A"));
        q.play_next(song("X"));
        assert!(!q.back());
        assert_eq!(current_id(&q), Some("A"));
        assert_eq!(upcoming_ids(&q), ["X"]);
    }

    #[test]
    fn back_restores_the_entry_and_requeues_the_one_left() {
        let mut q = Queue::new();
        let a = q.play_next(song("A")).id;
        q.play_next(song("X"));
        q.play_next(song("Y"));
        q.advance();
        let x = q.current().unwrap().id;
        assert!(q.back());
        assert_eq!(q.current().unwrap().id, a);
        assert_eq!(upcoming_ids(&q), ["X", "Y"]);
        assert_eq!(q.upcoming()[0].id, x);
        assert!(!q.back(), "history is used up");
    }

    #[test]
    fn back_steps_through_several_songs() {
        let mut q = Queue::new();
        q.play_all(vec![song("A"), song("B"), song("C")]);
        q.advance();
        q.advance();
        assert!(q.back());
        assert_eq!(current_id(&q), Some("B"));
        assert!(q.back());
        assert_eq!(current_id(&q), Some("A"));
        assert_eq!(upcoming_ids(&q), ["B", "C"]);
    }

    #[test]
    fn back_resets_the_insertion_point() {
        let mut q = Queue::new();
        q.play_all(vec![song("A"), song("B")]);
        q.advance();
        q.play_next(song("Z"));
        assert!(q.back());
        q.play_next(song("W"));
        assert_eq!(upcoming_ids(&q), ["W", "B", "Z"]);
    }

    #[test]
    fn play_all_and_replace_current_keep_the_old_current() {
        let mut q = Queue::new();
        q.play_next(song("A"));
        q.play_all(vec![song("B")]);
        q.replace_current(song("C"));
        assert!(q.back());
        assert_eq!(current_id(&q), Some("B"));
        assert!(q.back());
        assert_eq!(current_id(&q), Some("A"));
        assert!(!q.back());
    }

    #[test]
    fn advance_discarding_skips_the_history() {
        let mut q = Queue::new();
        q.play_all(vec![song("A"), song("B"), song("C")]);
        q.advance();
        q.advance_discarding();
        assert_eq!(current_id(&q), Some("C"));
        assert!(q.back());
        assert_eq!(current_id(&q), Some("A"));
    }

    #[test]
    fn history_is_capped_dropping_the_oldest() {
        let mut q = Queue::new();
        q.play_all((0..MAX_BACK + 10).map(|i| song(&i.to_string())).collect());
        for _ in 0..MAX_BACK + 5 {
            q.advance();
        }
        let mut steps = 0;
        while q.back() {
            steps += 1;
        }
        assert_eq!(steps, MAX_BACK);
        assert_eq!(current_id(&q), Some("5"));
    }

    fn looped_ids(q: &mut Queue) -> Vec<String> {
        let ids = q.append_cycle();
        q.upcoming()
            .iter()
            .filter(|e| ids.contains(&e.id))
            .map(|e| e.song.video_id.clone())
            .collect()
    }

    #[test]
    fn play_all_sets_the_cycle() {
        let mut q = Queue::new();
        q.play_all(vec![song("A"), song("B"), song("C")]);
        q.advance();
        q.advance();
        assert!(q.upcoming().is_empty());
        assert_eq!(looped_ids(&mut q), ["A", "B", "C"]);
    }

    #[test]
    fn play_all_again_replaces_the_cycle() {
        let mut q = Queue::new();
        q.play_all(vec![song("A"), song("B")]);
        q.play_all(vec![song("X")]);
        assert_eq!(looped_ids(&mut q), ["X"]);
    }

    #[test]
    fn added_songs_join_the_cycle_but_suggestions_do_not() {
        let mut q = Queue::new();
        q.play_all(vec![song("A")]);
        q.play_next(song("B"));
        q.add_many(vec![song("C")], AddAt::End);
        q.add_suggestion(song("S"));
        assert_eq!(upcoming_ids(&q), ["B", "C", "S"]);
        assert_eq!(looped_ids(&mut q), ["A", "B", "C"]);
    }

    #[test]
    fn adding_to_an_idle_queue_starts_a_new_cycle() {
        let mut q = Queue::new();
        q.play_all(vec![song("A"), song("B")]);
        q.advance();
        q.advance();
        assert!(q.current().is_none());
        q.add_many(vec![song("X"), song("Y")], AddAt::End);
        assert_eq!(looped_ids(&mut q), ["X", "Y"]);
    }

    #[test]
    fn remove_drops_one_occurrence_from_the_cycle() {
        let mut q = Queue::new();
        q.play_all(vec![song("A"), song("B"), song("B")]);
        let b = q.upcoming()[0].id;
        assert!(q.remove(b));
        assert_eq!(looped_ids(&mut q), ["A", "B"]);
    }

    #[test]
    fn replace_current_resets_the_cycle_to_it_plus_upcoming() {
        let mut q = Queue::new();
        q.play_all(vec![song("A"), song("B"), song("C")]);
        q.replace_current(song("S"));
        assert_eq!(looped_ids(&mut q), ["S", "B", "C"]);
    }

    #[test]
    fn append_cycle_with_no_cycle_adds_nothing() {
        let mut q = Queue::new();
        assert!(!q.has_cycle());
        assert!(q.append_cycle().is_empty());
    }

    #[test]
    fn drop_upcoming_removes_the_named_entries_only() {
        let mut q = Queue::new();
        q.play_all(vec![song("A"), song("B")]);
        let ids = q.append_cycle();
        q.drop_upcoming(&ids.into_iter().collect());
        assert_eq!(upcoming_ids(&q), ["B"]);
        assert_eq!(looped_ids(&mut q), ["A", "B"]);
    }
}
