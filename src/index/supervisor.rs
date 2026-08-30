//! Watcher retry/supervisor logic: runs the file watcher and restarts it on
//! failure with exponential backoff, until either it stops normally or the
//! retry budget is exhausted.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use tracing::{error, info};

use crate::config::Config;
use crate::index::IndexCoordinator;
use crate::index::writer::IndexWriterWrapper;

/// Maximum number of watcher start attempts before giving up.
const MAX_WATCHER_RETRIES: u32 = 7;
/// Upper bound for the exponential backoff between watcher restarts.
const MAX_WATCHER_RETRY_DELAY_SECS: u64 = 300;

/// Backoff delay before the given (1-based) watcher start attempt:
/// `5 << (attempt - 1)` seconds, capped at `MAX_WATCHER_RETRY_DELAY_SECS`.
fn backoff_delay(attempt: u32) -> Duration {
    // 5 << 6 = 320 s already exceeds the cap, so any larger shift can only
    // produce the capped value.
    let shift = u32::min(u32::saturating_sub(attempt, 1), 6);
    Duration::from_secs(u64::min(5u64 << shift, MAX_WATCHER_RETRY_DELAY_SECS))
}

/// Run the file watcher with retry logic, using the production backoff
/// (`backoff_delay`).
///
/// Sends `true` on `done_tx` on **every** exit path (main's graceful
/// shutdown selects on it; missing a send would hang the process).
pub async fn run(
    config: Arc<Config>,
    writer: Arc<IndexWriterWrapper>,
    shutdown_rx: watch::Receiver<bool>,
    done_tx: watch::Sender<bool>,
) {
    run_with_backoff(config, writer, shutdown_rx, done_tx, backoff_delay).await
}

/// The engine of the watcher retry loop: identical to [`run`], but the
/// backoff delay between attempts is supplied by the caller instead of using
/// `backoff_delay`. `run` calls this with the production backoff; tests use
/// it with custom (tiny) delays so the retry/give-up path is testable without
/// real-time sleeps.
///
/// Sends `true` on `done_tx` on **every** exit path (main's graceful
/// shutdown selects on it; missing a send would hang the process).
pub async fn run_with_backoff(
    config: Arc<Config>,
    writer: Arc<IndexWriterWrapper>,
    mut shutdown_rx: watch::Receiver<bool>,
    done_tx: watch::Sender<bool>,
    backoff: impl Fn(u32) -> Duration + Send,
) {
    let mut attempt: u32 = 0;

    loop {
        // An explicit shutdown takes precedence over restarting: a fresh
        // watch::Receiver clone treats the already-set flag as seen, so a
        // restarted watcher would never observe it and shutdown would hang.
        if *shutdown_rx.borrow() {
            info!("Shutdown requested — not restarting file watcher");
            break;
        }
        let coordinator = IndexCoordinator::with_writer(writer.clone(), config.clone());
        match coordinator.start_watching(shutdown_rx.clone()).await {
            Ok(()) => {
                info!("File watcher stopped normally");
                break;
            }
            Err(e) => {
                attempt += 1;
                if attempt >= MAX_WATCHER_RETRIES {
                    error!(error = %e, attempts = attempt,
                        "File watcher failed repeatedly — giving up");
                    break;
                }
                let delay = backoff(attempt);
                error!(error = %e, attempt = attempt, delay_secs = delay.as_secs(),
                    "File watcher error — restarting");
                // Interruptible sleep: a shutdown signal arriving during the backoff
                // must not stall process exit until the sleep finishes.
                let _ = tokio::time::timeout(delay, shutdown_rx.changed()).await;
            }
        }
    }
    let _ = done_tx.send(true);
}

#[cfg(test)]
mod tests {
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
}
