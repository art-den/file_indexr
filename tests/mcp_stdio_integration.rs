//! Integration tests for MCP STDIO transport mode (`--stdio`).
//!
//! Spawns the real compiled binary and communicates via stdin/stdout pipes.
//! No changes to production code required — pure black-box testing.

use std::cell::RefCell;
use std::os::unix::io::AsRawFd;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use file_indexr::mcp::jsonrpc::{METHOD_NOT_FOUND, PARSE_ERROR};
use serde_json::Value;

mod common;
use common::prepare_mcp_files;
use file_indexr::testutil::unique_id;

// Timeout constants.
// Generous on purpose: tests run in parallel, and each spawns a child
// process with its own multi-threaded tokio runtime, so responses can be
// delayed under CPU contention.
const RESPONSE_TIMEOUT_SECS: u64 = 15;
const READY_TIMEOUT_SECS: u64 = 30;
// How long to wait for the child to exit after a client disconnect.
// The exit requires a multi-step chain (EPIPE on the response write → STDIO
// loop exit → watcher shutdown → final commit → process exit), and every
// step needs the child to be scheduled — so this must be as generous as
// RESPONSE_TIMEOUT_SECS, not a tight 5s.
const EXIT_TIMEOUT_SECS: u64 = 15;

// How long to wait to confirm that a notification produces NO response.
const NO_RESPONSE_WAIT_MS: u64 = 1000;
const INIT_RETRY_INTERVAL_MS: u64 = 200;
const POLL_SLEEP_MS: u64 = 100;
// How often the disconnect test resends a request while waiting for the
// server to exit (see the test for why a single request is not enough).
const DISCONNECT_RETRY_INTERVAL_MS: u64 = 500;
const READ_BUF_SIZE: usize = 4096;

// ============================================================================
// Line reader over the child's stdout pipe, with deadline support
// ============================================================================

#[cfg(unix)]
fn poll_readable(fd: i32, timeout_ms: i32) -> bool {
    let mut pollfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // poll returns >0 if readable, 0 on timeout, -1 on error
    unsafe { libc::poll(&mut pollfd, 1, timeout_ms) > 0 }
}

/// Line reader over a child process's stdout pipe.
///
/// Keeps partial data in `pending` across calls: a single `read(2)` may
/// deliver several lines at once, and `poll(2)` on the raw fd cannot see
/// bytes that have already been pulled out of the pipe. Polling without
/// this buffer would time out on the next call even though a complete
/// line is already in hand.
struct PipeLineReader {
    stdout: Option<std::process::ChildStdout>,
    pending: Vec<u8>,
}

impl PipeLineReader {
    fn new(stdout: std::process::ChildStdout) -> Self {
        Self {
            stdout: Some(stdout),
            pending: Vec::new(),
        }
    }

    /// Take (and thus close) the stdout pipe, simulating a client disconnect.
    fn take_stdout(&mut self) -> Option<std::process::ChildStdout> {
        self.stdout.take()
    }

    /// Read one complete line before `deadline`.
    /// Returns None on timeout or EOF (a trailing partial line without a
    /// newline is returned as-is). Empty lines are skipped.
    fn read_line_with_deadline(&mut self, deadline: std::time::Instant) -> Option<String> {
        loop {
            if let Some(pos) = self.pending.iter().position(|&b| b == b'\n') {
                let mut line: Vec<u8> = self.pending.drain(..=pos).collect();
                line.pop(); // drop the newline
                let trimmed = String::from_utf8_lossy(&line).trim().to_string();
                if !trimmed.is_empty() {
                    return Some(trimmed);
                }
                continue;
            }

            if std::time::Instant::now() >= deadline {
                return None;
            }

            // Pipe already closed by the test: return any leftover data.
            let Some(stdout) = self.stdout.as_ref() else {
                if self.pending.is_empty() {
                    return None;
                }
                let rest = std::mem::take(&mut self.pending);
                let trimmed = String::from_utf8_lossy(&rest).trim().to_string();
                return if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed)
                };
            };

            // Check if data is available before blocking in read(2).
            let fd = stdout.as_raw_fd();
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            let timeout_ms = remaining.as_millis().min(i32::MAX as u128) as i32;
            if !poll_readable(fd, timeout_ms.max(1)) {
                return None; // timeout: no data available
            }

            let mut tmp = [0u8; READ_BUF_SIZE];
            let n = unsafe { libc::read(fd, tmp.as_mut_ptr().cast(), tmp.len()) };
            if n <= 0 {
                // EOF (or read error): return any leftover data, if present.
                if self.pending.is_empty() {
                    return None;
                }
                let rest = std::mem::take(&mut self.pending);
                let trimmed = String::from_utf8_lossy(&rest).trim().to_string();
                return if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed)
                };
            }
            self.pending.extend_from_slice(&tmp[..n as usize]);
        }
    }
}

// ============================================================================
// Test infrastructure
// ============================================================================

fn make_temp_dir(name: &str) -> std::path::PathBuf {
    file_indexr::testutil::unique_temp_dir(&format!("stdio_test_{name}"))
}

/// Handles communication with a running `file_indexr --stdio` instance.
struct StdioSession {
    child: Child,
    stdin: Option<std::process::ChildStdin>,
    stdout: RefCell<PipeLineReader>,
    /// Where the child's stderr (tracing logs) is written; tests may assert on it.
    stderr_log: std::path::PathBuf,
}

impl StdioSession {
    /// Spawn a new `file_indexr --stdio` process. Does NOT wait for ready state.
    fn spawn(watch_dir: std::path::PathBuf) -> Self {
        let index_dir = make_temp_dir("index");

        // The child's stderr goes to a file, not an unread pipe (a full pipe
        // buffer would block the child), and tests can assert on log content.
        let stderr_dir = make_temp_dir("stderr");
        let stderr_log = stderr_dir.join("child.log");
        let stderr_file = std::fs::File::create(&stderr_log).unwrap();

        // Pin the child's log level so log-content assertions don't depend
        // on a RUST_LOG inherited from the test environment.
        let mut child = Command::new(env!("CARGO_BIN_EXE_file_indexr"))
            .env("RUST_LOG", "info")
            .arg("--stdio")
            .arg("-d")
            .arg(watch_dir.as_os_str())
            .arg("-i")
            .arg(index_dir.as_os_str())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(stderr_file))
            .spawn()
            .expect("failed to spawn file_indexr --stdio");

        let stdin = child.stdin.take().expect("failed to take stdin");
        let stdout = child.stdout.take().expect("failed to take stdout");

        Self {
            child,
            stdin: Some(stdin),
            stdout: RefCell::new(PipeLineReader::new(stdout)),
            stderr_log,
        }
    }

    /// Write a raw line into stdin and flush immediately.
    fn write_line(&mut self, text: &str) {
        use std::io::Write;
        let stdin = self.stdin.as_mut().expect("stdin already consumed");
        stdin.write_all(text.as_bytes()).unwrap();
        stdin.write_all(b"\n").unwrap();
        stdin.flush().unwrap();
    }

    /// Close the child's stdout pipe to simulate a client disconnect
    /// (the server's next response write hits EPIPE).
    fn close_stdout(&mut self) {
        let mut reader = self.stdout.borrow_mut();
        drop(reader.take_stdout());
    }

    /// Send a raw JSON-RPC message and wait for the corresponding response.
    fn send(&mut self, msg: &Value) -> Option<Value> {
        self.write_line(&msg.to_string());
        self.read_response()
    }

    /// Build a JSON-RPC 2.0 request with an auto-generated incremental id.
    fn build_req(method: &str, params: Value) -> Value {
        let id = format!("id_{}", unique_id());
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        })
    }

    /// Send a request with auto-generated incremental id.
    fn send_req(&mut self, method: &str, params: Value) -> Option<Value> {
        self.send(&Self::build_req(method, params))
    }

    /// Send a request without waiting for a response (used once the stdout
    /// pipe is closed and there is nothing left to read).
    ///
    /// Deliberately non-fatal: the child may have exited since our last
    /// liveness check, and a failed write simply means the disconnect
    /// already happened.
    fn write_req(&mut self, method: &str, params: Value) {
        use std::io::Write;
        let Some(stdin) = self.stdin.as_mut() else {
            return;
        };
        let msg = Self::build_req(method, params);
        let text = msg.to_string();
        let _ = stdin.write_all(text.as_bytes());
        let _ = stdin.write_all(b"\n");
        let _ = stdin.flush();
    }

    /// Read one line from stdout and parse as JSON. Returns None on timeout/error.
    fn read_response(&mut self) -> Option<Value> {
        self.read_response_within(Duration::from_secs(RESPONSE_TIMEOUT_SECS))
    }

    /// Read one line with a custom deadline and parse as JSON.
    fn read_response_within(&mut self, timeout: Duration) -> Option<Value> {
        let mut reader = self.stdout.borrow_mut();
        let deadline = std::time::Instant::now() + timeout;
        reader
            .read_line_with_deadline(deadline)
            .and_then(|line| serde_json::from_str(&line).ok())
    }

    /// Wait for the process to be ready by sending `initialize` requests with retries.
    /// A late response from a previous attempt is kept in the reader's pending
    /// buffer and satisfies a later read, so no explicit draining is needed.
    fn ensure_ready(&mut self) {
        let overall_deadline = std::time::Instant::now() + Duration::from_secs(READY_TIMEOUT_SECS);
        loop {
            let resp = self.send_req("initialize", serde_json::json!({}));
            if resp.is_some() {
                return;
            }
            if std::time::Instant::now() >= overall_deadline {
                panic!(
                    "STDIO server did not become ready within {}s",
                    READY_TIMEOUT_SECS
                );
            }
            std::thread::sleep(Duration::from_millis(INIT_RETRY_INTERVAL_MS));
        }
    }
}

impl Drop for StdioSession {
    fn drop(&mut self) {
        // Kill the process to avoid hanging tests.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ============================================================================
// Helpers
// ============================================================================

/// Best-effort read of the child's stderr log for failure diagnostics.
fn read_child_log(session: &StdioSession) -> String {
    std::fs::read_to_string(&session.stderr_log)
        .unwrap_or_else(|e| format!("(failed to read child log: {e})"))
}

fn send_noise(session: &mut StdioSession) {
    session.write_line(""); // empty line
    session.write_line("not json"); // invalid JSON
    session.write_line("{{bad json}}"); // malformed JSON
}

// ============================================================================
// Tests: STDIO transport behavior
// ============================================================================

#[test]
fn test_stdio_graceful_shutdown_on_kill() {
    let watch_dir = make_temp_dir("files");
    prepare_mcp_files(&watch_dir);
    let mut session = StdioSession::spawn(watch_dir);
    session.ensure_ready();

    // Kill the process — it should exit cleanly.
    let _ = session.child.kill();

    // Wait for termination (max 5s).
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        match session.child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    panic!("STDIO server did not shutdown on kill within 5s");
                }
                std::thread::sleep(Duration::from_millis(POLL_SLEEP_MS));
            }
            Err(_) => break, // Process already reaped
        }
    }
}

#[test]
fn test_stdio_client_disconnect_exits_cleanly() {
    let watch_dir = make_temp_dir("files");
    prepare_mcp_files(&watch_dir);
    let mut session = StdioSession::spawn(watch_dir);
    session.ensure_ready();

    // Close our end of the stdout pipe: the server's next response write
    // hits EPIPE and must be treated as a normal client disconnect, not
    // as an error (previously it was logged as "STDIO loop error").
    session.close_stdout();
    session.write_req("tools/list", serde_json::json!({}));

    // The server should exit on its own with status 0.
    //
    // A single request does not guarantee the disconnect fires: closing
    // OUR end of the pipe only removes our reader, but any process forked
    // from this test binary (e.g. a parallel test spawning its own server)
    // holds a copy of the read end between fork and exec, and the kernel
    // counts it as a live pipe reader. A response write that lands in that
    // window succeeds instead of failing with EPIPE, and the server keeps
    // waiting for input. Resending a request while we wait gives the server
    // a fresh EPIPE opportunity once the transient reader is gone.
    let deadline = std::time::Instant::now() + Duration::from_secs(EXIT_TIMEOUT_SECS);
    let mut next_retry =
        std::time::Instant::now() + Duration::from_millis(DISCONNECT_RETRY_INTERVAL_MS);
    let status = loop {
        match session.child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let log = read_child_log(&session);
                    panic!(
                        "STDIO server did not exit after client disconnect within {EXIT_TIMEOUT_SECS}s.\nChild log:\n{log}"
                    );
                }
                if std::time::Instant::now() >= next_retry {
                    session.write_req("tools/list", serde_json::json!({}));
                    next_retry = std::time::Instant::now()
                        + Duration::from_millis(DISCONNECT_RETRY_INTERVAL_MS);
                }
                std::thread::sleep(Duration::from_millis(POLL_SLEEP_MS));
            }
            Err(_) => panic!("failed to poll child status"),
        }
    };
    assert!(
        status.code() == Some(0),
        "server should exit cleanly (code 0) on client disconnect, got {status:?}"
    );

    // The disconnect must be logged as a clean shutdown, not as an error.
    let log = std::fs::read_to_string(&session.stderr_log).unwrap();
    assert!(
        log.contains("Client disconnected (stdout closed)"),
        "expected a clean-disconnect log line, got:\n{log}"
    );
}

#[test]
fn test_stdio_invalid_json_returns_parse_error() {
    let watch_dir = make_temp_dir("files");
    prepare_mcp_files(&watch_dir);
    let mut session = StdioSession::spawn(watch_dir);
    session.ensure_ready();

    // Send garbage. Empty lines are ignored; each invalid line gets a
    // JSON-RPC parse-error response with id: null.
    send_noise(&mut session);

    // Drain the two parse-error responses (one per invalid line) and verify them.
    for _ in 0..2 {
        let resp = session.read_response();
        assert!(
            resp.is_some(),
            "expected parse-error response for invalid JSON"
        );
        let resp = resp.unwrap();
        assert!(
            resp["id"].is_null(),
            "parse-error response must carry id: null"
        );
        assert_eq!(
            resp["error"]["code"], PARSE_ERROR,
            "expected PARSE_ERROR code"
        );
    }

    // Server still works after the errors.
    let resp = session.send_req("tools/list", serde_json::json!({}));
    assert!(resp.is_some(), "expected tools/list response after noise");
    assert!(resp.as_ref().unwrap()["error"].is_null());
}

#[test]
fn test_stdio_non_utf8_line_is_skipped() {
    let watch_dir = make_temp_dir("files");
    prepare_mcp_files(&watch_dir);
    let mut session = StdioSession::spawn(watch_dir);
    session.ensure_ready();

    // Raw non-UTF-8 bytes: the server must skip the line, not treat the
    // read error as EOF (previously it exited the loop silently with code 0).
    use std::io::Write;
    let stdin = session.stdin.as_mut().expect("stdin already consumed");
    stdin.write_all(b"\xff\xfe\n").unwrap();
    stdin.flush().unwrap();

    // The server must still respond to subsequent valid requests.
    let resp = session.send_req("tools/list", serde_json::json!({}));
    assert!(
        resp.is_some(),
        "expected tools/list response after non-UTF-8 line"
    );
    assert!(resp.as_ref().unwrap()["error"].is_null());
}

#[test]
fn test_stdio_notification_produces_no_response() {
    let watch_dir = make_temp_dir("files");
    prepare_mcp_files(&watch_dir);
    let mut session = StdioSession::spawn(watch_dir);
    session.ensure_ready();

    // Notification (no "id" field) — no response expected.
    session.write_line(
        &serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"}).to_string(),
    );

    // Should get nothing for the notification.
    std::thread::sleep(Duration::from_millis(POLL_SLEEP_MS));
    assert!(
        session
            .read_response_within(Duration::from_millis(NO_RESPONSE_WAIT_MS))
            .is_none(),
        "notification should not produce a response"
    );

    // Subsequent request should still work.
    let resp = session.send_req("tools/list", serde_json::json!({}));
    assert!(resp.is_some(), "tools/list should work after notification");
}

#[test]
fn test_stdio_preserves_request_id() {
    let watch_dir = make_temp_dir("files");
    prepare_mcp_files(&watch_dir);
    let mut session = StdioSession::spawn(watch_dir);
    session.ensure_ready();

    let msg = serde_json::json!({
        "jsonrpc": "2.0",
        "id": "custom_string_id_42",
        "method": "initialize",
        "params": {}
    });
    session.write_line(&msg.to_string());

    let resp = session.read_response().expect("expected response");
    assert_eq!(resp["id"], "custom_string_id_42");
}

#[test]
fn test_stdio_unknown_method_returns_error() {
    let watch_dir = make_temp_dir("files");
    prepare_mcp_files(&watch_dir);
    let mut session = StdioSession::spawn(watch_dir);
    session.ensure_ready();

    let resp = session.send_req("nonexistent/method", serde_json::json!({}));
    assert!(resp.is_some());
    assert!(resp.as_ref().unwrap()["error"].is_object());
    assert_eq!(resp.as_ref().unwrap()["error"]["code"], METHOD_NOT_FOUND);
}

#[test]
fn test_stdio_initialize_response() {
    let watch_dir = make_temp_dir("files");
    prepare_mcp_files(&watch_dir);
    let mut session = StdioSession::spawn(watch_dir);
    session.ensure_ready();

    let resp = session.send_req("initialize", serde_json::json!({}));
    assert!(resp.is_some());
    let resp = resp.unwrap();
    assert_eq!(resp["result"]["serverInfo"]["name"], "file_indexr");
    assert!(resp["result"]["protocolVersion"].as_str().is_some());
}

// ============================================================================
// Tests: end-to-end tool calls via STDIO
// ============================================================================

#[test]
fn test_stdio_tools_list_has_three_tools() {
    let watch_dir = make_temp_dir("files");
    prepare_mcp_files(&watch_dir);
    let mut session = StdioSession::spawn(watch_dir);
    session.ensure_ready();

    let resp = session.send_req("tools/list", serde_json::json!({}));
    assert!(resp.is_some());
    let tools = &resp.unwrap()["result"]["tools"];
    assert!(tools.is_array());

    let names: Vec<String> = tools
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["name"].as_str().map(String::from))
        .collect();
    assert!(names.contains(&"docs_search".into()));
    assert!(names.contains(&"docs_headings".into()));
    assert!(names.contains(&"docs_get".into()));
}

#[test]
fn test_stdio_tools_call_docs_search() {
    let watch_dir = make_temp_dir("files");
    prepare_mcp_files(&watch_dir);
    let mut session = StdioSession::spawn(watch_dir);
    session.ensure_ready();

    let resp = session.send_req(
        "tools/call",
        serde_json::json!({"name": "docs_search", "arguments": {"query": "Hello"}}),
    );
    assert!(resp.is_some());

    let content = &resp.unwrap()["result"]["content"][0];
    assert_eq!(content["type"], "text");
    let text = content["text"].as_str().unwrap();
    assert!(text.contains("hello.txt"));
}

#[test]
fn test_stdio_tools_call_docs_search_empty_query() {
    let watch_dir = make_temp_dir("files");
    prepare_mcp_files(&watch_dir);
    let mut session = StdioSession::spawn(watch_dir);
    session.ensure_ready();

    let resp = session.send_req(
        "tools/call",
        serde_json::json!({"name": "docs_search", "arguments": {"query": ""}}),
    );
    assert!(resp.is_some());
    let text = &resp.unwrap()["result"]["content"][0]["text"];
    assert!(text.as_str().unwrap().to_lowercase().contains("empty"));
}

#[test]
fn test_stdio_tools_call_docs_get() {
    let watch_dir = make_temp_dir("files");
    prepare_mcp_files(&watch_dir);
    let mut session = StdioSession::spawn(watch_dir);
    session.ensure_ready();

    let resp = session.send_req(
        "tools/call",
        serde_json::json!({"name": "docs_get", "arguments": {"path": "hello.txt"}}),
    );
    assert!(resp.is_some());

    let text = &resp.unwrap()["result"]["content"][0];
    let body = text["text"].as_str().unwrap();
    assert!(body.contains("Hello"));
}

#[test]
fn test_stdio_tools_call_docs_headings() {
    let watch_dir = make_temp_dir("files");
    prepare_mcp_files(&watch_dir);
    let mut session = StdioSession::spawn(watch_dir);
    session.ensure_ready();

    let resp = session.send_req(
        "tools/call",
        serde_json::json!({"name": "docs_headings", "arguments": {"path": "README.md"}}),
    );
    assert!(resp.is_some());

    let text = &resp.unwrap()["result"]["content"][0];
    let body = text["text"].as_str().unwrap();
    assert!(body.contains("FileIndexr"));
    assert!(body.contains("Features"));
}

#[test]
fn test_stdio_tools_call_docs_get_file_not_found() {
    let watch_dir = make_temp_dir("files");
    prepare_mcp_files(&watch_dir);
    let mut session = StdioSession::spawn(watch_dir);
    session.ensure_ready();

    let resp = session.send_req(
        "tools/call",
        serde_json::json!({"name": "docs_get", "arguments": {"path": "nonexistent.txt"}}),
    );
    assert!(resp.is_some());
    assert!(
        resp.as_ref().unwrap()["error"].is_object(),
        "should return error for missing file"
    );
}

#[test]
fn test_stdio_tools_call_unknown_tool() {
    let watch_dir = make_temp_dir("files");
    prepare_mcp_files(&watch_dir);
    let mut session = StdioSession::spawn(watch_dir);
    session.ensure_ready();

    let resp = session.send_req(
        "tools/call",
        serde_json::json!({"name": "unknown_tool", "arguments": {}}),
    );
    assert!(resp.is_some());
    assert!(resp.as_ref().unwrap()["error"].is_object());
}

#[test]
fn test_stdio_multiple_sequential_requests() {
    let watch_dir = make_temp_dir("files");
    prepare_mcp_files(&watch_dir);
    let mut session = StdioSession::spawn(watch_dir);
    session.ensure_ready();

    // Chain several requests.
    for method in ["initialize", "tools/list"] {
        let resp = session.send_req(method, serde_json::json!({}));
        assert!(resp.is_some(), "{} request failed", method);
    }

    // Tool call in the middle.
    let resp = session.send_req(
        "tools/call",
        serde_json::json!({"name": "docs_search", "arguments": {"query": "search"}}),
    );
    assert!(resp.is_some());

    // Back to regular methods.
    for method in ["initialize", "tools/list"] {
        let resp = session.send_req(method, serde_json::json!({}));
        assert!(resp.is_some(), "{} after tool call failed", method);
    }
}

#[test]
fn test_stdio_mixed_valid_invalid_requests() {
    let watch_dir = make_temp_dir("files");
    prepare_mcp_files(&watch_dir);
    let mut session = StdioSession::spawn(watch_dir);
    session.ensure_ready();

    // Invalid JSON → parse-error responses (id: null).
    send_noise(&mut session);
    for _ in 0..2 {
        let resp = session.read_response();
        assert!(
            resp.is_some(),
            "expected parse-error response for invalid JSON"
        );
        assert!(resp.as_ref().unwrap()["id"].is_null());
    }

    // Valid request — should work (initialize response, no error).
    let resp = session.send_req("initialize", serde_json::json!({}));
    assert!(resp.is_some(), "valid request should work after noise");
    assert!(
        resp.as_ref().unwrap()["error"].is_null(),
        "initialize should not error"
    );

    // Notification → no response.
    session.write_line(
        &serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized"}).to_string(),
    );

    // Yet another valid — still works.
    let resp2 = session.send_req("initialize", serde_json::json!({}));
    assert!(resp2.is_some());
    assert!(
        resp2.as_ref().unwrap()["error"].is_null(),
        "second initialize should not error"
    );
}
