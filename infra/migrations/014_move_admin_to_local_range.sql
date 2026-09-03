-- 014_move_admin_to_local_range.sql
-- Design change: admin@agentraas.local belongs in the local range (1-9,
-- alongside personal/founder accounts) rather than the service range
-- (10-99). Also cleans up two stale test accounts from earlier development
-- testing (bcrypttest2@test.com, fastifytest@test.com) that were sitting in
-- the local range as noise.
--
-- Relies on migration 011's ON UPDATE CASCADE to safely carry any dependent
-- rows along with the id change.

DELETE FROM users WHERE email IN ('bcrypttest2@test.com', 'fastifytest@test.com');

DO $$
DECLARE
  admin_current_id INTEGER;
  next_local_id INTEGER;
BEGIN
  SELECT id INTO admin_current_id FROM users WHERE email = 'admin@agentraas.local';
  IF admin_current_id IS NOT NULL THEN
    SELECT MIN(candidate) INTO next_local_id
    FROM generate_series(1, 9) AS candidate
    WHERE candidate NOT IN (SELECT id FROM users WHERE id BETWEEN 1 AND 9);
    IF next_local_id IS NOT NULL AND admin_current_id != next_local_id THEN
      UPDATE users SET id = next_local_id WHERE id = admin_current_id;
      RAISE NOTICE 'Moved admin@agentraas.local to id=%', next_local_id;
    ELSE
      RAISE NOTICE 'No local range id available, or admin already in range — no change made';
    END IF;
  END IF;
END $$;
