# FileIndexr

A local file search server that makes your codebase instantly searchable. Built for AI agents that need to find and read files without access to the filesystem.

> **Note:** This project is designed for a single user or a small team. It does not support multi-user authentication, role-based access control, or shared indexes across users.

## What problem does it solve

An AI agent in a browser or sandbox can call `fetch()`, but cannot walk your filesystem. FileIndexr bridges that gap: it indexes all files in a directory and exposes an MCP interface so any MCP client can search by filename, path, or file content.

## How it works

- **Indexes on startup**: Scans the target directory and builds a full-text search index using [Tantivy](https://github.com/quickwit-oss/tantivy) (Rust equivalent of Lucene).
- **Stays in sync**: Watches the directory for changes — new files, modifications, deletions — and updates the index in real time. If the kernel drops file events (inotify queue overflow during bulk operations), the gap is detected and an automatic rescan recovers the missed changes.
- **Resumes intelligently**: On restart, only re-indexes files that changed since the last run (mtime-based; checkpoint persisted on disk).
- **Answers queries via MCP**: Model Context Protocol tools for searching, reading files, and browsing structure.

## Quick start

### Install

```bash
cargo build --release
```

The binary ends up in `target/release/file_indexr`.

### Run

```bash
# Minimal — index a directory, listen on HTTP port 8080
./target/release/file_indexr -d /path/to/your/project

# With custom settings
./target/release/file_indexr \
  -d /home/user/projects \
  -i /home/user/.local/share/file_indexr/index \
  -p 9090 \
  -b 0.0.0.0

# MCP over STDIO — for integration with Claude Desktop, CLI tools, etc.
./target/release/file_indexr -d /path/to/your/project --stdio
```

The first run indexes everything. Subsequent runs only process changes.

### Config file (optional)

Create a config file anywhere and pass its path with `--config`:

```toml
directory = "/home/user/projects"
port = 8080
bind = "127.0.0.1"

# Only index content for files up to 2 MB
max_file_size_mb = 2

# Only index these extensions (empty = all files)
# Case-insensitive; leading dots and surrounding whitespace are tolerated
allowed_extensions = []

# Batch settings for the writer
batch_size = 500
batch_timeout_ms = 1000
```

Command-line arguments override config file values.

## Usage

### MCP Tools

**HTTP mode (default):** connect any MCP client to `http://127.0.0.1:8080/mcp`.

**STDIO mode:** run with `--stdio` flag to serve MCP over stdin/stdout — suitable for Claude Desktop, CLI tools, or other stdio-based MCP clients.

**`docs_search`** — Search the documentation index by keywords.

| Parameter     | Type    | Required | Description                          |
| ------------- | ------- | -------- | ------------------------------------ |
| `query`       | string  | Yes      | Search query text.                   |
| `max_results` | integer | No       | Max results. Default: `10`, max: `100`. |

**`docs_headings`** — Get the table of contents (headings with line ranges) for a file.

| Parameter | Type   | Required | Description                    |
| --------- | ------ | -------- | ------------------------------ |
| `path`    | string | Yes      | Relative path from search results. |

**`docs_get`** — Read file content. Supports line-range reads and auto-truncation at 100 lines.

| Parameter    | Type    | Required | Description                                     |
| ------------ | ------- | -------- | ----------------------------------------------- |
| `path`       | string  | Yes      | Relative path to the file.                      |
| `start_line` | integer | No       | 1-based starting line number.                   |
| `end_line`   | integer | No       | 1-based ending line number (inclusive).         |

Supported formats for `docs_headings` and `docs_get`: `txt`, `md`, `rs`, `py`, `htm`, `html`, `pdf`, `epub`.

### Web UI

Open `http://127.0.0.1:8080/` in a browser for a visual search interface.

### HTTP Endpoints

| Method | Path     | Description                    |
| ------ | -------- | ------------------------------ |
| GET    | `/`      | Web UI                         |
| GET    | `/search`| Full-text search (JSON)        |
| GET    | `/file`  | File content (HTML/PDF/EPUB → Markdown) |
| GET    | `/structure` | Document structure (JSON)    |
| GET    | `/stats` | Index statistics (JSON)        |
| POST   | `/mcp`   | MCP JSON-RPC requests          |
| GET    | `/mcp`   | MCP SSE stream (empty stub)    |

### For AI agents

A typical workflow looks like this:

1. **Search**: `docs_search(query="my_function", max_results=5)`
2. **Read**: `docs_get(path="src/utils/helpers.ts")` for the most relevant result
3. **Browse**: `docs_headings(path="src/utils/helpers.ts")` to get file structure

## CLI reference

```
Usage: file_indexr [OPTIONS]

Options:
  -d, --directory <PATH>      Directory to index (required)
  -i, --index-path <PATH>    Where to store the index [default: <directory>/.file_indexr/index]
  -p, --port <PORT>          HTTP port to listen on (ignored with --stdio) [default: 8080]
  -b, --bind <ADDRESS>       Bind address (ignored with --stdio) [default: 127.0.0.1]
      --max-file-size-mb <MB> Max file size to index content (MB) [default: 20]
      --batch-size <N>       Changes per batch commit [default: 500]
      --batch-timeout-ms <MS> Max wait before committing batch (ms) [default: 1000]
      --allowed-extensions   Comma-separated list of allowed extensions to index content (e.g., "md,txt,html")
      --config <PATH>        Path to config.toml
  -v, --verbose              Enable debug logging
      --stdio                Run MCP over STDIO instead of starting HTTP server
```

## Notes

- The index lives on disk by default inside the watched directory (`.file_indexr/index/`). You can move it elsewhere with `-i` to keep your project clean or put it on a faster drive.
- All files are indexed, but content is read from disk on demand (never stored in the index) for files with a recognized format (`txt`, `md`, `rs`, `py`, `htm`, `html`, `pdf`, `epub`) and within the size limit. Other files appear in results with `has_content=false`.
- Allowed extensions (via `--allowed-extensions` or `allowed_extensions`) are normalized before use: trimmed, leading dots removed, lowercased, and de-duplicated. So `"md, TXT"`, `".md"`, and `"md"` all match the same files.
- Large catalogs are supported: startup scan streams events through a bounded channel, deletion detection uses the Tantivy FST term dictionary, and indexing happens in configurable batches.
- The server listens on `127.0.0.1` by default. Use `-b 0.0.0.0` to expose it on the network.
- HTML files (`.html`, `.htm`), PDF files (`.pdf`), and EPUB books (`.epub`) are automatically converted to Markdown when served via MCP tools or the web `/file` endpoint.
- Directories starting with `.` are excluded from indexing (dot files at top level are still indexed).
- The watcher task auto-restarts on failure (up to 7 attempts with exponential backoff). If the watcher exits, the entire server shuts down gracefully.
