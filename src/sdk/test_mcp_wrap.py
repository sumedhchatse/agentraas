"""Run: python test_mcp_wrap.py  (no dependencies; spawns a fake stdio MCP server)"""
import json
import os
import subprocess
import sys
import tempfile
import threading
import time

FAKE_SERVER = r'''
import json, sys, time, collections
count = collections.Counter()
TOOLS = [
    {"name": "send_email", "inputSchema": {"type": "object"}},
    {"name": "get_inbox", "inputSchema": {"type": "object"}},
    {"name": "inbox_snapshot", "inputSchema": {"type": "object"}, "annotations": {"readOnlyHint": True}},
    {"name": "flaky_post", "inputSchema": {"type": "object"}},
    {"name": "slow_create", "inputSchema": {"type": "object"}},
]
for line in sys.stdin:
    msg = json.loads(line)
    if "id" not in msg:
        continue  # notification
    m, rid = msg["method"], msg["id"]
    if m == "initialize":
        res = {"protocolVersion": "2025-06-18", "capabilities": {"tools": {}}, "serverInfo": {"name": "fake", "version": "1"}}
    elif m == "tools/list":
        res = {"tools": TOOLS}
    elif m == "tools/call":
        name = msg["params"]["name"]
        count[name] += 1
        if name == "slow_create":
            time.sleep(0.5)
        err = name == "flaky_post" and count[name] == 1
        res = {"content": [{"type": "text", "text": f"{name} run #{count[name]}"}], "isError": err}
    else:
        res = {}
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": rid, "result": res}) + "\n")
    sys.stdout.flush()
'''


class Client:
    def __init__(self):
        d = tempfile.mkdtemp()
        self.log = os.path.join(d, "audit.jsonl")
        server = os.path.join(d, "server.py")
        open(server, "w").write(FAKE_SERVER)
        self.p = subprocess.Popen([sys.executable, "-m", "agentraas.cli", "wrap", "--db", os.path.join(d, "d.db"), "--log", self.log,
                                   "--", sys.executable, server],
                                  stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True, bufsize=1,
                                  cwd=os.path.dirname(os.path.abspath(__file__)))
        self.responses, self.lock, self.next_id = {}, threading.Condition(), 0
        threading.Thread(target=self._read, daemon=True).start()

    def _read(self):
        for line in self.p.stdout:
            msg = json.loads(line)
            with self.lock:
                self.responses[msg["id"]] = msg
                self.lock.notify_all()

    def send(self, method, params=None, notify=False):
        msg = {"jsonrpc": "2.0", "method": method, "params": params or {}}
        if not notify:
            self.next_id += 1
            msg["id"] = self.next_id
        self.p.stdin.write(json.dumps(msg) + "\n")
        self.p.stdin.flush()
        return msg.get("id")

    def wait(self, rid, timeout=10):
        with self.lock:
            self.lock.wait_for(lambda: rid in self.responses, timeout)
            return self.responses[rid]

    def call(self, tool, args=None):
        return self.wait(self.send("tools/call", {"name": tool, "arguments": args or {}}))["result"]

    def close(self):
        self.p.stdin.close()
        self.p.wait(timeout=10)


def text(result):
    return result["content"][0]["text"]


def deduped(result):
    return (result.get("_meta") or {}).get("agentraas/deduplicated") is True


def test_wrap():
    c = Client()
    assert c.wait(c.send("initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}))["result"]["serverInfo"]["name"] == "fake"
    c.send("notifications/initialized", notify=True)
    assert len(c.wait(c.send("tools/list"))["result"]["tools"]) == 5

    # identical write call: runs once, the repeat gets the stored result
    a = c.call("send_email", {"to": "a@b.c", "body": "hi"})
    b = c.call("send_email", {"to": "a@b.c", "body": "hi"})
    assert text(a) == text(b) == "send_email run #1" and not deduped(a) and deduped(b)
    # different arguments: a different action
    assert text(c.call("send_email", {"to": "x@y.z", "body": "hi"})) == "send_email run #2"

    # read-only by name and by annotation: always run
    assert text(c.call("get_inbox")) == "get_inbox run #1" and text(c.call("get_inbox")) == "get_inbox run #2"
    assert text(c.call("inbox_snapshot")) == "inbox_snapshot run #1" and text(c.call("inbox_snapshot")) == "inbox_snapshot run #2"

    # a failed call is not cached: the retry runs for real
    first = c.call("flaky_post", {"n": 1})
    second = c.call("flaky_post", {"n": 1})
    assert first["isError"] and text(second) == "flaky_post run #2" and not deduped(second)

    # two identical calls in flight at once: one execution, same answer for both
    r1, r2 = c.send("tools/call", {"name": "slow_create", "arguments": {"k": 1}}), c.send("tools/call", {"name": "slow_create", "arguments": {"k": 1}})
    x, y = c.wait(r1)["result"], c.wait(r2)["result"]
    assert text(x) == text(y) == "slow_create run #1" and deduped(x) != deduped(y)

    c.close()
    outcomes = [json.loads(l)["outcome"] for l in open(c.log)]
    assert outcomes.count("deduplicated") == 2 and outcomes.count("executed") == 4 and outcomes.count("failed") == 1, outcomes
    assert outcomes.count("passthrough") == 4, outcomes


if __name__ == "__main__":
    test_wrap()
    print("ok test_wrap")
