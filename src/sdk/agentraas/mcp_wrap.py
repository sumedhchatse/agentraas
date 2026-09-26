"""
agentraas wrap: exactly-once tool calls and an audit log for any MCP server.

Put it in front of an MCP server's own command, e.g. in Claude Desktop's
config:

    "github": {
      "command": "uvx",
      "args": ["agentraas", "wrap", "--", "npx", "-y", "@modelcontextprotocol/server-github"]
    }

It speaks MCP's stdio transport on both sides and passes everything
through unchanged, except `tools/call` for write tools: an identical call
(same tool, same arguments) repeated inside the dedup window gets the
first call's result back instead of running again, so an agent that
retries or loops can't send the same email or open the same issue twice.

Read-only tools are never deduplicated (the server's `readOnlyHint`
annotation, or names like get_/list_/search_), so lookups stay fresh. A
call that fails (JSON-RPC error or `isError: true`) is not cached, so a
retry runs for real. Every tool call is appended to a JSON-lines audit log.
"""

import argparse
import hashlib
import json
import os
import subprocess
import sys
import threading
import time

from .local import _PENDING, SQLiteStore

READ_PREFIXES = ("get", "list", "search", "read", "fetch", "find", "query", "describe", "lookup", "view", "show", "count", "check")
HOME = os.path.join(os.path.expanduser("~"), ".agentraas")


class Wrapper:
    def __init__(self, cmd, store, log_path, ttl, wait, include, exclude, out=sys.stdout):
        self.cmd, self.store, self.ttl, self.wait = cmd, store, ttl, wait
        self.include, self.exclude = set(include), set(exclude)
        self.out = out
        self.write_lock = threading.Lock()
        self.inflight = {}       # request id -> (dedup key, tool, started)
        self.list_ids = set()    # ids of our clients' tools/list requests
        self.read_only = set()   # tools the server annotated readOnlyHint
        self.server_name = " ".join(cmd)
        self.log = open(log_path, "a", buffering=1) if log_path else None

    # ── helpers ──
    def send(self, msg):
        with self.write_lock:
            self.out.write(json.dumps(msg, separators=(",", ":")) + "\n")
            self.out.flush()

    def audit(self, tool, key, outcome, started, **extra):
        if self.log:
            self.log.write(json.dumps({"ts": time.strftime("%Y-%m-%dT%H:%M:%S%z"), "server": self.server_name, "tool": tool,
                                       "args_sha256": key[-64:], "outcome": outcome,
                                       "ms": round((time.monotonic() - started) * 1000, 1), **extra}) + "\n")

    def should_dedupe(self, tool):
        if tool in self.include:
            return True
        if tool in self.exclude or tool in self.read_only:
            return False
        return not tool.lower().startswith(READ_PREFIXES)

    def key(self, tool, args):
        raw = json.dumps({"server": self.server_name, "tool": tool, "arguments": args}, sort_keys=True, separators=(",", ":"))
        return "dedup:mcp:" + hashlib.sha256(raw.encode()).hexdigest()

    # ── client -> server ──
    def from_client(self, msg, child_in):
        method = msg.get("method")
        if method == "tools/list" and "id" in msg:
            self.list_ids.add(msg["id"])
        if method == "tools/call" and "id" in msg:
            params = msg.get("params") or {}
            tool, args = params.get("name", ""), params.get("arguments") or {}
            started = time.monotonic()
            if self.should_dedupe(tool):
                k = self.key(tool, args)
                if not self.store.claim(k, self.ttl):
                    threading.Thread(target=self.answer_duplicate, args=(msg["id"], tool, k, started, child_in, msg), daemon=True).start()
                    return
                self.inflight[msg["id"]] = (k, tool, started)
            else:
                self.inflight[msg["id"]] = (None, tool, started)
        child_in.write(json.dumps(msg, separators=(",", ":")) + "\n")
        child_in.flush()

    def answer_duplicate(self, req_id, tool, k, started, child_in, msg):
        deadline = time.monotonic() + self.wait
        while True:
            v = self.store.get(k)
            if v is not None and v != _PENDING:
                result = json.loads(v)["result"]
                result = {**result, "_meta": {**(result.get("_meta") or {}), "agentraas/deduplicated": True}}
                self.send({"jsonrpc": "2.0", "id": req_id, "result": result})
                self.audit(tool, k, "deduplicated", started)
                return
            if v is None and self.store.claim(k, self.ttl):
                # the first call failed and released its slot: this one runs for real
                self.inflight[req_id] = (k, tool, started)
                child_in.write(json.dumps(msg, separators=(",", ":")) + "\n")
                child_in.flush()
                return
            if time.monotonic() > deadline:
                self.send({"jsonrpc": "2.0", "id": req_id, "error": {"code": -32000, "message": f"agentraas: identical call to {tool} still in flight after {self.wait}s; not re-running it"}})
                self.audit(tool, k, "in_flight_timeout", started)
                return
            time.sleep(0.1)

    # ── server -> client ──
    def from_server(self, msg):
        rid = msg.get("id")
        if rid in self.list_ids and "result" in msg:
            self.list_ids.discard(rid)
            for t in (msg["result"].get("tools") or []):
                if (t.get("annotations") or {}).get("readOnlyHint") is True:
                    self.read_only.add(t.get("name"))
        entry = self.inflight.pop(rid, None) if rid is not None and ("result" in msg or "error" in msg) else None
        if entry:
            k, tool, started = entry
            failed = "error" in msg or bool((msg.get("result") or {}).get("isError"))
            if k:
                if failed:
                    self.store.release(k)
                else:
                    try:
                        self.store.complete(k, json.dumps({"result": msg["result"]}), self.ttl)
                    except TypeError:
                        self.store.release(k)
            self.audit(tool, k or self.key(tool, None), ("failed" if failed else "executed") if k else ("passthrough_failed" if failed else "passthrough"), started)
        self.send(msg)

    # ── run ──
    def run(self, stdin=sys.stdin):
        child = subprocess.Popen(self.cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=None, text=True, bufsize=1)

        def pump_server():
            for line in child.stdout:
                line = line.strip()
                if not line:
                    continue
                try:
                    self.from_server(json.loads(line))
                except ValueError:
                    with self.write_lock:
                        self.out.write(line + "\n")
                        self.out.flush()
        t = threading.Thread(target=pump_server, daemon=True)
        t.start()
        try:
            for line in stdin:
                line = line.strip()
                if not line:
                    continue
                try:
                    msg = json.loads(line)
                except ValueError:
                    child.stdin.write(line + "\n"); child.stdin.flush()
                    continue
                if isinstance(msg, list):  # JSON-RPC batch: pass through untouched
                    child.stdin.write(line + "\n"); child.stdin.flush()
                    continue
                self.from_client(msg, child.stdin)
        except (BrokenPipeError, KeyboardInterrupt):
            pass
        finally:
            try:
                child.stdin.close()
            except OSError:
                pass
            t.join(timeout=5)
            child.terminate()
        return child.wait()


def main(argv=None):
    argv = sys.argv[1:] if argv is None else argv
    if "--" not in argv:
        print("usage: agentraas wrap [options] -- <mcp server command...>", file=sys.stderr)
        return 2
    i = argv.index("--")
    opts, cmd = argv[:i], argv[i + 1:]
    p = argparse.ArgumentParser(prog="agentraas wrap", description="Exactly-once tool calls and an audit log for any stdio MCP server.")
    p.add_argument("--ttl", type=int, default=600, help="seconds an identical write call is deduplicated (default 600)")
    p.add_argument("--wait", type=float, default=30, help="seconds a duplicate waits for an in-flight first call (default 30)")
    p.add_argument("--db", default=os.path.join(HOME, "mcp-dedup.db"), help="SQLite file for dedup state")
    p.add_argument("--log", default=os.path.join(HOME, "mcp-audit.jsonl"), help="JSON-lines audit log ('' to disable)")
    p.add_argument("--dedupe", action="append", default=[], metavar="TOOL", help="always dedupe this tool, even if it looks read-only")
    p.add_argument("--no-dedupe", action="append", default=[], metavar="TOOL", help="never dedupe this tool")
    a = p.parse_args(opts)
    if not cmd:
        p.error("no MCP server command after --")
    for path in (a.db, a.log):
        if path:
            os.makedirs(os.path.dirname(os.path.abspath(path)), exist_ok=True)
    return Wrapper(cmd, SQLiteStore(a.db), a.log or None, a.ttl, a.wait, a.dedupe, a.no_dedupe).run()


if __name__ == "__main__":
    sys.exit(main())
