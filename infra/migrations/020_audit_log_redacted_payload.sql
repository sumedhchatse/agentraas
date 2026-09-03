-- 020_audit_log_redacted_payload.sql
-- Purely additive - adds a new nullable column, doesn't touch the existing
-- payload_hash column or any current audit_log behavior. Existing dedup
-- logic (which relies on payload_hash) is completely unaffected.
--
-- Currently audit_log only stores a hash of each payload - useful for
-- dedup, but means there's no way to actually review what happened for
-- debugging or compliance purposes (a hash can't be read). This column
-- gives Enterprise-tier orgs a genuinely reviewable audit trail - the
-- payload with PII redacted (credit cards, SSNs, common API key formats -
-- see src/ee/dlp), safe to look at, export, or hand to an auditor.
--
-- Nullable and optional: only populated when DLP redaction is actually
-- enabled for an org. Free-tier/Community orgs see NULL here, same as
-- today's behavior - this feature doesn't change anything for them.

ALTER TABLE audit_log ADD COLUMN IF NOT EXISTS redacted_payload_preview TEXT;
