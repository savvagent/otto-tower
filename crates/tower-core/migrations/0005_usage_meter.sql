-- A usage row can carry a value meter (savvagent/otto-factory#222).
--
-- Until now every row was a per-call record: the tool, whether it was billable.
-- The platform (otto-platform v0.10.0) also accepts per-service value meters, a
-- named counter with a quantity, so the factory can bill a unit of outcome (a
-- completed job) rather than only calls. A row is either a plain call
-- (`meter` and `quantity` both NULL) or a metered one (both set); a meter with
-- no quantity, or a quantity with no meter, would ship a half-formed event, and
-- a quantity must be a positive count.
--
-- Additive only: existing rows keep both columns NULL and ship exactly as before.
-- RLS is unchanged; no new table. The rejected table gets the same columns so a
-- refused metered event is kept whole, not reduced to a plain call.

ALTER TABLE usage_outbox
  ADD COLUMN meter    text,
  ADD COLUMN quantity bigint,
  ADD CONSTRAINT usage_outbox_meter_quantity_together
    CHECK ((meter IS NULL) = (quantity IS NULL)),
  ADD CONSTRAINT usage_outbox_quantity_positive
    CHECK (quantity IS NULL OR quantity > 0);

ALTER TABLE usage_outbox_rejected
  ADD COLUMN meter    text,
  ADD COLUMN quantity bigint,
  ADD CONSTRAINT usage_outbox_rejected_meter_quantity_together
    CHECK ((meter IS NULL) = (quantity IS NULL)),
  ADD CONSTRAINT usage_outbox_rejected_quantity_positive
    CHECK (quantity IS NULL OR quantity > 0);
