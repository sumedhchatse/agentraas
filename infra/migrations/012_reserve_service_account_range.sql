-- 012_reserve_service_account_range.sql
-- Establishes three id ranges by convention:
--   1-9    local/founder personal accounts (e.g. your own login)
--   10-99  service/system accounts (e.g. admin@agentraas.local, created via
--          bootstrap-admin.sh)
--   100+   external users — everyone who registers normally through the app
--
-- Moves admin@agentraas.local into the service range (id=10) if that id
-- isn't already taken. Relies on migration 011 (ON UPDATE CASCADE on every
-- FK referencing users) to safely carry any dependent rows along — 011
-- runs before this one, by design of the numbering.
--
-- Also sets the id sequence so all FUTURE normal registrations start at
-- id=100 — this is what actually keeps external users out of the 1-99
-- range going forward. The 1-9 and 10-99 ranges only ever get populated by
-- deliberate, explicit id assignment (see bootstrap-admin.sh), not by the
-- normal auto-incrementing registration flow.

DO $$
DECLARE
  admin_current_id INTEGER;
  id_10_taken BOOLEAN;
BEGIN
  SELECT id INTO admin_current_id FROM users WHERE email = 'admin@agentraas.local';

  IF admin_current_id IS NOT NULL AND admin_current_id != 10 THEN
    SELECT EXISTS(SELECT 1 FROM users WHERE id = 10) INTO id_10_taken;
    IF NOT id_10_taken THEN
      UPDATE users SET id = 10 WHERE email = 'admin@agentraas.local';
      RAISE NOTICE 'Reassigned admin@agentraas.local to id=10 (service account range)';
    ELSE
      RAISE NOTICE 'Skipped id=10 reassignment for admin@agentraas.local (id=10 already taken)';
    END IF;
  END IF;
END $$;

-- Ensure all future normal registrations start at id >= 100, regardless of
-- whether the reassignment above ran.
SELECT setval('users_id_seq', GREATEST(100, (SELECT COALESCE(MAX(id), 0) + 1 FROM users)), false);
