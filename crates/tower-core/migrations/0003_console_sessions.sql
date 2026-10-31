-- The console's own login sessions (console login: OAuth authorization code +
-- PKCE against the otto platform, savvagent/otto-factory#192).
--
-- A row is the factory's half of a sign-in: the platform token pair the console
-- holds on a browser's behalf, kept server-side so the browser never sees it.
-- The cookie is a random 32-byte value that is never stored; `id_hash` is its
-- SHA-256, so a database dump cannot be replayed as a cookie. The tokens are
-- sealed with the deployment's encryption key (`base64(nonce || ciphertext)`,
-- the same encoding as tracker secrets), so a dump alone yields no credential.
--
-- `user_id` and `org_id` are the platform's opaque ids (no foreign key: they live
-- in another database). A token is bound to exactly one org, so a session is too.
--
-- `expires_at` is the session's deadline: when the refresh token it holds stops
-- working. Past it the row is dead weight and is purged.
--
-- Tenancy: `org_id` is NOT NULL and the table is FORCE ROW LEVEL SECURITY, but
-- the policy has the same two branches as `usage_outbox`. A request arrives with a
-- cookie and no org, and the cookie is how the org is learned, so the lookup is
-- necessarily unpinned. A pinned transaction (the refresh, the lifecycle
-- webhooks) sees only its own org's sessions.

CREATE TABLE console_sessions (
  id_hash           bytea       PRIMARY KEY CHECK (octet_length(id_hash) = 32),
  user_id           uuid        NOT NULL,
  org_id            uuid        NOT NULL,
  access_token_enc  text        NOT NULL,
  refresh_token_enc text        NOT NULL,
  access_expires_at timestamptz NOT NULL,
  scopes            text[]      NOT NULL DEFAULT '{}',
  created_at        timestamptz NOT NULL DEFAULT now(),
  last_used_at      timestamptz NOT NULL DEFAULT now(),
  expires_at        timestamptz NOT NULL
);

-- "Every session of this user in this org" (member.removed) and "every session
-- of this org" (org.deleted).
CREATE INDEX console_sessions_org_user_idx ON console_sessions (org_id, user_id);
-- The purge.
CREATE INDEX console_sessions_expires_idx ON console_sessions (expires_at);

ALTER TABLE console_sessions ENABLE ROW LEVEL SECURITY;
ALTER TABLE console_sessions FORCE ROW LEVEL SECURITY;
CREATE POLICY console_sessions_tenant_isolation ON console_sessions
  USING (org_id = current_org() OR current_org() IS NULL)
  WITH CHECK (org_id = current_org() OR current_org() IS NULL);

DO $$
BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'otto_app') THEN
    EXECUTE 'GRANT SELECT, INSERT, UPDATE, DELETE ON console_sessions TO otto_app';
  END IF;
END $$;
