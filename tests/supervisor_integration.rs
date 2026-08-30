use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use file_indexr::config::Config;
use file_indexr::index::supervisor;
use file_indexr::index::writer::IndexWriterWrapper;
use tokio::sync::watch;

fn make_temp_dir() -> PathBuf {
    file_indexr::testutil::unique_temp_dir("integ_test")
}

async fn make_config(watch_dir: PathBuf, index_dir: PathBuf) -> Arc<Config> {
    let mut config = Config {
        directory: watch_dir,
        index_path: index_dir,
        port: 0,
        bind: "127.0.0.1".to_string(),
        max_file_size_mb: 2,
        batch_size: 500,
        batch_timeout_ms: 1000,
        allowed_extensions: vec![],
    };
    // Resolve to canonical paths so the scanner/watcher index-exclusion
    // comparisons run in one canonical space.
    config.canonicalize_paths().await.unwrap();
    Arc::new(config)
}

async fn make_writer(config: Arc<Config>) -> Arc<IndexWriterWrapper> {
    let writer = IndexWriterWrapper::new(
        &config.index_path,
        config.clone(),
        file_indexr::formats::new_text_data_cache(),
    )
    .await
    .unwrap();
    Arc::new(writer)
}

/// Shutdown-before-restart regression: a shutdown flag set while the
/// supervisor is (re)starting must terminate it promptly. A fresh
/// `watch::Receiver` clone treats an already-set flag as seen, so the
/// pre-attempt check must use the original receiver — otherwise a restarted
/// watcher would never observe the flag and the process would hang through
/// the backoff sleep.
#[tokio::test]
async fn test_shutdown_before_restart_exits_without_backoff() {
    let watch_dir = make_temp_dir();
    let index_dir = make_temp_dir();
    let config = make_config(watch_dir, index_dir).await;
    let writer = make_writer(config.clone()).await;

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let (done_tx, done_rx) = watch::channel(false);

    let task = tokio::spawn(supervisor::run(config, writer, shutdown_rx, done_tx));

    // Request shutdown immediately: the supervisor must exit without sitting
    // out a backoff sleep (which would take at least 5 seconds).
    shutdown_tx.send(true).unwrap();

    // 3 s comfortably exceeds startup latency yet stays under the 5 s first
    // backoff, so a supervisor that slept through a backoff would fail here.
    tokio::time::timeout(Duration::from_secs(3), task)
        .await
        .expect("supervisor did not exit promptly after shutdown")
        .unwrap();

    // `run` must signal done before exiting: main's graceful shutdown
    // selects on it, and a missing signal would hang the process.
    assert!(*done_rx.borrow(), "supervisor did not signal done");
}

/// Happy path: the supervisor watches the directory, indexes a newly written
/// file, and shuts down cleanly on request.
#[tokio::test]
async fn test_file_change_is_indexed_and_shutdown_is_clean() {
    let watch_dir = make_temp_dir();
    let index_dir = make_temp_dir();
    let config = make_config(watch_dir.clone(), index_dir).await;
    let writer = make_writer(config.clone()).await;

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let (done_tx, done_rx) = watch::channel(false);

    let task = tokio::spawn(supervisor::run(
        config,
        writer.clone(),
        shutdown_rx,
        done_tx,
    ));

    // Give the watcher a moment to register before writing.
    tokio::time::sleep(Duration::from_millis(500)).await;
    std::fs::write(
        watch_dir.join("supervisor_test.txt"),
        "supervisor happy path marker",
    )
    .unwrap();

    // The watcher debounces for 1 s and the batch commit fires on a 1 s
    // timer, so the doc is visible only after both — allow plenty of time.
    let deadline = tokio::time::sleep(Duration::from_secs(30));
    tokio::pin!(deadline);
    loop {
        if writer.doc_count().unwrap() > 0 {
            break;
        }
        tokio::select! {
            _ = &mut deadline => panic!("file was not indexed within 30 s"),
            _ = tokio::time::sleep(Duration::from_millis(200)) => {}
        }
    }

    shutdown_tx.send(true).unwrap();

    tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("supervisor did not exit after shutdown")
        .unwrap();

    assert!(*done_rx.borrow(), "supervisor did not signal done");
}

/// Retry/give-up path: with every start attempt failing, the supervisor must
/// exhaust its retry budget (7 attempts) and exit, signaling done. A tiny
/// injected backoff keeps this fast — with the production backoff the same
/// scenario would take at least 315 s of real time.
#[tokio::test]
async fn test_retry_exhausts_attempts_and_gives_up() {
    let watch_dir = make_temp_dir();
    let index_dir = make_temp_dir();
    let config = make_config(watch_dir.clone(), index_dir).await;
    let writer = make_writer(config.clone()).await;

    // Removing the watched dir makes every start_watching attempt fail
    // (notify cannot watch a nonexistent path).
    std::fs::remove_dir(&watch_dir).unwrap();

    let (_shutdown_tx, shutdown_rx) = watch::channel(false);
    let (done_tx, done_rx) = watch::channel(false);

    let task = tokio::spawn(supervisor::run_with_backoff(
        config,
        writer,
        shutdown_rx,
        done_tx,
        |_| Duration::from_millis(1),
    ));

    // 7 attempts × 1 ms of injected backoff is far under this budget; the
    // production backoff would take at least 315 s.
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("supervisor did not give up within 5 s")
        .unwrap();

    assert!(*done_rx.borrow(), "supervisor did not signal done");
}

/// Interruptible backoff sleep: a shutdown request arriving during the
/// backoff sleep must terminate the supervisor promptly, not after the
/// injected delay elapses.
#[tokio::test]
async fn test_shutdown_interrupts_backoff_sleep() {
    let watch_dir = make_temp_dir();
    let index_dir = make_temp_dir();
    let config = make_config(watch_dir.clone(), index_dir).await;
    let writer = make_writer(config.clone()).await;

    // Removing the watched dir makes the first start_watching attempt fail
    // immediately, putting the supervisor inside its backoff sleep.
    std::fs::remove_dir(&watch_dir).unwrap();

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let (done_tx, done_rx) = watch::channel(false);

    let task = tokio::spawn(supervisor::run_with_backoff(
        config,
        writer,
        shutdown_rx,
        done_tx,
        |_| Duration::from_secs(10),
    ));

    // Wait for the first attempt to fail so the supervisor is inside the
    // 10 s injected sleep, then request shutdown.
    tokio::time::sleep(Duration::from_secs(1)).await;
    shutdown_tx.send(true).unwrap();

    // 3 s is well under the injected 10 s sleep: without the interruptible
    // select! the supervisor would not exit until ~10 s.
    tokio::time::timeout(Duration::from_secs(3), task)
        .await
        .expect("supervisor did not exit within 3 s of shutdown")
        .unwrap();

    assert!(*done_rx.borrow(), "supervisor did not signal done");
}
