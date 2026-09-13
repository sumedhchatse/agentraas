-- Fuzzy/semantic similarity dedup: catches near-duplicate payloads that
-- exact-hash and field-hash dedup (033) can't, e.g. "Transfer $500 to John"
-- vs "Send five hundred dollars to John Doe". Opt-in per rule, Team+ tier
-- (see agentraas_core::tier). Nullable/default-off: an existing rule with
-- semantic_enabled=false behaves exactly as before this migration.
ALTER TABLE custom_dedup_rules ADD COLUMN IF NOT EXISTS semantic_enabled BOOLEAN NOT NULL DEFAULT false;
ALTER TABLE custom_dedup_rules ADD COLUMN IF NOT EXISTS semantic_threshold REAL NOT NULL DEFAULT 0.85;
