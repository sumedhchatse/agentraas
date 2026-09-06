-- Semantic/entity-level idempotency key enhancements (see PORT_PROGRESS.md's
-- feature plan): a dedup rule can now specify its own dedup window instead
-- of always using the system default (e.g. a 15-minute rolling window
-- instead of 24h), and can opt into normalizing each configured key field's
-- value (trim/lowercase/numeric-coerce) before hashing, so trivially
-- different-looking values for the SAME field ("100" vs 100, " Jane " vs
-- "jane") still count as the same entity. Both nullable/default-off: an
-- existing rule with neither set behaves exactly as before this migration.
ALTER TABLE custom_dedup_rules ADD COLUMN IF NOT EXISTS ttl_seconds INTEGER;
ALTER TABLE custom_dedup_rules ADD COLUMN IF NOT EXISTS normalize BOOLEAN NOT NULL DEFAULT false;
