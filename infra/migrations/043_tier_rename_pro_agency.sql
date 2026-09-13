-- Tier restructure: 4 tiers (Community/Pro/Agency/Enterprise) collapsed to
-- 3 (Community/Team/Enterprise) — Pro renamed to Team, Agency folded up
-- into Enterprise. `Tier::from_plan_str` (crates/core/src/tier.rs) already
-- treats "pro"/"agency" as aliases so nothing breaks without this migration,
-- but any UI that surfaces `users.plan` as raw text (e.g. billing_checkout_
-- info's `current_plan` field) would otherwise keep showing the old name
-- forever for an existing row. Safe to re-run — a no-op once applied.
UPDATE users SET plan = 'team' WHERE plan = 'pro';
UPDATE users SET plan = 'enterprise' WHERE plan = 'agency';
