-- 024_audit_log_integrity.sql
-- Tamper-evident audit logging: a hash chain over audit_log rows (each
-- row's hash covers its own fields plus the previous row's hash), so any
-- row altered after the fact breaks the chain from that point forward -
-- detectable via GET /api/v1/admin/audit/verify-integrity (server.js).
--
-- Computed entirely in a BEFORE INSERT trigger, not in application code -
-- logAudit (server.js) is completely unchanged; every row gets a hash
-- automatically regardless of which code path inserted it. This runs for
-- every row (Community and Enterprise alike) - unlike DLP/SSO/SAML, which
-- are consciously-invoked capabilities worth gating per deployment tier,
-- a hash chain is invisible infrastructure that's cheap and strictly
-- beneficial to have on unconditionally. What IS Enterprise-gated (same
-- ENTERPRISE_MODE pattern as everything else) is who can actually call the
-- verify-integrity and SIEM-export endpoints that make use of it.
--
-- Concurrency correctness note (the actual reason this needs a dedicated
-- tip-tracking table instead of the more obvious "SELECT ... ORDER BY id
-- DESC LIMIT 1" + an advisory lock): under READ COMMITTED, a plain SELECT
-- uses the snapshot established at the START of its enclosing statement -
-- blocking mid-statement on an advisory lock and then continuing does NOT
-- get a fresher snapshot once unblocked, so two concurrent inserts could
-- both wait on the same advisory lock, both get released in turn, but
-- BOTH still read the pre-wait snapshot and fork the chain (verified
-- empirically while building this - 30 concurrent inserts reliably
-- produced multiple forks with that approach). `SELECT ... FOR UPDATE`
-- on a row is different: Postgres re-fetches that row's latest committed
-- value once its lock is granted (EvalPlanQual), which is exactly the
-- read-modify-write guarantee this needs.
--
-- Doesn't attempt to make audit_log literally undeletable - the existing
-- retention cleanup job (cleanupAuditLogRetention in server.js) legitimately
-- DELETEs rows past the free/pro retention window; blocking that entirely
-- would break a real, intentional feature. What this DOES block absolutely
-- is UPDATE - once written, a row's contents never change. This is the
-- standard shape real tamper-evident logs use: integrity of the retained
-- window, not eternal unconditional immutability.

CREATE EXTENSION IF NOT EXISTS pgcrypto;

ALTER TABLE audit_log ADD COLUMN IF NOT EXISTS prev_hash TEXT;
ALTER TABLE audit_log ADD COLUMN IF NOT EXISTS row_hash TEXT;

-- Single-row table tracking the current chain tip. Locked with
-- SELECT ... FOR UPDATE (not a plain SELECT) specifically for the fresh-
-- read-after-block guarantee described above.
CREATE TABLE IF NOT EXISTS audit_log_chain_tip (
  id       SMALLINT PRIMARY KEY DEFAULT 1 CHECK (id = 1), -- enforces exactly one row
  tip_hash TEXT
);
-- Seeds from whatever the current last hashed row already is (relevant if
-- this migration ever re-runs against a table that already has hashed rows
-- from an earlier partial run), falling back to NULL on a fresh install.
INSERT INTO audit_log_chain_tip (id, tip_hash)
VALUES (1, (SELECT row_hash FROM audit_log WHERE row_hash IS NOT NULL ORDER BY id DESC LIMIT 1))
ON CONFLICT (id) DO NOTHING;

CREATE OR REPLACE FUNCTION compute_audit_log_hash_chain() RETURNS TRIGGER AS $$
DECLARE
  last_hash TEXT;
BEGIN
  SELECT tip_hash INTO last_hash FROM audit_log_chain_tip WHERE id = 1 FOR UPDATE;
  NEW.prev_hash := last_hash;
  NEW.row_hash := encode(
    digest(
      COALESCE(last_hash, '') || '|' || NEW.req_id || '|' || COALESCE(NEW.org_id, '') || '|' ||
      NEW.service || '|' || NEW.action || '|' || NEW.status || '|' || NEW.created_at::text,
      'sha256'
    ),
    'hex'
  );
  UPDATE audit_log_chain_tip SET tip_hash = NEW.row_hash WHERE id = 1;
  RETURN NEW;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS audit_log_hash_chain ON audit_log;
CREATE TRIGGER audit_log_hash_chain
  BEFORE INSERT ON audit_log
  FOR EACH ROW EXECUTE FUNCTION compute_audit_log_hash_chain();

CREATE OR REPLACE FUNCTION reject_audit_log_update() RETURNS TRIGGER AS $$
BEGIN
  RAISE EXCEPTION 'audit_log rows are append-only and cannot be modified after insert (id=%)', OLD.id;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS audit_log_no_update ON audit_log;
CREATE TRIGGER audit_log_no_update
  BEFORE UPDATE ON audit_log
  FOR EACH ROW EXECUTE FUNCTION reject_audit_log_update();
