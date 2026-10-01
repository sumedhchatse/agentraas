"""Offline check of `agentraas apply`'s plan: no server needed."""
import os
import tempfile

from agentraas.apply import load, normalize, plan

YAML = """
apiVersion: agentraas.io/v1alpha1
kind: DedupRule
metadata: {name: charge-once, org: org_a}
spec: {service: stripe, action: charges.create, fields: [customer, amount], ttlSeconds: 300}
---
apiVersion: agentraas.io/v1alpha1
kind: SpendCap
metadata: {org: org_a}
spec: {service: stripe, action: refunds.create, window: day, maxCalls: 100, onExceed: block}
---
apiVersion: agentraas.io/v1alpha1
kind: ActionPolicy
metadata: {org: org_a}
spec: {service: stripe, action: payouts.create, effect: deny}
"""


def row(kind, id, **fields):
    return dict(fields, id=id, norm=normalize(kind, fields["org_id"], fields))


def main():
    with tempfile.NamedTemporaryFile("w", suffix=".yaml", delete=False) as f:
        f.write(YAML)
    desired = load(f.name)
    os.unlink(f.name)

    # Empty server: everything is created, nothing deleted.
    creates, deletes = plan(desired, {}, prune=True)
    assert [c[0] for c in creates] == ["DedupRule", "SpendCap", "ActionPolicy"] and deletes == []

    current = {
        # identical, as the server lists it (f32 threshold, defaults filled in)
        "DedupRule": [row("DedupRule", 1, org_id="org_a", service="stripe", action="charges.create", fields=["customer", "amount"],
                          ttl_seconds=300, normalize=False, semantic_enabled=False, semantic_threshold=0.8500000238)],
        # same cap, different number: replaced (create new, delete old)
        "SpendCap": [row("SpendCap", 7, org_id="org_a", agent_id=None, service="stripe", action="refunds.create", window="day", max_calls=50, on_exceed="block")],
        # identical policy plus one made in the dashboard
        "ActionPolicy": [row("ActionPolicy", 3, org_id="org_a", agent_id=None, service="stripe", action="payouts.create", effect="deny", fields=None, on_violation="block"),
                         row("ActionPolicy", 4, org_id="org_a", agent_id=None, service="email", action="send", effect="deny", fields=None, on_violation="block")],
    }
    creates, deletes = plan(desired, current, prune=False)
    assert [(c[0], c[1]["max_calls"]) for c in creates] == [("SpendCap", 100)], creates
    assert [(k, r["id"]) for k, r in deletes] == [("SpendCap", 7)], deletes

    # An interrupted run left the new cap next to the old one: keep the matching one, drop the other.
    both = dict(current, SpendCap=current["SpendCap"] + [row("SpendCap", 8, org_id="org_a", agent_id=None, service="stripe", action="refunds.create", window="day", max_calls=100, on_exceed="block")])
    creates, deletes = plan(desired, both, prune=False)
    assert creates == [] and [(k, r["id"]) for k, r in deletes] == [("SpendCap", 7)], (creates, deletes)

    creates, deletes = plan(desired, current, prune=True)
    assert sorted((k, r["id"]) for k, r in deletes) == [("ActionPolicy", 4), ("SpendCap", 7)], deletes
    print("test_apply: ok")


if __name__ == "__main__":
    main()
