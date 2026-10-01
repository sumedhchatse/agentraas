-- Destinations (email address, phone, account id, URL host) a human has
-- approved for an org + service, for action policies' `new_destination`
-- rule (action_policies.rs). A value not listed here needs
-- approval once; approving it inserts it (ee/hitl.rs approve path).
CREATE TABLE IF NOT EXISTS known_destinations (
  org_id     VARCHAR(255) NOT NULL,
  service    VARCHAR(255) NOT NULL,
  field      VARCHAR(255) NOT NULL,
  value      TEXT NOT NULL,
  first_seen TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  PRIMARY KEY (org_id, service, field, value)
);
