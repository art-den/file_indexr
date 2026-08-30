use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use clap::Parser;
use file_indexr::config::{self, Args, TransportMode};
use file_indexr::index::{IndexCoordinator, writer::IndexWriterWrapper};
use file_indexr::{AppState, api};
use tokio::sync::watch;
use tracing::{error, info};

#[tokio::main]
async fn main() -> Result<()> {
    // 1. Parse CLI arguments
    let args = Args::parse();

    // 2. Initialize logging
    init_logging(args.verbose);

    // 3. Load configuration (file + CLI merge)
    let file_config = match args.config.as_ref() {
        Some(path) => config::load_config_file(path)
            .await
            .inspect_err(|e| {
                error!(error = %e, path = %path.display(), "Failed to load config file, using defaults");
            })
            .ok(),
        None => None,
    };

    let (mut app_config, transport) =
        config::merge_config(args, file_config).map_err(anyhow::Error::msg)?;

    app_config.validate().map_err(anyhow::Error::msg)?;

    // Canonicalize directory and index path so that the index-exclusion
    // comparisons in the scanner/watcher are not broken by symlinks, `..`
    // or relative path forms.
    app_config
        .canonicalize_paths()
        .await
        .map_err(anyhow::Error::msg)?;

    let config = Arc::new(app_config);

    info!(
        directory = %config.directory.display(),
        index_path = %config.index_path.display(),
        port = config.port,
        bind = %config.bind,
        transport = ?transport,
        "Starting FileIndexr"
    );

    // 4. Create shared index writer
    let text_data_cache = file_indexr::formats::new_text_data_cache();
    let writer = Arc::new(
        IndexWriterWrapper::new(&config.index_path, config.clone(), text_data_cache.clone())
            .await?,
    );

    // 5. Run startup scan via coordinator
    let coordinator = IndexCoordinator::with_writer(writer.clone(), config.clone());

    // 6. Build shared application state for the HTTP server
    let start_time = Instant::now();
    let count = coordinator
        .startup_scan()
        .await
        .context("Startup scan failed")?;
    info!(files = count, elapsed = ?start_time.elapsed(), "Initial scan complete");

    let reader = writer
        .index()
        .reader()
        .context("Failed to create index reader")?;

    let app_state = AppState {
        reader: Arc::new(reader),
        config: config.clone(),
        writer,
        text_data_cache,
    };

    // 7. Launch based on transport mode
    match transport {
        TransportMode::Http => run_http_server(app_state).await?,
        TransportMode::Stdio => run_stdio_mode(app_state).await?,
    }

    info!("FileIndexr stopped");
    Ok(())
}

async fn run_http_server(app_state: AppState) -> Result<()> {
    let (watcher_shutdown_tx, mut watcher_done_rx, watcher_task) =
        spawn_watcher(app_state.config.clone(), app_state.writer.clone());

    let addr = format!("{}:{}", app_state.config.bind, app_state.config.port);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("Failed to bind to {}", addr))?;

    let server_addr = listener.local_addr()?;
    info!(
        address = %server_addr,
        "HTTP server started — FileIndexr is ready"
    );

    let app = api::create_router(app_state);
    let server = axum::serve(listener, app).with_graceful_shutdown(async move {
        tokio::select! {
            _ = shutdown_signal() => {},
            _ = watcher_done_rx.changed() => {
                info!("Watcher exited — triggering graceful shutdown");
            }
        }
    });

    match server.await {
        Ok(()) => info!("HTTP server shut down gracefully"),
        Err(e) => error!(error = %e, "HTTP server error"),
    }

    stop_watcher(watcher_shutdown_tx, watcher_task).await;

    Ok(())
}

async fn run_stdio_mode(state: AppState) -> Result<()> {
    use file_indexr::mcp::stdio::run_stdio_loop;

    let (watcher_shutdown_tx, mut watcher_done_rx, watcher_task) =
        spawn_watcher(state.config.clone(), state.writer.clone());

    // Run the MCP STDIO loop. Graceful shutdown via SIGINT/SIGTERM.
    let stdio_task = tokio::spawn(run_stdio_loop(state));

    // A failed STDIO loop (or a panic) is a real failure for the host
    // process: propagate it so the process exits non-zero. Normal
    // terminations (stdin EOF, client disconnect) return Ok.
    let loop_result = tokio::select! {
        result = stdio_task => match result {
            Ok(Ok(())) => {
                info!("STDIO loop completed");
                Ok(())
            }
            Ok(Err(e)) => {
                error!(error = %e, "STDIO loop error");
                Err(e)
            }
            Err(e) => {
                error!(error = %e, "STDIO task panicked");
                Err(anyhow::anyhow!("STDIO task panicked: {e}"))
            }
        },
        _ = shutdown_signal() => {
            info!("Shutdown signal received — stopping STDIO loop");
            Ok(())
        }
        _ = watcher_done_rx.changed() => {
            info!("Watcher exited — triggering graceful shutdown");
            Ok(())
        }
    };

    stop_watcher(watcher_shutdown_tx, watcher_task).await;

    loop_result
}

/// Spawn the file-watcher supervisor and return its shutdown sender,
/// "done" receiver, and the join handle.
fn spawn_watcher(
    config: Arc<config::Config>,
    writer: Arc<IndexWriterWrapper>,
) -> (
    watch::Sender<bool>,
    watch::Receiver<bool>,
    tokio::task::JoinHandle<()>,
) {
    let (watcher_shutdown_tx, watcher_shutdown_rx) = watch::channel(false);
    let (watcher_done_tx, watcher_done_rx) = watch::channel(false);
    let watcher_task = tokio::spawn(file_indexr::index::supervisor::run(
        config,
        writer,
        watcher_shutdown_rx,
        watcher_done_tx,
    ));
    (watcher_shutdown_tx, watcher_done_rx, watcher_task)
}

/// Signal the watcher to stop and wait for it to finish.
async fn stop_watcher(shutdown_tx: watch::Sender<bool>, task: tokio::task::JoinHandle<()>) {
    let _ = shutdown_tx.send(true);
    let _ = task.await;
}

/// Listen for shutdown signals (SIGINT, SIGTERM).
#[cfg(unix)]
async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut sigterm = signal(SignalKind::terminate()).expect("failed to set up SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {},
        _ = sigterm.recv() => {},
    }
    info!("Shutdown signal received");
}

#[cfg(not(unix))]
async fn shutdown_signal() {
    tokio::signal::ctrl_c().await.ok();
    info!("Shutdown signal received");
}

fn init_logging(verbose: bool) {
    let level = if verbose { "debug" } else { "info" };
    // Always write logs to stderr so stdout is clean for MCP transport payloads.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(level)),
        )
        .with_writer(std::io::stderr)
        .init();
}
