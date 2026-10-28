-- A job's description can drift from its linked issue: a comment or edit on
-- the issue after the job was written is something an agent working the job
-- has not seen (savvagent/otto-factory#257). Two timestamps let the read
-- compute `ticketStale` without the server interpreting either text.
--
-- Schema only: both columns start NULL for every existing row, which reads as
-- "no foreign change seen" and "described when created", so no row is read
-- or rewritten here and none of the per-org scoping a data migration needs
-- applies (see CLAUDE.md).
ALTER TABLE jobs
  ADD COLUMN ticket_changed_at timestamptz,
  ADD COLUMN described_at      timestamptz;

COMMENT ON COLUMN jobs.ticket_changed_at IS
  'Newest change to the linked issue that otto-tower did not make itself (a human comment, or a pull that found a newer revision). NULL = none seen.';
COMMENT ON COLUMN jobs.described_at IS
  'When title/description were last written (update_job, or a webhook issue edit). NULL = created_at. Never exposed; ticket_stale compares against it.';
