-- A message keeps the team scope it was sent under (savvagent/otto-factory#192).
--
-- Who may read a message is decided by its team, its repo's team, and its job's
-- (and that job's repo's) team. The repo and job links are `ON DELETE SET NULL`,
-- so a message only ever *looked* scoped while they lasted: deleting a team's
-- repo turned its messages org-wide, readable by every member. `scope_teams` is
-- the set of every team the message was bound to when it was sent, including
-- the thread it replies to, and it never changes. A reader must be on all of
-- them, whatever has been deleted since. (The live repo and job checks still
-- apply as well, so a repo moved to another team after the fact also hides its
-- messages; the snapshot only ever narrows.)
--
-- The job link also never worked as written: `FOREIGN KEY (org_id, job_id) ...
-- ON DELETE SET NULL` nulls *both* columns, `org_id` is NOT NULL, and so
-- deleting any job that had a message failed outright. Postgres 15's column
-- list nulls only `job_id`.

ALTER TABLE messages ADD COLUMN scope_teams uuid[] NOT NULL DEFAULT '{}';

ALTER TABLE messages DROP CONSTRAINT messages_org_id_job_id_fkey;
ALTER TABLE messages
  ADD CONSTRAINT messages_org_id_job_id_fkey
  FOREIGN KEY (org_id, job_id) REFERENCES jobs (org_id, id) ON DELETE SET NULL (job_id);

-- Backfill existing rows from what is still linked. Per org, with both the
-- explicit predicate and `app.org_id` (see CLAUDE.md).
--
-- Finding the orgs is the one read that cannot be pinned to an org. Where the
-- migrating role is subject to row-level security (FORCE binds the owner),
-- `messages_tenant_isolation` hides every row from an unpinned read, so the
-- loop would find nothing and silently skip the backfill. This read-only policy
-- lets an unpinned read see the rows for the length of this migration, the
-- same branch `audit_events_retention_read` gives the retention sweep. It is
-- created and dropped inside this migration's own transaction, so no other
-- session ever sees it, and it grants no write.
CREATE POLICY messages_backfill_0004 ON messages FOR SELECT USING (current_org() IS NULL);

DO $$
DECLARE
  o uuid;
  n bigint;
BEGIN
  FOR o IN SELECT DISTINCT org_id FROM messages LOOP
    PERFORM set_config('app.org_id', o::text, true);

    UPDATE messages m SET scope_teams = ARRAY(
      SELECT DISTINCT t FROM unnest(ARRAY[
        m.team_id,
        (SELECT r.team_id FROM repos r WHERE r.org_id = m.org_id AND r.id = m.repo_id),
        (SELECT j.team_id FROM jobs j WHERE j.org_id = m.org_id AND j.id = m.job_id),
        (SELECT r.team_id FROM jobs j JOIN repos r ON r.org_id = j.org_id AND r.id = j.repo_id
          WHERE j.org_id = m.org_id AND j.id = m.job_id)
      ]) AS t WHERE t IS NOT NULL)
    WHERE m.org_id = o;

    -- A reply inherits its thread's scope. One level per pass, until nothing
    -- changes: bounded by the deepest thread.
    LOOP
      UPDATE messages m
         SET scope_teams = ARRAY(SELECT DISTINCT unnest(m.scope_teams || p.scope_teams))
        FROM messages p
       WHERE m.org_id = o AND p.org_id = o AND p.id = m.in_reply_to
         AND NOT p.scope_teams <@ m.scope_teams;
      GET DIAGNOSTICS n = ROW_COUNT;
      EXIT WHEN n = 0;
    END LOOP;
  END LOOP;
  PERFORM set_config('app.org_id', '', true);
END $$;

DROP POLICY messages_backfill_0004 ON messages;
