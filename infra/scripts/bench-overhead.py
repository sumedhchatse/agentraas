#!/usr/bin/env python3
"""Measure the latency AgentRaaS adds in front of an upstream API.

Local stack only (./install.sh, API on :13001). The upstream is the API's
built-in mock payment endpoint (/internal/mockpay: no delay, no I/O), so the
same POST can be timed (a) straight to it and (b) through /v1/webhook, where
the gateway makes that same call itself after auth, dedup, spend caps and
the audit write. Plus (c) a retried duplicate, which AgentRaaS answers
without touching the upstream. A custom upstream on this machine isn't
possible: the SSRF guard refuses private addresses, at forward time too.

The throwaway bench org is set to the enterprise plan in the local
Postgres container (ar-postgres) so its per-agent rate limit (2000/min)
fits the burst; the rate-limit check itself still runs on every call.
Never point this at production.

    python3 infra/scripts/bench-overhead.py [--n 500]
"""
import argparse
import statistics
import subprocess
import time
from urllib.parse import parse_qs, urlparse

import requests

BASE = "http://localhost:13001"


def timed(fn, n):
    out = []
    for i in range(n):
        t = time.perf_counter()
        r = fn(i)
        out.append((time.perf_counter() - t) * 1000)
        assert r.status_code == 200, r.text
    return out


def summary(ms):
    ms = sorted(ms)
    pick = lambda q: ms[min(len(ms) - 1, int(q * len(ms)))]
    return {"p50": statistics.median(ms), "p95": pick(0.95), "p99": pick(0.99)}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--n", type=int, default=500)
    args = ap.parse_args()

    run = int(time.time())
    org, agent = "org_bench_{}".format(run), "agent_bench_{}".format(run)
    s = requests.Session()
    r = s.post(BASE + "/api/v1/auth/register", json={"email": "bench-{}@internal.test".format(run), "password": "validpassword123", "org_id": org})
    assert r.status_code == 200, r.text
    token = parse_qs(urlparse(r.json()["dev_verify_url"]).query)["verify_token"][0]
    s.get(BASE + "/api/v1/auth/verify-email", params={"token": token})
    key = s.post(BASE + "/api/v1/agents/connect", json={"org_id": org, "agent_id": agent, "label": "bench"}).json()["api_key"]

    subprocess.run(
        ["podman", "exec", "ar-postgres", "psql", "-U", "agentraas", "-d", "agentraas", "-qc",
         "UPDATE users SET plan = 'enterprise' WHERE org_id = '{}'".format(org)],
        check=True,
    )

    direct = requests.Session()
    via = requests.Session()
    via.headers["Authorization"] = "Bearer " + key
    url = "{}/v1/webhook/{}/{}".format(BASE, org, agent)

    call_direct = lambda i: direct.post(BASE + "/internal/mockpay", json={"amount": i, "fail": False})
    call_via = lambda i: via.post(url, json={"service": "mockpay", "action": "payment.create", "payload": {"amount": i + 1, "fail": False}})
    call_dup = lambda i: via.post(url, json={"service": "mockpay", "action": "payment.create", "payload": {"amount": 424242, "fail": False}})

    timed(call_direct, 50)
    timed(lambda i: call_via(1_000_000 + i), 50)  # warm-up, distinct payloads
    results = {
        "direct to upstream": summary(timed(call_direct, args.n)),
        "through AgentRaaS": summary(timed(call_via, args.n)),
        "retried duplicate": summary(timed(call_dup, args.n)),
    }
    print("n={} sequential requests per row, same machine, ms".format(args.n))
    print("{:<22}{:>8}{:>8}{:>8}".format("", "p50", "p95", "p99"))
    for name, v in results.items():
        print("{:<22}{:>8.2f}{:>8.2f}{:>8.2f}".format(name, v["p50"], v["p95"], v["p99"]))
    d, t = results["direct to upstream"], results["through AgentRaaS"]
    print("added by AgentRaaS: p50 {:.2f} ms, p95 {:.2f} ms".format(t["p50"] - d["p50"], t["p95"] - d["p95"]))


if __name__ == "__main__":
    main()
