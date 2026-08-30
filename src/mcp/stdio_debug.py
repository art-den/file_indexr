
"""
MCP STDIO client for testing file_indexr's STDIO transport.

Usage:
    # Basic smoke test (needs an existing directory to index)
    python3 src/mcp/stdio_debug.py --smoke --directory /tmp/test_index

    # Send a custom raw message via stdin
    echo '{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}' | \
        python3 src/mcp/stdio_debug.py --exec --directory /tmp/test_index

    # Interactive REPL mode
    python3 src/mcp/stdio_debug.py --repl --directory /tmp/test_index
"""

# The script deals with untyped JSON from the wire (json.loads -> Any by design),
# so disable Any-related and unused-call-result rules for this file only.
# pyright: reportAny=none, reportExplicitAny=none, reportUnusedCallResult=none

from __future__ import annotations

import argparse
import asyncio
import json
import os
import subprocess
import sys
import threading
import time
from collections import deque
from pathlib import Path
from typing import IO, Any, final


@final
class McpStdioClient:
    """Simple MCP STDIO client that communicates via process stdin/stdout."""

    def __init__(self, cmd: list[str], env: dict[str, str] | None = None):
        self.cmd = cmd
        self.env = env or {}
        self._proc: subprocess.Popen[bytes] | None = None
        self._writer: IO[bytes] | None = None
        # Thread-safe queue for incoming responses
        self._queue: deque[tuple[int | str | None, dict[str, Any]]] = deque()
        self._lock = threading.Lock()

    def start(self) -> None:
        """Launch the MCP server process and start the reader thread."""
        self._proc = subprocess.Popen(
            self.cmd,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=sys.stderr,
        )
        self._writer = self._proc.stdin

        # Start a background thread that reads responses from stdout
        reader_thread = threading.Thread(target=self._read_loop, daemon=True)
        reader_thread.start()

        print(f"✓ Process started (PID {self._proc.pid})")

    def _read_loop(self) -> None:
        """Continuously read lines from stdout and enqueue them."""
        proc = self._proc
        if proc is None or proc.stdout is None:
            return
        try:
            for raw_line in proc.stdout:
                stripped = raw_line.decode("utf-8").strip()
                if not stripped:
                    continue
                data: dict[str, Any] = json.loads(stripped)
                msg_id: int | str | None = data.get("id")
                with self._lock:
                    self._queue.append((msg_id, data))
        except (json.JSONDecodeError, UnicodeDecodeError, OSError) as e:
            # Parse errors or broken pipe
            print(f"[reader thread error] {type(e).__name__}: {e}")
        finally:
            with self._lock:
                self._queue.clear()

    def stop(self) -> None:
        """Terminate the MCP server process."""
        if self._proc and self._proc.poll() is None:
            self._proc.terminate()
            try:
                self._proc.wait(timeout=5.0)
            except subprocess.TimeoutExpired:
                self._proc.kill()
                self._proc.wait()
            print("✓ Process terminated")

    def send_request(
        self,
        method: str,
        params: dict[str, Any] | None = None,
        msg_id: int | str | None = 1,
        timeout: float = 30.0,
    ) -> dict[str, Any]:
        """Send a JSON-RPC request and return the matching response."""
        assert self._writer is not None

        if msg_id is None:
            body = json.dumps({
                "jsonrpc": "2.0",
                "method": method,
                "params": params or {},
            }) + "\n"
        else:
            body = json.dumps({
                "jsonrpc": "2.0",
                "id": msg_id,
                "method": method,
                "params": params or {},
            }) + "\n"

        self._writer.write(body.encode("utf-8"))
        self._writer.flush()

        if msg_id is None:
            # Notifications — consume any stray responses
            with self._lock:
                while self._queue:
                    old_id = self._queue.popleft()[0]
                    print(f"  [consumed stray response id={old_id}]")
            return {}

        # Wait for the matching response
        deadline = time.monotonic() + timeout
        while True:
            with self._lock:
                for i, (qid, qdata) in enumerate(self._queue):
                    # Match by ID regardless of whether it's a success or error response
                    if qid == msg_id:
                        del self._queue[i]
                        if "error" in qdata:
                            err = qdata["error"]
                            raise RuntimeError(
                                f"MCP error [{err['code']}]: {err['message']}"
                            )
                        return qdata

            if self._proc and self._proc.poll() is not None:
                raise RuntimeError("Server closed connection unexpectedly")

            if time.monotonic() > deadline:
                raise RuntimeError(f"Timed out waiting for response id={msg_id}")

            time.sleep(0.05)

    def initialize(self) -> dict[str, Any]:
        """Run the standard initialization handshake."""
        resp: dict[str, Any] = self.send_request("server/discover", msg_id="init-1")
        info: dict[str, Any] = resp.get("result", {})
        ver: Any = info.get("protocolVersion", "?")
        srv: dict[str, Any] = info.get("serverInfo", {})
        print(f"✓ Server discover: version={ver}, {srv.get('name','?')} v{srv.get('version','?')}")

        self.send_request("notifications/initialized", params={}, msg_id=None)
        print("✓ Initialized notification sent")

        return resp

    def write_raw(self, data: bytes) -> None:
        """Write raw bytes to the server stdin (test hook)."""
        assert self._writer is not None
        self._writer.write(data)
        self._writer.flush()


def _resolve_binary(directory: str) -> list[str]:
    """Build the command to launch file_indexr in STDIO mode."""
    script_dir = Path(__file__).resolve()
    project_root = script_dir.parent.parent.parent
    binary_path = project_root / "target" / "release" / "file_indexr"
    if binary_path.exists():
        return [str(binary_path), "--stdio", "--directory", directory]
    debug_path = project_root / "target" / "debug" / "file_indexr"
    if debug_path.exists():
        return [str(debug_path), "--stdio", "--directory", directory]
    print("⚠ Binary not found — falling back to 'cargo run'")
    return ["cargo", "run", "--quiet", "--", "--stdio", "--directory", directory]


def run_smoke_test(directory: str) -> bool:
    """Run a full smoke test synchronously."""
    print("=" * 60)
    print("MCP STDIO Smoke Test")
    print("=" * 60)

    cmd = _resolve_binary(directory)
    client = McpStdioClient(cmd=cmd, env={"RUST_BACKTRACE": "1"})
    client.start()

    try:
        # Test 1: server/discover
        print("\n── Test 1: server/discover ──")
        resp = client.initialize()
        assert "result" in resp, "Expected result field"
        keys = list(resp["result"].keys())
        print(f"  Result keys: {keys}")

        # Test 2: tools/list
        print("\n── Test 2: tools/list ──")
        resp = client.send_request("tools/list", msg_id=2)
        tools = resp["result"]["tools"]
        names = [t["name"] for t in tools]
        print(f"  Tools found: {names}")
        assert len(names) >= 1, f"Expected at least 1 tool, got {len(names)}"

        # Test 3: tools/call — docs_search
        print("\n── Test 3: tools/call — docs_search ──")
        resp = client.send_request(
            "tools/call",
            params={
                "name": "docs_search",
                "arguments": {"query": "test", "max_results": 3},
            },
            msg_id=3,
        )
        content = resp["result"]["content"]
        text = content[0]["text"] if content else ""
        lines = text.strip().split("\n")
        print(f"  First line: {lines[0] if lines else '(empty)'}")

        # Test 4: tools/call — docs_headings (bad path → error response)
        print("\n── Test 4: tools/call — docs_headings (bad path) ──")
        try:
            client.send_request(
                "tools/call",
                params={
                    "name": "docs_headings",
                    "arguments": {"path": "/nonexistent.md"},
                },
                msg_id=4,
            )
            print("  ✗ Should have returned an error!")
        except RuntimeError as e:
            err_msg = str(e)
            print(f"  ✓ Got expected error: {err_msg.split(': ')[-1]}")

        # Test 5: legacy initialize
        print("\n── Test 5: legacy initialize ──")
        resp = client.send_request(
            "initialize",
            params={
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "test-client", "version": "1.0"},
            },
            msg_id=5,
        )
        srv = resp["result"]["serverInfo"]
        print(f"  Legacy: {srv['name']} v{srv['version']}")

        # Test 6: unknown method → error
        print("\n── Test 6: unknown method ──")
        try:
            client.send_request("tools/unknown_action", msg_id=6)
            print("  ✗ Should have returned an error!")
            return False
        except RuntimeError as e:
            print(f"  ✓ Got expected error: {str(e).split(': ')[0]}")

        # Test 7: invalid JSON → server skips silently (no ID to respond to)
        print("\n── Test 7: invalid JSON — robustness check ──")
        client.write_raw(b"not valid json {{{\n")
        time.sleep(0.2)  # let the reader thread process it
        # Server correctly discards unparseable input — verify it still works after
        resp = client.send_request("tools/list", msg_id=7)
        print("  ✓ Server survived invalid JSON (still responding)")

        # Test 8: missing name for tools/call → validation error
        print("\n── Test 8: tools/call without name ──")
        try:
            client.send_request("tools/call", params={}, msg_id=8)
            print("  ✗ Should have returned an error!")
            return False
        except RuntimeError as e:
            print(f"  ✓ Got expected error: {str(e).split(': ')[0]}")

        print("\n" + "=" * 60)
        print("All tests passed! ✓")
        print("=" * 60)
        return True

    finally:
        client.stop()


def run_exec_mode(directory: str) -> None:
    """Execute messages piped via stdin."""
    print("=" * 60)
    print("MCP STDIO — Execute mode")
    print("Reads JSON lines from stdin, writes responses to stdout")
    print("=" * 60)

    cmd = _resolve_binary(directory)
    client = McpStdioClient(cmd=cmd, env={"RUST_BACKTRACE": "1"})
    client.start()

    try:
        client.initialize()

        for line in sys.stdin:
            line = line.strip()
            if not line:
                continue
            try:
                msg: dict[str, Any] = json.loads(line)
                resp = client.send_request(
                    msg["method"],
                    params=msg.get("params", {}),
                    msg_id=msg.get("id"),
                )
                result = resp.get("result", {})
                error = resp.get("error")
                output = {"response": result} if result else {"error": error}
                print(json.dumps(output, indent=2, ensure_ascii=False))
            except RuntimeError as e:
                print(f"Error: {e}", file=sys.stderr)

    finally:
        client.stop()


def main() -> None:
    parser = argparse.ArgumentParser(description="MCP STDIO test client for file_indexr")
    group = parser.add_mutually_exclusive_group(required=True)
    group.add_argument("--smoke", action="store_true", help="Run automated smoke tests")
    group.add_argument("--repl", action="store_true", help="Enter interactive REPL mode")
    group.add_argument("--exec", action="store_true", help="Execute mode (read JSON from stdin)")
    parser.add_argument("--directory", required=True, help="Directory to index (must exist)")

    args = parser.parse_args()

    if not os.path.isdir(args.directory):
        print(f"Error: Directory does not exist: {args.directory}", file=sys.stderr)
        sys.exit(1)

    if args.smoke:
        success = run_smoke_test(args.directory)
        sys.exit(0 if success else 1)
    elif args.repl:
        import asyncio
        asyncio.run(run_repl_mode_async(args.directory))
    elif args.exec:
        run_exec_mode(args.directory)


async def run_repl_mode_async(directory: str) -> None:
    """Async wrapper around the sync REPL."""
    # Since we use threading-based client, run sync code in a thread
    await asyncio.to_thread(run_repl_mode_sync, directory)


def run_repl_mode_sync(directory: str) -> None:
    """Sync REPL mode (uses threading-based client internally)."""
    print("=" * 60)
    print("MCP STDIO Interactive REPL")
    print("Type JSON-RPC requests or built-in commands:")
    print("  init          — Run initialization handshake")
    print("  list          — List available tools")
    print("  search <q>    — Search files")
    print("  headings <f>  — Get document structure")
    print("  get <f>       — Get file content")
    print("  quit / exit   — Exit")
    print("=" * 60)

    cmd = _resolve_binary(directory)
    client = McpStdioClient(cmd=cmd, env={"RUST_BACKTRACE": "1"})
    client.start()

    try:
        client.initialize()

        while True:
            try:
                line = input("\n>>> ").strip()
            except EOFError:
                break

            if not line:
                continue

            parts = line.split(maxsplit=1)
            command = parts[0].lower()
            arg = parts[1] if len(parts) > 1 else ""

            match command:
                case "quit" | "exit":
                    break
                case "init":
                    client.initialize()
                case "list":
                    resp = client.send_request("tools/list", msg_id=time.time_ns())
                    tools = resp["result"]["tools"]
                    print(f"\nTools ({len(tools)}):")
                    for t in tools:
                        desc = t.get("description", "")[:80]
                        print(f"  • {t['name']}: {desc}...")
                case "search":
                    query = arg or input("Query: ").strip()
                    print(f"Searching: {query}")
                    resp = client.send_request(
                        "tools/call",
                        params={"name": "docs_search", "arguments": {"query": query}},
                        msg_id=time.time_ns(),
                    )
                    content = resp["result"]["content"]
                    text = content[0]["text"] if content else "(empty)"
                    print(text)
                case "headings":
                    path = arg or input("Path: ").strip()
                    print(f"Headings for: {path}")
                    resp = client.send_request(
                        "tools/call",
                        params={"name": "docs_headings", "arguments": {"path": path}},
                        msg_id=time.time_ns(),
                    )
                    content = resp["result"]["content"]
                    text = content[0]["text"] if content else "(empty)"
                    print(text)
                case "get":
                    path = arg or input("Path: ").strip()
                    print(f"Content for: {path}")
                    resp = client.send_request(
                        "tools/call",
                        params={"name": "docs_get", "arguments": {"path": path}},
                        msg_id=time.time_ns(),
                    )
                    content = resp["result"]["content"]
                    text = content[0]["text"] if content else "(empty)"
                    print(text)
                case _:
                    try:
                        msg: dict[str, Any] = json.loads(line)
                        resp = client.send_request(
                            msg["method"],
                            params=msg.get("params", {}),
                            msg_id=msg.get("id"),
                        )
                        result = resp.get("result", {})
                        error = resp.get("error")
                        output = {"response": result} if result else {"error": error}
                        print(json.dumps(output, indent=2, ensure_ascii=False))
                    except json.JSONDecodeError:
                        print(f"Invalid JSON: {line}")
                    except RuntimeError as e:
                        print(f"Error: {e}")

    finally:
        client.stop()


if __name__ == "__main__":
    main()
