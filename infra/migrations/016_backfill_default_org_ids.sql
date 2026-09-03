-- 016_backfill_default_org_ids.sql
-- The registration endpoint now auto-generates a default org_id for every
-- new user, but that only applies going forward. Backfill existing users
-- who registered before this change and still have org_id = NULL.

UPDATE users
SET org_id = 'org_' || substr(md5(random()::text || id::text), 1, 12)
WHERE org_id IS NULL;
