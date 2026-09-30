use std::time::Duration;

use super::*;

#[test]
fn test_backoff_delay_sequence() {
    assert_eq!(backoff_delay(1), Duration::from_secs(5));
    assert_eq!(backoff_delay(2), Duration::from_secs(10));
    assert_eq!(backoff_delay(3), Duration::from_secs(20));
    // The 7th failure gives up without sleeping, but the cap branch of
    // the pure function is exercised by the attempt-7 value.
    assert_eq!(
        backoff_delay(7),
        Duration::from_secs(MAX_WATCHER_RETRY_DELAY_SECS)
    );
    assert_eq!(
        backoff_delay(100),
        Duration::from_secs(MAX_WATCHER_RETRY_DELAY_SECS)
    );

    // The sequence must be non-decreasing.
    let mut previous = backoff_delay(1);
    for attempt in 2..=100 {
        let current = backoff_delay(attempt);
        assert!(
            current >= previous,
            "backoff decreased at attempt {attempt}"
        );
        previous = current;
    }
}
