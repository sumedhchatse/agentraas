"""Run: TEST_BASE_URL=http://localhost:13001 python test_tunnel.py

End-to-end: needs a running AgentRaaS stack and websocket-client. Starts a
local echo server, runs the real `agentraas tunnel` CLI against it, sends a
request to the public tunnel URL and checks it came back from the echo server.
"""
import json
import os
import subprocess
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, HTTPServer
from urllib.parse import urlparse, parse_qs

import requests

BASE = os.environ.get("TEST_BASE_URL", "http://localhost:13001")
RUN_ID = int(time.time() * 1000)


class Echo(BaseHTTPRequestHandler):
    def do_POST(self):
        body = self.rfile.read(int(self.headers.get("content-length") or 0))
        out = json.dumps({"method": "POST", "path": self.path, "body": body.decode()}).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.end_headers()
        self.wfile.write(out)

    def log_message(self, *a):
        pass


def main():
    s = requests.Session()
    org, agent = "org_pytunnel_{}".format(RUN_ID), "agent_pytunnel_{}".format(RUN_ID)
    r = s.post(BASE + "/api/v1/auth/register", json={"email": "pytunnel-{}@internal.test".format(RUN_ID), "password": "validpassword123", "org_id": org})
    assert r.status_code == 200, r.text
    token = parse_qs(urlparse(r.json()["dev_verify_url"]).query)["verify_token"][0]
    assert s.get(BASE + "/api/v1/auth/verify-email", params={"token": token}).status_code == 200
    r = s.post(BASE + "/api/v1/agents/connect", json={"org_id": org, "agent_id": agent, "label": "python tunnel test"})
    assert r.status_code == 200, r.text
    key = r.json()["api_key"]

    echo = HTTPServer(("127.0.0.1", 0), Echo)
    threading.Thread(target=echo.serve_forever, daemon=True).start()

    cli = subprocess.Popen(
        [sys.executable, "-m", "agentraas.cli", "tunnel", "--port", str(echo.server_port), "--org", org, "--agent", agent,
         "--key", key, "--host", urlparse(BASE).netloc],
        stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True,
    )
    try:
        public_url = None
        deadline = time.time() + 15
        while time.time() < deadline and not public_url:
            line = cli.stdout.readline()
            if "Tunnel is live:" in line:
                public_url = line.split("Tunnel is live:")[1].strip()
        assert public_url, "tunnel never reported a public URL"

        r = requests.post(BASE + urlparse(public_url).path + "/webhooks/stripe", json={"hello": "world"})
        assert r.status_code == 200, r.text
        assert r.json()["path"] == "/webhooks/stripe", r.json()
        assert json.loads(r.json()["body"]) == {"hello": "world"}, r.json()
        line = ""
        while "->" not in line:
            line = cli.stdout.readline()
        assert "POST /webhooks/stripe -> 200" in line, line
    finally:
        cli.terminate()
        echo.shutdown()
    print("ok: python tunnel round-trip")


if __name__ == "__main__":
    main()
