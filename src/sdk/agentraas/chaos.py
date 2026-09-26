"""
agentraas-chaos: find the API calls your agent would execute twice.

Point your agent (or n8n workflow, or test suite) at this instead of the
real API. The first time each distinct write request (POST/PUT/PATCH/
DELETE) arrives, it is executed and then the response is dropped, exactly
like a network failure after the provider already did the work. If your
code retries it without an idempotency key, that's a duplicate charge,
email or record in production. This reports every one of them.

    agentraas-chaos --mock -- python my_agent.py      # safe: nothing real is called
    agentraas-chaos --upstream https://api.stripe.com -- pytest   # use TEST keys

The child command gets AGENTRAAS_CHAOS_URL (e.g. http://127.0.0.1:8787)
to use as its API base URL. Exit code is 1 if any duplicate was found,
otherwise the child's own exit code. Without a command, it runs until
Ctrl-C and then prints the report.
"""

import argparse
import hashlib
import http.client
import json
import os
import subprocess
import sys
import threading
import urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

WRITE_METHODS = {"POST", "PUT", "PATCH", "DELETE"}
HOP_BY_HOP = {"connection", "keep-alive", "transfer-encoding", "te", "trailer", "upgrade", "proxy-authorization", "proxy-connection", "host", "content-length", "accept-encoding"}
IDEMPOTENCY_HEADERS = ("idempotency-key", "x-idempotency-key", "x-request-id")


class Recorder:
    def __init__(self):
        self.lock = threading.Lock()
        self.executions = {}   # request fingerprint -> times executed upstream
        self.faulted = set()   # fingerprints whose first response was dropped
        self.keys = {}         # idempotency key -> fingerprint of first use
        self.total = 0
        self.protected = []    # (method, path) retried with the same idempotency key

    def report(self):
        dups = [(fp, n) for fp, n in self.executions.items() if n > 1]
        lines = [f"agentraas-chaos: {self.total} write requests, {len(self.faulted)} responses dropped after execution"]
        if dups:
            lines.append(f"\n  DUPLICATES: {len(dups)} action(s) executed more than once\n")
            for (method, path, body_hash), n in dups:
                lines.append(f"    {n}x  {method} {path}  (body sha256 {body_hash[:12]})")
            lines.append("\n  Fix: send an Idempotency-Key header that stays the same across retries, for")
            lines.append("  providers that honor one. With agentraas.local.exactly_once, give the function an")
            lines.append("  `idempotency_key` parameter and send it as that header; for providers without")
            lines.append("  idempotency support, route the call through an AgentRaaS server.")
        else:
            lines.append("\n  No duplicates: every retried write was deduplicated or never retried.")
        if self.protected:
            lines.append(f"\n  Protected by an idempotency key: {len(self.protected)} retr{'y' if len(self.protected) == 1 else 'ies'}")
        return "\n".join(lines), bool(dups)


def make_handler(rec, upstream, fault):
    up = urllib.parse.urlsplit(upstream) if upstream else None

    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, *a):
            pass

        def _handle(self):
            body = self.rfile.read(int(self.headers.get("Content-Length") or 0))
            method = self.command
            idem = next((self.headers[h] for h in IDEMPOTENCY_HEADERS if self.headers.get(h)), None)
            fp = (method, self.path.split("?")[0], hashlib.sha256(body).hexdigest())

            is_write = method in WRITE_METHODS
            with rec.lock:
                if is_write:
                    rec.total += 1
                # A retry carrying an idempotency key already seen is one the
                # provider (and our mock) will dedupe itself: not executed again.
                replay = is_write and idem is not None and idem in rec.keys
                if replay:
                    rec.protected.append(fp[:2])
                elif is_write and idem is not None:
                    rec.keys[idem] = fp
                drop = is_write and not replay and fault != "none" and fp not in rec.faulted
                if drop:
                    rec.faulted.add(fp)

            status, headers, data = self._forward(method, body, replay, idem)
            if is_write and not replay and 200 <= status < 300:
                with rec.lock:
                    rec.executions[fp] = rec.executions.get(fp, 0) + 1

            if drop and fault == "lost-response":
                self.close_connection = True
                self.connection.close()  # the work happened; the caller never hears about it
                return
            if drop and fault == "error-after-commit":
                status, headers, data = 502, [("Content-Type", "application/json")], b'{"error":"bad gateway (injected by agentraas-chaos)"}'

            self.send_response(status)
            for k, v in headers:
                if k.lower() not in HOP_BY_HOP:
                    self.send_header(k, v)
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)

        def _forward(self, method, body, replay, idem):
            if up is None:  # --mock: pretend every call succeeds
                rid = hashlib.sha256((idem or repr((method, self.path, body))).encode()).hexdigest()[:16]
                data = json.dumps({"id": f"mock_{rid}", "ok": True, "replayed": replay}).encode()
                return 200, [("Content-Type", "application/json")], data
            conn_cls = http.client.HTTPSConnection if up.scheme == "https" else http.client.HTTPConnection
            conn = conn_cls(up.netloc, timeout=60)
            headers = {k: v for k, v in self.headers.items() if k.lower() not in HOP_BY_HOP}
            headers["Host"] = up.netloc
            conn.request(method, up.path.rstrip("/") + self.path, body=body or None, headers=headers)
            resp = conn.getresponse()
            data = resp.read()
            conn.close()
            return resp.status, resp.getheaders(), data

        do_GET = do_POST = do_PUT = do_PATCH = do_DELETE = do_HEAD = do_OPTIONS = _handle

    return Handler


def serve(port=8787, upstream=None, fault="lost-response"):
    """Start the chaos proxy in a background thread. Returns (server, recorder)."""
    rec = Recorder()
    server = ThreadingHTTPServer(("127.0.0.1", port), make_handler(rec, upstream, fault))
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server, rec


def main(argv=None):
    argv = sys.argv[1:] if argv is None else argv
    cmd = []
    if "--" in argv:
        i = argv.index("--")
        argv, cmd = argv[:i], argv[i + 1:]
    p = argparse.ArgumentParser(prog="agentraas-chaos", description=__doc__.split("\n\n")[1], formatter_class=argparse.RawDescriptionHelpFormatter)
    target = p.add_mutually_exclusive_group(required=True)
    target.add_argument("--upstream", help="real API base URL to forward to (use test-mode keys)")
    target.add_argument("--mock", action="store_true", help="answer every call with a fake 200; nothing real is called")
    p.add_argument("--port", type=int, default=8787)
    p.add_argument("--fault", choices=["lost-response", "error-after-commit", "none"], default="lost-response",
                   help="what happens to the first response of each distinct write request (default: lost-response)")
    args = p.parse_args(argv)

    server, rec = serve(args.port, args.upstream, args.fault)
    url = f"http://127.0.0.1:{server.server_address[1]}"
    print(f"agentraas-chaos listening on {url} -> {args.upstream or 'mock'} (fault: {args.fault})", file=sys.stderr)

    code = 0
    if cmd:
        code = subprocess.call(cmd, env={**os.environ, "AGENTRAAS_CHAOS_URL": url})
    else:
        try:
            threading.Event().wait()
        except KeyboardInterrupt:
            pass
    server.shutdown()
    text, found = rec.report()
    print(text, file=sys.stderr)
    return 1 if found else code


if __name__ == "__main__":
    sys.exit(main())
