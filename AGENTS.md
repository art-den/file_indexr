# FileIndexr

Local search engine for files: full-text search (Tantivy), web UI, MCP server.
Single-user — no auth, RBAC, or shared indexes.

## Critical rules

- **Never write debug output to `stdout`** — the STDIO MCP transport writes JSON-RPC responses to stdout, so any `println!`/`dbg!` corrupts the protocol. Log via `tracing` (stderr) only.
- File content is indexed for search but **NOT stored** in Tantivy documents — it is read from disk at search time via `src/formats/`. Don't add `STORED` to the content field.
- Transient filesystem errors must never wipe the index: deletion detection counts only `ErrorKind::NotFound`, and a failed scan/commit simply leaves stale stored mtimes in the index, so the next start re-applies the missed changes (per-file mtime comparison against the indexed value).
- Tantivy: `IndexWriter` holds an exclusive lock — drop it before creating a new one on the same path; explicit `.commit()` is required for search visibility.
- Search snippets use `StreamExt::buffered`, which preserves result order — do not replace with `buffer_unordered`.
- Virtual EPUB image paths in `docs_get` (`<book.epub>/<inner>`): only the outer EPUB file goes through `validate_path`. The inner part is an opaque zip-entry lookup (literal name, else unique suffix match) that cannot escape the archive — do not run it through filesystem operations.

## Testing

- Unit-tests are placed into `tests/tests_*.rs` files to avoid cluttering the code of the main modules.
- Temp dirs come from `file_indexr::testutil` (pid + atomic counter) — unique per test, run in parallel, do NOT clean up. `unique_temp_dir` enforces freshness with `create_dir` + retry on `AlreadyExists`: the OS recycles pids, so `create_dir_all` could silently reuse a dir left over from a previous run. Never rely on `create_dir_all` for unique test paths.
- Always commit before searching in tests (`writer.commit().await` / `commit().unwrap()`).
- HTTP endpoint tests use `tower::Service` — no real server needed.
- STDIO transport tests spawn the compiled binary over stdin/stdout pipes (`tests/mcp_stdio_integration.rs`, `libc` in `[dev-dependencies]`).
