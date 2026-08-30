//! Shared test infrastructure for file_indexr integration tests.

use std::path::Path;

/// Prepare watch directory with standard test files for MCP testing.
pub fn prepare_mcp_files(watch_dir: &Path) {
    std::fs::create_dir_all(watch_dir).unwrap();
    std::fs::write(
        watch_dir.join("hello.txt"),
        "Hello, world!\nWelcome to FileIndexr.",
    )
    .unwrap();
    std::fs::write(
        watch_dir.join("README.md"),
        "# FileIndexr\n\nA local search engine.\n\n## Features\n\n- Full-text search\n- REST API\n",
    )
    .unwrap();
}
