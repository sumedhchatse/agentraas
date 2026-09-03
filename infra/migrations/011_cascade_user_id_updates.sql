-- 012_cascade_user_id_updates.sql
-- Adds ON UPDATE CASCADE to every foreign key that references users(id).
-- Without this, reassigning a user's id (as 011 tries to do for service
-- accounts) fails the moment ANY table has a row referencing that user —
-- and as it turns out, email_verification_tokens always does, for every
-- account, immediately after registration. This was missed when writing
-- 011's manual dependent-table checks (api_keys, custom_actions,
-- service_credentials — forgetting the two token tables).
--
-- Rather than keep hand-checking every table (and risk missing another one
-- later), this dynamically finds every FK referencing users(id) via
-- Postgres's own constraint catalog and adds ON UPDATE CASCADE to each —
-- correct now, and automatically correct for any table added in the future.

DO $$
DECLARE
  rec RECORD;
  new_def TEXT;
BEGIN
  FOR rec IN
    SELECT c.conname, c.conrelid::regclass AS table_name, pg_get_constraintdef(c.oid) AS def
    FROM pg_constraint c
    WHERE c.confrelid = 'users'::regclass AND c.contype = 'f'
  LOOP
    IF rec.def NOT LIKE '%ON UPDATE CASCADE%' THEN
      EXECUTE format('ALTER TABLE %s DROP CONSTRAINT %I', rec.table_name, rec.conname);
      new_def := rec.def;
      IF new_def LIKE '%ON DELETE CASCADE%' THEN
        new_def := replace(new_def, 'ON DELETE CASCADE', 'ON UPDATE CASCADE ON DELETE CASCADE');
      ELSE
        new_def := new_def || ' ON UPDATE CASCADE';
      END IF;
      EXECUTE format('ALTER TABLE %s ADD CONSTRAINT %I %s', rec.table_name, rec.conname, new_def);
      RAISE NOTICE 'Added ON UPDATE CASCADE to %.%', rec.table_name, rec.conname;
    END IF;
  END LOOP;
END $$;
