-- New tracker bindings default to the `otto-tower` trigger label
-- (savvagent/otto-factory#233), matching the product's name.
--
-- Only the column default changes. Existing bindings keep the label they were
-- stored with, because customer automation may already apply it; no row is read
-- or rewritten here, so none of the per-org scoping a data migration needs
-- applies (see CLAUDE.md).
ALTER TABLE tracker_bindings ALTER COLUMN trigger_label SET DEFAULT 'otto-tower';
