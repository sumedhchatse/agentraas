-- 025_agency_tier.sql
-- Agency tier mechanics (strategy doc's $149/mo tier: up to 10 client
-- tenants, white-label dashboard, outbound rate smoothing). This migration
-- only adds what the tier needs structurally - white-label branding per
-- org. The plan value itself ('agency', alongside the existing 'free'/
-- 'pro') needs no schema change, since users.plan is already a plain
-- VARCHAR(50) with no CHECK constraint (see PLAN_MONTHLY_LIMITS/
-- PLAN_RATE_LIMITS in server.js for where 'agency' actually gets
-- recognized). Real Paddle product/price wiring for this tier is
-- deliberately NOT part of this migration or the code that uses it -
-- that's a real billing decision for a real Paddle account, out of scope
-- here (see server.js's AGENCY_MAX_CLIENT_TENANTS comment).

CREATE TABLE IF NOT EXISTS org_branding (
  org_id       VARCHAR(255) PRIMARY KEY,
  display_name VARCHAR(255),
  logo_url     TEXT,
  updated_at   TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
-- No FK to orgs(org_id) on purpose - white-labeling doesn't require an org
-- to have ever touched Enterprise SSO (orgs only gets a row then). Any
-- agency-owned org_id can have branding, same "org_id VARCHAR(255) is the
-- real key, no other table required" shape as org_limit_overrides.
