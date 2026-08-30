use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::change::FileChange;

/// Debounce `Modified` events for the same file path within a time window.
/// Multiple events for the same path are merged into a single event.
pub struct Debouncer {
    /// Path -> last event time.
    pending: HashMap<PathBuf, Instant>,
    interval: Duration,
}

impl Debouncer {
    pub fn new(interval: Duration) -> Self {
        Self {
            pending: HashMap::new(),
            interval,
        }
    }

    /// Add a `Modified` event for `path`. If the debounce window for this path
    /// has expired, emit the new event immediately and do not buffer it.
    /// Otherwise, store it pending — it will be emitted via [tick()][Self::tick]
    /// or [flush()][Self::flush].
    pub fn add(&mut self, path: PathBuf, now: Instant) -> Option<FileChange> {
        match self.pending.entry(path) {
            // Window already expired: drop the stale entry and emit immediately.
            Entry::Occupied(occ) if now.duration_since(*occ.get()) >= self.interval => {
                // The key is consumed by `entry`, so clone it back out for the
                // emitted event (rare path: window already expired).
                let change = FileChange::Modified(occ.key().clone());
                occ.remove();
                Some(change)
            }
            // Within the window: refresh the timestamp.
            Entry::Occupied(mut occ) => {
                *occ.get_mut() = now;
                None
            }
            // New path: buffer the event.
            Entry::Vacant(vac) => {
                vac.insert(now);
                None
            }
        }
    }

    /// Flush all pending events. Returns all accumulated changes.
    pub fn flush(&mut self) -> Vec<FileChange> {
        self.pending
            .drain()
            .map(|(path, _)| FileChange::Modified(path))
            .collect()
    }

    /// Get all events that have exceeded the debounce interval.
    pub fn tick(&mut self, now: Instant) -> Vec<FileChange> {
        let mut expired = Vec::new();
        self.pending.retain(|path, last_time| {
            if now.duration_since(*last_time) < self.interval {
                return true;
            }
            // `retain` borrows the key, so clone it for the emitted event;
            // the `false` return removes the entry in the same pass.
            expired.push(FileChange::Modified(path.clone()));
            false
        });
        expired
    }

    /// Number of pending events.
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_debouncer_single_event() {
        let mut debouncer = Debouncer::new(Duration::from_millis(200));
        let now = Instant::now();

        let result = debouncer.add(PathBuf::from("a.txt"), now);
        assert!(result.is_none());
        assert_eq!(debouncer.pending_count(), 1);
    }

    #[test]
    fn test_debouncer_merge_events() {
        let mut debouncer = Debouncer::new(Duration::from_millis(200));

        let t1 = Instant::now();
        debouncer.add(PathBuf::from("a.txt"), t1);

        let t2 = t1 + Duration::from_millis(50);
        let result = debouncer.add(PathBuf::from("a.txt"), t2);
        // Within debounce window, no event emitted yet
        assert!(result.is_none());
        assert_eq!(debouncer.pending_count(), 1);
    }

    #[test]
    fn test_debouncer_tick_emits_expired() {
        let mut debouncer = Debouncer::new(Duration::from_millis(200));

        let t1 = Instant::now() - Duration::from_millis(300);
        debouncer.add(PathBuf::from("a.txt"), t1);

        let now = Instant::now();
        let events = debouncer.tick(now);
        assert_eq!(events.len(), 1);
        assert_eq!(debouncer.pending_count(), 0);
    }

    #[test]
    fn test_debouncer_tick_keeps_fresh_entries() {
        let mut debouncer = Debouncer::new(Duration::from_millis(200));

        let t1 = Instant::now() - Duration::from_millis(300);
        debouncer.add(PathBuf::from("old.txt"), t1);

        let now = Instant::now();
        debouncer.add(PathBuf::from("fresh.txt"), now);

        let events = debouncer.tick(now);
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], FileChange::Modified(ref p) if p.as_path() == "old.txt"));
        assert_eq!(debouncer.pending_count(), 1);
    }

    #[test]
    fn test_debouncer_flush() {
        let mut debouncer = Debouncer::new(Duration::from_millis(200));
        let now = Instant::now();

        debouncer.add(PathBuf::from("a.txt"), now);
        debouncer.add(PathBuf::from("b.txt"), now);

        let events = debouncer.flush();
        assert_eq!(events.len(), 2);
        assert_eq!(debouncer.pending_count(), 0);
    }
}
