-- Admit a `tracker_deliveries` row only from a transaction pinned to its org
-- (savvagent/otto-factory#273).
--
-- 0010 gave the table one policy for every command, with the two branches
-- `platform_events` has, so an unpinned statement could insert a row for any
-- org. Only the retention sweep needs the `IS NULL` branch, and it never
-- inserts: a delivery is recorded in the webhook's own pinned transaction. So
-- the policy is split by command, and only reads and deletes keep the branch.
--
-- The SELECT policy keeps the name `tracker_deliveries_tenant_isolation`:
-- `Db::verify_tenant_isolation` discovers tenant tables by a policy of exactly
-- that name, so renaming it would quietly drop the table from the startup
-- check. The sweep still needs it, not just its DELETE policy: a DELETE's WHERE
-- reads the rows it filters, so Postgres applies the table's SELECT policies to
-- it as well (as 0002_org_less_retention.sql found for `audit_events`).
--
-- There is still no UPDATE policy: a recorded delivery is never rewritten.
--
-- Schema only: no existing row is read or rewritten.

DROP POLICY tracker_deliveries_tenant_isolation ON tracker_deliveries;
CREATE POLICY tracker_deliveries_tenant_isolation ON tracker_deliveries
  FOR SELECT USING (org_id = current_org() OR current_org() IS NULL);
CREATE POLICY tracker_deliveries_insert ON tracker_deliveries
  FOR INSERT WITH CHECK (org_id = current_org());
CREATE POLICY tracker_deliveries_sweep ON tracker_deliveries
  FOR DELETE USING (org_id = current_org() OR current_org() IS NULL);
