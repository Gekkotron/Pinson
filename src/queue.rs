//! The "play next" queue (spec §9).
//!
//! `play_next` inserts at `next_insert` inside `upcoming` and moves the
//! insertion point forward, so songs added with Play next keep their order.
//! The insertion point resets whenever `current` changes.

use std::collections::VecDeque;

use serde::Serialize;

use crate::song::Song;

pub type EntryId = u64;

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

#[derive(Debug, Default)]
pub struct Queue {
    current: Option<QueueEntry>,
    upcoming: VecDeque<QueueEntry>,
    next_insert: usize,
    next_id: EntryId,
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
        self.upcoming.remove(index);
        if index < self.next_insert {
            self.next_insert -= 1;
        }
        true
    }

    /// Moves the head of `upcoming` into `current`; `None` means idle.
    pub fn advance(&mut self) -> Option<&QueueEntry> {
        self.current = self.upcoming.pop_front();
        self.next_insert = 0;
        self.current.as_ref()
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
}
