-- Tracker webhook deliveries already applied, so a repeat is acknowledged and
-- does nothing (savvagent/otto-factory#265).
--
-- GitHub's `X-Hub-Signature-256` is an HMAC over the body alone, with no
-- timestamp, so a captured signed delivery stays valid until the webhook secret
-- is rotated. A row is written in the same pinned transaction as the delivery's
-- effects (as `platform_events` is for the platform's webhooks): a delivery
-- that fails rolls its row back, and a manual redelivery of it (GitHub does not
-- retry on its own) still applies. An event that matched no binding or was
-- dropped still commits its row: it was judged, and is not judged again.
--
-- The key is `digest`, SHA-256 of the signed body. A replay must send the same
-- body bytes, because the signature covers them, so this is what stops one.
-- No unsigned header is part of the key: `X-GitHub-Event` is checked against
-- the body's shape before anything is recorded (a mismatch is rejected), so a
-- signed body is applied under one event only. `delivery_id`
-- (`X-GitHub-Delivery`, NULL when absent) is kept for diagnosis and is NOT a
-- key: it is unsigned, so as one it would let any request mark a genuine
-- delivery's id as seen. GitHub's own redelivery resends the same body, which
-- the digest already catches.
--
-- Rows are kept for a retention window and then swept by `of-server` with no
-- org pinned, so the policy has the two branches `platform_events` has
-- (0002_org_less_retention.sql): a pinned transaction sees only its own org;
-- an unpinned one, which only background code is, sees every org's rows.
--
-- Schema only: no existing row is read or rewritten.

CREATE TABLE tracker_deliveries (
  org_id      uuid             NOT NULL,
  provider    tracker_provider NOT NULL,
  digest      bytea            NOT NULL CHECK (octet_length(digest) = 32),
  delivery_id text             CHECK (delivery_id IS NULL OR length(delivery_id) BETWEEN 1 AND 128),
  received_at timestamptz      NOT NULL DEFAULT now(),
  PRIMARY KEY (org_id, provider, digest)
);

-- The sweep.
CREATE INDEX tracker_deliveries_received_idx ON tracker_deliveries (received_at);

ALTER TABLE tracker_deliveries ENABLE ROW LEVEL SECURITY;
ALTER TABLE tracker_deliveries FORCE ROW LEVEL SECURITY;
CREATE POLICY tracker_deliveries_tenant_isolation ON tracker_deliveries
  USING (org_id = current_org() OR current_org() IS NULL)
  WITH CHECK (org_id = current_org() OR current_org() IS NULL);

DO $$
BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'otto_app') THEN
    EXECUTE 'GRANT SELECT, INSERT, DELETE ON tracker_deliveries TO otto_app';
  END IF;
END $$;
