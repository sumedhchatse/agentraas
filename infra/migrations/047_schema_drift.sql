-- Upstream contract/schema drift detection (SPEC-SCHEMA-DRIFT.md).
-- Shared per service.action, not per-org - same precedent the circuit
-- breaker already established (one shared state per service, not one
-- per org calling it), since API response shape is a property of the
-- upstream, not the caller. See crates/core/src/schema_drift.rs for the
-- fingerprinting logic and crates/api/src/schema_drift.rs for how these
-- tables get read/written.
CREATE TABLE IF NOT EXISTS schema_baselines (
  service         VARCHAR(255) NOT NULL,
  action          VARCHAR(255) NOT NULL,
  field_paths     JSONB NOT NULL,
  sample_count    BIGINT NOT NULL DEFAULT 1,
  first_seen_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  last_updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  PRIMARY KEY (service, action)
);

-- org_id is the org whose request happened to detect the drift (v1 only
-- notifies that org, not every org calling this service.action - see
-- SPEC-SCHEMA-DRIFT.md §2.4's noted follow-up).
CREATE TABLE IF NOT EXISTS schema_drift_events (
  id                   SERIAL PRIMARY KEY,
  service              VARCHAR(255) NOT NULL,
  action               VARCHAR(255) NOT NULL,
  org_id               VARCHAR(255) NOT NULL,
  req_id               VARCHAR(255) NOT NULL,
  removed_fields       JSONB NOT NULL DEFAULT '[]',
  type_changed_fields  JSONB NOT NULL DEFAULT '[]',
  detected_at          TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS idx_schema_drift_events_lookup ON schema_drift_events (service, action, detected_at DESC);
CREATE INDEX IF NOT EXISTS idx_schema_drift_events_org ON schema_drift_events (org_id, detected_at DESC);
