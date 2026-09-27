-- Lets a dead-letter replay resend under the original call's Idempotency-Key
-- and resolve an outcome-unknown dedup slot. NULL for entries written before
-- this column existed; those replay without a key, as before.
ALTER TABLE dead_letter_queue ADD COLUMN IF NOT EXISTS dedup_hash TEXT;
