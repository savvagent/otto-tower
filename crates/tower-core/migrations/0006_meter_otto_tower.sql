-- The billing meter is renamed with the product (savvagent/otto-factory#229).
--
-- The service the platform bills against is now `otto-tower`, so the value meter
-- a completed job is metered under becomes `otto-tower/jobs_completed`. Rows
-- still waiting in the outbox were stamped with the old key at enqueue time;
-- shipped after the rename they would be refused as an undeclared meter, so
-- they are rewritten here to the key the shipper will declare.
--
-- `usage_outbox_rejected` is deliberately left alone: those rows are history of
-- what was refused, under the key they were sent with, and nothing re-ships them.
--
-- Migrations run on the raw pool (no `app.org_id`, no `otto_app`), and
-- `usage_outbox`'s policy is `org_id = current_org() OR current_org() IS NULL`.
-- Rather than depend on which shape this runs under, follow the per-org pattern
-- from CLAUDE.md: an explicit `org_id` predicate (covers a role that bypasses
-- RLS) and `set_config('app.org_id', ...)` (covers a role RLS binds).
-- Idempotent: a second run finds no rows still on the old key.
DO $$
DECLARE
  o uuid;
BEGIN
  FOR o IN SELECT DISTINCT org_id FROM usage_outbox WHERE meter = 'otto-factory/jobs_completed' LOOP
    PERFORM set_config('app.org_id', o::text, true);
    UPDATE usage_outbox SET meter = 'otto-tower/jobs_completed'
      WHERE org_id = o AND meter = 'otto-factory/jobs_completed';
  END LOOP;
  PERFORM set_config('app.org_id', '', true);
END $$;
