"""Run: python test_chaos.py  (no dependencies)"""
import json
import threading
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from agentraas.chaos import serve


def post(url, body, headers=None, retries=1):
    """A typical naive client: retry on any network error."""
    for attempt in range(retries + 1):
        req = urllib.request.Request(url, data=json.dumps(body).encode(), method="POST",
                                     headers={"Content-Type": "application/json", **(headers or {})})
        try:
            with urllib.request.urlopen(req, timeout=5) as r:
                return r.status, json.loads(r.read())
        except Exception:
            if attempt == retries:
                raise


def start(**kw):
    server, rec = serve(port=0, **kw)
    return f"http://127.0.0.1:{server.server_address[1]}", server, rec


def test_naive_retry_is_reported_as_duplicate():
    url, server, rec = start(upstream=None)
    post(url + "/v1/charges", {"amount": 4200})
    urllib.request.urlopen(url + "/v1/charges")  # GETs are never faulted or counted
    server.shutdown()
    text, found = rec.report()
    assert found and "2x  POST /v1/charges" in text, text


def test_idempotency_key_retry_is_protected():
    url, server, rec = start(upstream=None)
    post(url + "/v1/charges", {"amount": 4200}, headers={"Idempotency-Key": "order-7"})
    server.shutdown()
    text, found = rec.report()
    assert not found and "Protected by an idempotency key: 1" in text, text


def test_error_after_commit_mode():
    url, server, rec = start(upstream=None, fault="error-after-commit")
    status, _ = post(url + "/v1/emails", {"to": "a@b.c"})
    server.shutdown()
    assert status == 200 and rec.report()[1]


def test_forwards_to_real_upstream_and_counts_its_executions():
    executed = []

    class Up(BaseHTTPRequestHandler):
        def log_message(self, *a):
            pass

        def do_POST(self):
            self.rfile.read(int(self.headers["Content-Length"]))
            executed.append((self.path, self.headers["Host"], self.headers.get("Authorization")))
            data = b'{"id":"re_1"}'
            self.send_response(201)
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)

    up = ThreadingHTTPServer(("127.0.0.1", 0), Up)
    threading.Thread(target=up.serve_forever, daemon=True).start()
    upstream = f"http://127.0.0.1:{up.server_address[1]}/api"
    url, server, rec = start(upstream=upstream)
    status, body = post(url + "/refunds", {"charge": "ch_1"}, headers={"Authorization": "Bearer sk_test"})
    server.shutdown()
    up.shutdown()
    assert status == 201 and body == {"id": "re_1"}
    assert len(executed) == 2 and executed[0][0] == "/api/refunds" and executed[0][2] == "Bearer sk_test"
    assert executed[0][1] == f"127.0.0.1:{up.server_address[1]}"
    assert rec.report()[1]


def test_no_retry_means_no_duplicate():
    url, server, rec = start(upstream=None, fault="none")
    post(url + "/v1/charges", {"amount": 1}, retries=0)
    post(url + "/v1/charges", {"amount": 2}, retries=0)
    server.shutdown()
    assert not rec.report()[1]


if __name__ == "__main__":
    for name, fn in list(globals().items()):
        if name.startswith("test_"):
            fn()
            print("ok", name)
