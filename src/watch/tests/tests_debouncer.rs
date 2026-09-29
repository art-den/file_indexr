use std::{path::PathBuf, time::{Duration, Instant}};

use crate::watch::{FileChange, debouncer::*};

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
