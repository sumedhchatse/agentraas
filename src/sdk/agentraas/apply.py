"""`agentraas apply -f agentraas.yaml`: keep an org's rules in Git.

The file holds one or more YAML documents, each a resource:

    apiVersion: agentraas.io/v1alpha1
    kind: DedupRule            # or ValidationRule, ActionPolicy, SpendCap
    metadata:
      name: stripe-charge-once # for people reading the file; not stored
      org: org_acme
    spec:
      service: stripe
      action: charges.create
      fields: [customer, amount]
      ttlSeconds: 300

Spec keys are the API's fields in camelCase (`ttl_seconds` -> `ttlSeconds`,
`on_violation` -> `onViolation`, `agent_id` -> `agentId`). `apply` makes
the server match the file: it creates what is missing and replaces what
differs. With `--prune` it also deletes rules of these four kinds that the
file does not mention, in the orgs the file names; without it, rules made
in the dashboard are left alone. `--dry-run` prints the plan only.

Logs in with AGENTRAAS_EMAIL / AGENTRAAS_PASSWORD (a dedicated user with
write access to the org, so CI holds no one's personal password) against
AGENTRAAS_URL (default https://agentraas.io).
"""
import argparse
import json
import os
import re
import sys

import requests

API_VERSIONS = ("agentraas.io/v1alpha1",)

# kind -> (API path, list key or None for a bare array, identity keys, server upserts on identity?)
# Identity = what makes two entries "the same rule"; the file may hold one per identity.
KINDS = {
    "DedupRule": ("/api/v1/dedup-rules", None, ("service", "action"), True),
    "ValidationRule": ("/api/v1/validation-rules", None, ("service", "action"), True),
    "ActionPolicy": ("/api/v1/action-policies", "policies", ("agent_id", "service", "action", "effect"), False),
    "SpendCap": ("/api/v1/spend-cap-rules", "rules", ("agent_id", "service", "action", "window"), False),
}
# Server-side defaults, so a file that leaves them out matches what the server stored.
DEFAULTS = {
    "DedupRule": {"ttl_seconds": None, "normalize": False, "semantic_enabled": False, "semantic_threshold": 0.85},
    "ValidationRule": {},
    "ActionPolicy": {"agent_id": None, "fields": None, "on_violation": "block"},
    "SpendCap": {"agent_id": None},
}
COMPARED = {
    "DedupRule": ("service", "action", "fields", "ttl_seconds", "normalize", "semantic_enabled", "semantic_threshold"),
    "ValidationRule": ("service", "action", "fields"),
    "ActionPolicy": ("agent_id", "service", "action", "effect", "fields", "on_violation"),
    "SpendCap": ("agent_id", "service", "action", "window", "max_calls", "on_exceed"),
}


def snake(key):
    return re.sub(r"(?<!^)(?=[A-Z])", "_", key).lower()


def normalize(kind, org, spec):
    """Spec (or a listed server row) -> the comparable form."""
    full = dict(DEFAULTS[kind])
    full.update({snake(k): v for k, v in spec.items()})
    out = {k: full.get(k) for k in COMPARED[kind]}
    if out.get("semantic_threshold") is not None:
        out["semantic_threshold"] = round(float(out["semantic_threshold"]), 4)  # stored as f32
    out["org_id"] = org
    return out


def identity(kind, rule):
    return (kind, json.dumps([rule["org_id"]] + [rule[k] for k in KINDS[kind][2]]))


def load(path):
    try:
        import yaml
    except ImportError:
        sys.exit('agentraas apply needs PyYAML: pip install "agentraas[apply]"')
    with open(path) as f:
        docs = [d for d in yaml.safe_load_all(f) if d]
    desired = []
    for i, d in enumerate(docs, 1):
        where = "{} document {}".format(path, i)
        if d.get("apiVersion") not in API_VERSIONS:
            sys.exit("{}: apiVersion must be one of {}".format(where, ", ".join(API_VERSIONS)))
        kind = d.get("kind")
        if kind not in KINDS:
            sys.exit("{}: kind must be one of {}".format(where, ", ".join(KINDS)))
        org = (d.get("metadata") or {}).get("org")
        if not org:
            sys.exit("{}: metadata.org is required".format(where))
        if not isinstance(d.get("spec"), dict):
            sys.exit("{}: spec is required".format(where))
        desired.append((kind, normalize(kind, org, d["spec"]), d["spec"], (d.get("metadata") or {}).get("name", "")))
    return desired


def plan(desired, current, prune):
    """-> (creates, deletes). `current` is {kind: [server rows, each with "id" and "norm"]}.
    A changed rule is created again; for kinds the server doesn't upsert, the old rows are deleted
    too (including extra copies of the same rule, e.g. left by an interrupted run)."""
    have = {}
    for kind, rows in current.items():
        for row in rows:
            have.setdefault(identity(kind, row["norm"]), []).append(row)
    creates, deletes, wanted = [], [], set()
    for kind, norm, spec, name in desired:
        key = identity(kind, norm)
        if key in wanted:
            sys.exit("{} {} {}.{} is listed twice".format(kind, norm["org_id"], norm["service"], norm["action"]))
        wanted.add(key)
        rows = have.get(key, [])
        keep = next((r for r in rows if r["norm"] == norm), None)
        if keep is None:
            creates.append((kind, norm, spec, name))
        if not KINDS[kind][3]:
            deletes += [(kind, r) for r in rows if r is not keep]
    if prune:
        deletes += [(key[0], r) for key, rows in have.items() if key not in wanted for r in rows]
    return creates, deletes


def body_for(kind, org, spec):
    body = {snake(k): v for k, v in spec.items()}
    body["org_id"] = org
    return body


def main(argv=None):
    ap = argparse.ArgumentParser(prog="agentraas apply", description="Make the server's rules match a YAML file.")
    ap.add_argument("-f", "--file", required=True)
    ap.add_argument("--prune", action="store_true", help="delete rules in these orgs that the file does not list")
    ap.add_argument("--dry-run", action="store_true", help="print the plan, change nothing")
    args = ap.parse_args(argv)

    desired = load(args.file)
    orgs = {norm["org_id"] for _, norm, _, _ in desired}
    base = os.environ.get("AGENTRAAS_URL", "https://agentraas.io").rstrip("/")
    email, password = os.environ.get("AGENTRAAS_EMAIL"), os.environ.get("AGENTRAAS_PASSWORD")
    if not email or not password:
        sys.exit("Set AGENTRAAS_EMAIL and AGENTRAAS_PASSWORD (a user with write access to the org).")
    s = requests.Session()
    r = s.post(base + "/api/v1/auth/login", json={"email": email, "password": password}, timeout=30)
    if r.status_code != 200:
        sys.exit("login failed ({}): {}".format(r.status_code, r.text[:200]))

    current = {}
    for kind, (path, key, _, _) in KINDS.items():
        r = s.get(base + path, timeout=30)
        r.raise_for_status()
        rows = r.json() if key is None else r.json()[key]
        current[kind] = [dict(row, norm=normalize(kind, row["org_id"], row)) for row in rows if row["org_id"] in orgs]

    creates, deletes = plan(desired, current, args.prune)
    for kind, norm, _, name in creates:
        print("+ {} {} {}.{}{}".format(kind, norm["org_id"], norm["service"], norm["action"], " ({})".format(name) if name else ""))
    for kind, row in deletes:
        print("- {} {} {}.{} (id {})".format(kind, row["org_id"], row["norm"]["service"], row["norm"]["action"], row["id"]))
    if not creates and not deletes:
        print("no changes")
        return 0
    if args.dry_run:
        return 0

    failed = 0
    for kind, norm, spec, _ in creates:
        r = s.post(base + KINDS[kind][0], json=body_for(kind, norm["org_id"], spec), timeout=30)
        if r.status_code != 200:
            failed += 1
            print("error: {} {}.{}: {} {}".format(kind, norm["service"], norm["action"], r.status_code, r.text[:300]), file=sys.stderr)
    for kind, row in deletes:
        r = s.delete("{}{}/{}".format(base, KINDS[kind][0], row["id"]), timeout=30)
        if r.status_code != 200:
            failed += 1
            print("error: deleting {} id {}: {} {}".format(kind, row["id"], r.status_code, r.text[:300]), file=sys.stderr)
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
