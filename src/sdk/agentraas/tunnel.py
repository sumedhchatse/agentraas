"""`agentraas tunnel`: expose a local port through AgentRaaS for webhook testing.

Same protocol as infra/scripts/tunnel-cli/index.js: open a
WebSocket to /api/v1/tunnel/connect/:org/:agent, print the public URL, and
replay every "request" frame against localhost:<port>.

    pip install "agentraas[tunnel]"
    agentraas tunnel --port 3000 --org org_acme --agent agent_1 --key ar_live_...
"""
import argparse
import base64
import http.client
import json
import sys
import threading
import time
from urllib.parse import quote

SKIP_HEADERS = {"host", "content-length"}


def forward(port, msg):
    """Replay one tunneled request against localhost; returns the response frame."""
    body = base64.b64decode(msg["body_base64"]) if msg.get("body_base64") else None
    headers = {k: v for k, v in (msg.get("headers") or {}).items() if k.lower() not in SKIP_HEADERS}
    try:
        conn = http.client.HTTPConnection("localhost", port, timeout=25)
        conn.request(msg["method"], msg["path"], body=body, headers=headers)
        res = conn.getresponse()
        data = res.read()
        status, ctype = res.status, res.getheader("content-type", "application/octet-stream")
        conn.close()
    except OSError as err:
        status, ctype = 502, "text/plain"
        data = "agentraas-tunnel: could not reach localhost:{} ({})".format(port, err).encode()
    return {
        "type": "response",
        "correlation_id": msg["correlation_id"],
        "status": status,
        "content_type": ctype,
        "body_base64": base64.b64encode(data).decode(),
    }


def main(argv=None):
    p = argparse.ArgumentParser(prog="agentraas tunnel", description="Forward webhooks from a public AgentRaaS URL to a local port.")
    p.add_argument("--port", type=int, required=True, help="local port to forward to")
    p.add_argument("--org", required=True)
    p.add_argument("--agent", required=True)
    p.add_argument("--key", required=True, help="the agent's API key")
    p.add_argument("--host", default="agentraas.io", help="AgentRaaS host (default agentraas.io)")
    args = p.parse_args(argv)

    try:
        import websocket
    except ImportError:
        print('agentraas tunnel needs websocket-client: pip install "agentraas[tunnel]"', file=sys.stderr)
        return 1

    local = args.host.split(":")[0] in ("localhost", "127.0.0.1")
    url = "{}://{}/api/v1/tunnel/connect/{}/{}".format("ws" if local else "wss", args.host, quote(args.org, safe=""), quote(args.agent, safe=""))
    send_lock = threading.Lock()

    def handle(ws, msg):
        started = time.time()
        frame = forward(args.port, msg)
        print("{} {} -> {} ({}ms)".format(msg["method"], msg["path"], frame["status"], int((time.time() - started) * 1000)), flush=True)
        with send_lock:
            ws.send(json.dumps(frame))

    def on_message(ws, raw):
        try:
            msg = json.loads(raw)
        except ValueError:
            return
        if msg.get("type") == "connected":
            print("\n  Tunnel is live: {}\n  Forwarding to:  http://localhost:{}\n".format(msg["url"], args.port), flush=True)
        elif msg.get("type") == "request":
            threading.Thread(target=handle, args=(ws, msg), daemon=True).start()

    errors = []
    app = websocket.WebSocketApp(
        url,
        header=["Authorization: Bearer " + args.key],
        on_message=on_message,
        on_error=lambda ws, err: errors.append(err),
    )
    print("Connecting to {}...".format(args.host), flush=True)
    try:
        app.run_forever()
    except KeyboardInterrupt:
        return 0
    if errors and not isinstance(errors[0], KeyboardInterrupt):
        print("Connection error: {}".format(errors[0]), file=sys.stderr)
        return 1
    print("\nTunnel closed. Re-run this command to open a new one.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
