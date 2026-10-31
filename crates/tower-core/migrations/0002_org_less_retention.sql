-- Let the org-less housekeeping in `of_core::platform_events` actually delete.
--
-- `sweep` forgets old `platform_events` markers and `org_deleted` purges a
-- deleted org's `audit_events`, both on the pool with no org pinned. Where the
-- connecting role bypasses row-level security that already works; where it does
-- not (FORCE, or a non-owner role), both statements matched zero rows without
-- an error. A DELETE's WHERE clause reads the rows it filters, so Postgres
-- applies the table's SELECT policies to it as well as its DELETE policies:
-- `audit_events_retention` (DELETE only, `current_org() IS NULL`) alone could
-- never match a row whose SELECT policy demands `org_id = current_org()`.
--
-- Both tables get the shape `usage_outbox` already has: a pinned transaction
-- still sees only its own org; an unpinned one, which only background code is,
-- sees every org's rows. Policy names are unchanged where they exist, so
-- `Db::verify_tenant_isolation` still finds the tenant policy by convention.

DROP POLICY platform_events_tenant_isolation ON platform_events;
CREATE POLICY platform_events_tenant_isolation ON platform_events
  USING (org_id = current_org() OR current_org() IS NULL)
  WITH CHECK (org_id = current_org() OR current_org() IS NULL);

-- Read access for the unpinned retention DELETE only; still no UPDATE policy,
-- so the trail stays append-only to everyone, its owner included.
CREATE POLICY audit_events_retention_read ON audit_events
  FOR SELECT USING (current_org() IS NULL);
