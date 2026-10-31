-- otto-factory domain baseline.
--
-- ONE-TIME RESET (savvagent/otto-factory#192, Phase 4 of the otto-platform cutover).
-- This file replaces migrations 0001-0034, which interleaved otto-factory's own
-- domain with identity, auth, and billing tables that now live in the platform's
-- database. Production data was disposable at the cutover, so the history is
-- retired rather than carried: this is the only time the migration directory has
-- been rewritten, and from here it is append-only again. A deployment that ran
-- the old history must be recreated from empty, not migrated; see
-- docs/deploy/fly.md.
--
-- What this database holds: repos, jobs, leases, messages, tracker connections,
-- and the factory's own bookkeeping (counters, usage outbox, audit trail). What
-- it does NOT hold: users, orgs, teams, memberships, tokens, sessions, plans, or
-- usage totals. Those belong to the platform, in another database, so nothing
-- here can have a foreign key to them. An `org_id`, `team_id`, or user id in this
-- schema is an opaque uuid the platform issued; the application checks it
-- against the platform where it matters (introspection for every request,
-- `PlatformClient::team` before a team id is written) and the platform's signed
-- lifecycle webhooks (`POST /platform/webhooks`) clean up after deletions.
--
-- Tenant isolation: every table carrying an org's rows has a NOT NULL `org_id`,
-- FORCE ROW LEVEL SECURITY, and a policy named `<table>_tenant_isolation`
-- (`Db::verify_tenant_isolation` discovers tenant tables by that name).

-- The role every tenant transaction runs as. NOLOGIN: it is never a connection
-- identity, only a `SET LOCAL ROLE` target. This is otto-tenant's default tenant
-- role name.
--
-- Roles are cluster-scoped while migrations are database-scoped, so creation is
-- idempotent and tolerant of two concurrent migrations racing to create it
-- (`#[sqlx::test]` migrates many throwaway databases in one cluster in
-- parallel). It also tolerates `insufficient_privilege`: managed Postgres does
-- not hand the application CREATEROLE. There, FORCE ROW LEVEL SECURITY below
-- carries the isolation guarantee and `Db::verify_tenant_isolation` proves it at
-- startup, refusing to serve otherwise.
DO $$
BEGIN
  BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'otto_app') THEN
      CREATE ROLE otto_app NOLOGIN;
    END IF;
  EXCEPTION
    WHEN duplicate_object OR unique_violation THEN NULL;
    WHEN insufficient_privilege THEN
      RAISE NOTICE
        'could not CREATE ROLE otto_app (no CREATEROLE). Tenant transactions will '
        'run as the connecting role and rely on FORCE ROW LEVEL SECURITY, which '
        'holds only while that role is neither a superuser nor BYPASSRLS. '
        'of-server verifies this at startup and refuses to serve otherwise.';
  END;
END $$;

-- The org pinned to the current transaction, or NULL when unset. NULL is the
-- important case: `org_id = NULL` is not TRUE, so a transaction that forgot to
-- pin an org sees zero rows rather than every row.
CREATE OR REPLACE FUNCTION current_org() RETURNS uuid AS $$
  SELECT NULLIF(current_setting('app.org_id', true), '')::uuid;
$$ LANGUAGE sql STABLE;

CREATE TYPE repo_provider AS ENUM ('github', 'gitlab', 'bitbucket', 'other');
CREATE TYPE job_status AS ENUM
  ('pending', 'in-progress', 'completed', 'failed', 'active', 'cancelled');
CREATE TYPE tracker AS ENUM ('jira', 'github');
CREATE TYPE tracker_provider AS ENUM ('github', 'jira');
CREATE TYPE message_kind AS ENUM ('note', 'request', 'response');
CREATE TYPE sender_kind AS ENUM ('agent', 'human');

-- ---------------------------------------------------------------- counters

-- Per-org counters that used to be columns on the platform-side `orgs` row.
-- Job ids are `job-N` scoped to the org, so two orgs both have a `job-1` and
-- neither can enumerate the other's. A row is created on first use (see
-- of-core::jobs); `next_job_seq` is bumped under that row's lock inside the
-- insert transaction. `jobs_completed_total`/`jobs_failed_total` are kept in
-- step with every transition into those states so org-wide stats do not scan
-- terminal history.
CREATE TABLE org_counters (
  org_id               uuid PRIMARY KEY,
  next_job_seq         bigint NOT NULL DEFAULT 1,
  jobs_completed_total bigint NOT NULL DEFAULT 0,
  jobs_failed_total    bigint NOT NULL DEFAULT 0
);

-- ------------------------------------------------------------------- repos

CREATE TABLE repos (
  id                 uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  org_id             uuid          NOT NULL,
  slug               text          NOT NULL,
  name               text          NOT NULL,
  provider           repo_provider NOT NULL DEFAULT 'other',
  default_branch     text          NOT NULL DEFAULT 'main',
  -- Optional owning team (a platform team id; no foreign key, the team lives in
  -- another database). When set, only that team's members and org admins see the
  -- repo and its jobs; when null the repo is org-wide.
  --
  -- A team id here that the platform no longer knows (the team was deleted) is
  -- NOT org-wide: it matches nobody's team list, so the repo stays visible only
  -- to admins until one reassigns it. See of-core::platform_events.
  team_id            uuid,
  -- Free-form hint only ('claude-code', 'copilot-cli', ...). Never enforced:
  -- otto-factory is agent-agnostic and must not privilege any client.
  default_agent_type text,
  tracker_binding    jsonb         NOT NULL DEFAULT '{}'::jsonb,
  active             boolean       NOT NULL DEFAULT true,
  created_at         timestamptz   NOT NULL DEFAULT now(),
  created_by         uuid,
  UNIQUE (org_id, slug)
);

CREATE INDEX repos_org_active_idx ON repos (org_id, active);
CREATE INDEX repos_team_idx ON repos (team_id) WHERE team_id IS NOT NULL;

-- Every remote form that identifies a repo, NORMALIZED (see
-- of-core::repos::normalize_remote). Unique per ORG, not globally: two orgs may
-- legitimately both work in the same public repo, and neither may learn about
-- the other by registering it.
CREATE TABLE repo_remotes (
  org_id     uuid        NOT NULL,
  repo_id    uuid        NOT NULL REFERENCES repos (id) ON DELETE CASCADE,
  normalized text        NOT NULL,
  created_at timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (org_id, normalized)
);

CREATE INDEX repo_remotes_repo_idx ON repo_remotes (repo_id);

-- Advisory, time-bounded leases on a resource within a repo (`branch:<name>` or
-- any other string a team needs to serialize on). The server cannot see git
-- operations, so a lease makes a collision visible and avoidable; it is not a
-- mutex. A crashed agent's lease expires rather than deadlocking the repo.
CREATE TABLE repo_leases (
  id             uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  org_id         uuid        NOT NULL,
  repo_id        uuid        NOT NULL REFERENCES repos (id) ON DELETE CASCADE,
  resource       text        NOT NULL,
  holder_user_id uuid        NOT NULL,
  holder_label   text,
  job_id         text,
  acquired_at    timestamptz NOT NULL DEFAULT now(),
  renewed_at     timestamptz NOT NULL DEFAULT now(),
  expires_at     timestamptz NOT NULL,
  released_at    timestamptz
);

-- At most one live lease per (repo, resource). Expiry is NOT in this predicate
-- (now() is not immutable): `acquire_lease` reaps expired rows in the same
-- transaction first, so this index is the correctness backstop and the reap is
-- the liveness path.
CREATE UNIQUE INDEX repo_leases_live_key
  ON repo_leases (repo_id, resource)
  WHERE released_at IS NULL;

CREATE INDEX repo_leases_org_expiry_idx ON repo_leases (org_id, expires_at)
  WHERE released_at IS NULL;

-- -------------------------------------------------------------------- jobs

CREATE TABLE jobs (
  -- `job-N`, unique per org, drawn from org_counters.next_job_seq.
  id              text        NOT NULL,
  org_id          uuid        NOT NULL,
  repo_id         uuid        NOT NULL REFERENCES repos (id) ON DELETE CASCADE,
  -- Denormalized from repos.team_id at insert time so team-scoped reads never
  -- need a join, and so moving a repo between teams does not silently
  -- re-classify historical work. Same dangling-id rule as repos.team_id.
  team_id         uuid,

  title           text        NOT NULL,
  description     text,
  status          job_status  NOT NULL DEFAULT 'pending',

  -- A JIRA key (RELMGT-3340) or a GitHub issue (owner/repo#123).
  ticket_ref      text,
  tracker         tracker,

  -- Free-form hint, never validated against a list.
  agent_type      text,

  -- Never interpreted by the server: where a customer's own skills keep
  -- whatever their methodology needs.
  metadata        jsonb       NOT NULL DEFAULT '{}'::jsonb,

  created_at      timestamptz NOT NULL DEFAULT now(),
  started_at      timestamptz,
  completed_at    timestamptz,
  attempts        integer     NOT NULL DEFAULT 0,
  result          text,
  error           text,

  created_by      uuid,
  claimed_by      uuid,
  claimed_by_label text,
  claim_expires_at timestamptz,

  -- Loop-safety state for tracker sync.
  remote_revision text,

  cancel_requested_at timestamptz,
  cancel_requested_by uuid,
  cancel_reason       text,

  -- Retry-safe creation: a repeated add_job with the same key returns the
  -- original job instead of creating a second.
  idempotency_key          text,
  idempotency_payload_hash bytea,
  CONSTRAINT jobs_idempotency_pair_ck
    CHECK ((idempotency_key IS NULL) = (idempotency_payload_hash IS NULL)),

  PRIMARY KEY (org_id, id)
);

CREATE INDEX jobs_org_status_idx ON jobs (org_id, status);
CREATE INDEX jobs_repo_status_idx ON jobs (repo_id, status);
CREATE INDEX jobs_org_status_agent_type_idx ON jobs (org_id, status, agent_type);
CREATE INDEX jobs_repo_status_agent_type_idx ON jobs (repo_id, status, agent_type);
CREATE INDEX jobs_org_ticket_idx ON jobs (org_id, ticket_ref) WHERE ticket_ref IS NOT NULL;
CREATE INDEX jobs_team_idx ON jobs (team_id) WHERE team_id IS NOT NULL;
CREATE UNIQUE INDEX jobs_org_idempotency_key_idx
  ON jobs (org_id, idempotency_key) WHERE idempotency_key IS NOT NULL;
-- One open job per (repo, tracker, ticket).
CREATE UNIQUE INDEX jobs_org_repo_tracker_ticket_open_idx
  ON jobs (org_id, repo_id, tracker, ticket_ref)
  WHERE ticket_ref IS NOT NULL AND status IN ('pending', 'in-progress', 'active');

CREATE TABLE job_dependencies (
  org_id     uuid NOT NULL,
  job_id     text NOT NULL,
  depends_on text NOT NULL,
  PRIMARY KEY (org_id, job_id, depends_on),
  FOREIGN KEY (org_id, job_id)     REFERENCES jobs (org_id, id) ON DELETE CASCADE,
  FOREIGN KEY (org_id, depends_on) REFERENCES jobs (org_id, id) ON DELETE CASCADE,
  CHECK (job_id <> depends_on)
);

CREATE INDEX job_dependencies_depends_idx ON job_dependencies (org_id, depends_on);

-- ---------------------------------------------------------------- messages

-- The shared agent-to-agent channel plus each member's read cursor. Coordination
-- chatter, not a payload transport.
CREATE TABLE messages (
  id               bigserial PRIMARY KEY,
  org_id           uuid         NOT NULL,
  created_at       timestamptz  NOT NULL DEFAULT now(),

  -- Set server-side from the authenticated principal; never client-supplied.
  sender_user_id   uuid         NOT NULL,
  sender_label     text,
  sender_kind      sender_kind  NOT NULL DEFAULT 'agent',

  -- NULL recipient is a broadcast to the whole org (or team, when team_id is set).
  recipient_user_id uuid,
  team_id          uuid,

  kind             message_kind NOT NULL DEFAULT 'note',
  body             text         NOT NULL,

  repo_id          uuid         REFERENCES repos (id) ON DELETE SET NULL,
  job_id           text,
  in_reply_to      bigint       REFERENCES messages (id) ON DELETE SET NULL,

  idempotency_key          text,
  idempotency_payload_hash bytea,
  CONSTRAINT messages_idempotency_pair_ck
    CHECK ((idempotency_key IS NULL) = (idempotency_payload_hash IS NULL)),

  FOREIGN KEY (org_id, job_id) REFERENCES jobs (org_id, id) ON DELETE SET NULL
);

CREATE INDEX messages_org_id_idx ON messages (org_id, id DESC);
CREATE INDEX messages_recipient_idx ON messages (org_id, recipient_user_id, id DESC)
  WHERE recipient_user_id IS NOT NULL;
CREATE UNIQUE INDEX messages_org_idempotency_key_idx
  ON messages (org_id, idempotency_key) WHERE idempotency_key IS NOT NULL;

-- Per-member read cursor. `unread` is "id > cursor AND not sent by me".
CREATE TABLE message_cursors (
  org_id       uuid        NOT NULL,
  user_id      uuid        NOT NULL,
  last_read_id bigint      NOT NULL DEFAULT 0,
  updated_at   timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (org_id, user_id)
);

-- Change notification. ONE channel for the whole server with org_id in the
-- payload, rather than a channel per org: the server LISTENs once and fans out
-- in process.
CREATE OR REPLACE FUNCTION notify_change() RETURNS trigger AS $$
DECLARE
  rec record;
BEGIN
  rec := COALESCE(NEW, OLD);
  PERFORM pg_notify(
    'of_changes',
    json_build_object(
      'kind',  TG_ARGV[0],
      'org',   rec.org_id,
      'id',    rec.id::text,
      'op',    TG_OP
    )::text
  );
  RETURN NULL;
END;
$$ LANGUAGE plpgsql;

-- The payload carries the sender so a caller's own send does not wake its own
-- long poll.
CREATE OR REPLACE FUNCTION notify_message() RETURNS trigger AS $$
BEGIN
  PERFORM pg_notify(
    'of_changes',
    json_build_object(
      'kind',   'message',
      'org',    NEW.org_id,
      'id',     NEW.id::text,
      'op',     TG_OP,
      'sender', NEW.sender_user_id
    )::text
  );
  RETURN NULL;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER jobs_notify
  AFTER INSERT OR UPDATE OR DELETE ON jobs
  FOR EACH ROW EXECUTE FUNCTION notify_change('job');
CREATE TRIGGER repo_leases_notify
  AFTER INSERT OR UPDATE OR DELETE ON repo_leases
  FOR EACH ROW EXECUTE FUNCTION notify_change('lease');
CREATE TRIGGER messages_notify
  AFTER INSERT ON messages
  FOR EACH ROW EXECUTE FUNCTION notify_message();

-- ---------------------------------------------------------------- trackers

CREATE TABLE tracker_connections (
  id                         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  org_id                     uuid             NOT NULL,
  provider                   tracker_provider NOT NULL,
  external_id                text             NOT NULL,
  encrypted_credentials      text,
  encrypted_webhook_secret   text,
  created_at                 timestamptz      NOT NULL DEFAULT now(),
  updated_at                 timestamptz      NOT NULL DEFAULT now(),
  UNIQUE (org_id, provider)
);

CREATE TABLE tracker_bindings (
  id                         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  org_id                     uuid             NOT NULL,
  repo_id                    uuid             NOT NULL REFERENCES repos (id) ON DELETE CASCADE,
  connection_id              uuid             REFERENCES tracker_connections (id) ON DELETE SET NULL,
  provider                   tracker_provider NOT NULL,
  external_ref               text             NOT NULL,
  trigger_label              text             NOT NULL DEFAULT 'otto-factory',
  created_at                 timestamptz      NOT NULL DEFAULT now(),
  updated_at                 timestamptz      NOT NULL DEFAULT now(),
  UNIQUE (repo_id, provider)
);

CREATE INDEX tracker_bindings_org_id_idx ON tracker_bindings (org_id);
CREATE INDEX tracker_bindings_connection_id_idx ON tracker_bindings (connection_id);

-- Global lookup from a webhook's (provider, external id) to its org, for the
-- unauthenticated webhook route, which has to resolve a tenant before it can
-- pin one. Deliberately NOT under row-level security: the route runs before any
-- org is known (the same reason authentication tables are exempt in the
-- platform). Rows carry no secret.
CREATE TABLE tracker_connection_index (
  provider      tracker_provider NOT NULL,
  external_id   text             NOT NULL,
  org_id        uuid             NOT NULL,
  connection_id uuid             NOT NULL REFERENCES tracker_connections (id) ON DELETE CASCADE,
  PRIMARY KEY (provider, external_id)
);

CREATE INDEX tracker_connection_index_org_id_idx ON tracker_connection_index (org_id);
CREATE INDEX tracker_connection_index_connection_id_idx ON tracker_connection_index (connection_id);

-- ------------------------------------------------------------ usage outbox

-- Metered calls awaiting delivery to the platform (savvagent/otto-factory#192,
-- workstream 3). A row is written in the SAME transaction as the tool's own work,
-- so a failed call is never billed and a successful one is recorded even if the
-- platform is down. A background shipper (of-billing::outbox) posts batches to
-- the platform's /internal/usage, which dedupes on `event_id`, and deletes rows
-- the platform has accounted for (accepted, duplicate, or rejected).
--
-- `event_id` is minted once at insert and reused on every retry: that is what
-- makes shipping idempotent. `next_attempt_at` is both the retry backoff and the
-- claim lease between replicas.
CREATE TABLE usage_outbox (
  id              bigserial   PRIMARY KEY,
  event_id        uuid        NOT NULL DEFAULT gen_random_uuid() UNIQUE,
  org_id          uuid        NOT NULL,
  user_id         uuid,
  tool            text        NOT NULL,
  billable        boolean     NOT NULL,
  occurred_at     timestamptz NOT NULL DEFAULT now(),
  attempts        integer     NOT NULL DEFAULT 0,
  next_attempt_at timestamptz NOT NULL DEFAULT now(),
  last_error      text
);

CREATE INDEX usage_outbox_due_idx ON usage_outbox (next_attempt_at, id);
CREATE INDEX usage_outbox_org_idx ON usage_outbox (org_id);

-- ------------------------------------------------------ platform event log

-- Lifecycle webhook deliveries already applied, so a redelivery (the platform is
-- at-least-once) is a no-op. Written in the same pinned transaction as the
-- cleanup it records. Rows outlive the org they describe on purpose: an
-- `org.deleted` purge keeps its own marker, or a replay would find nothing to
-- dedupe against.
CREATE TABLE platform_events (
  event_id    uuid PRIMARY KEY,
  org_id      uuid        NOT NULL,
  kind        text        NOT NULL,
  received_at timestamptz NOT NULL DEFAULT now()
);

-- Usage the platform refused outright. Never dropped silently: a rejected event
-- is moved here, with the platform's reason, in the same transaction that
-- removes it from the outbox, so an operator can see what was refused and why.
CREATE TABLE usage_outbox_rejected (
  id          bigserial   PRIMARY KEY,
  event_id    uuid        NOT NULL,
  org_id      uuid        NOT NULL,
  user_id     uuid,
  tool        text        NOT NULL,
  billable    boolean     NOT NULL,
  occurred_at timestamptz NOT NULL,
  reason      text        NOT NULL,
  rejected_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX usage_outbox_rejected_org_idx ON usage_outbox_rejected (org_id, rejected_at DESC);

-- -------------------------------------------------------------- tombstones

-- Orgs the platform has deleted. Written FIRST by the org.deleted handler,
-- before the purge, and checked on every authenticated request: the platform's
-- token cache (60 s) and in-flight transactions would otherwise let writes
-- land after the purge, in rows nobody would ever clean up. Permanent (the
-- platform never reuses an org id). Deliberately not under row-level security:
-- authentication has to consult it before any org is pinned.
CREATE TABLE deleted_orgs (
  org_id     uuid PRIMARY KEY,
  deleted_at timestamptz NOT NULL DEFAULT now()
);

-- Members the platform has just removed. Short-lived: it only has to cover the
-- window in which a cached introspection still vouches for the user (60 s), after
-- which the platform itself answers "inactive". Swept after a few minutes, so a
-- member who is re-added is not locked out. Not under row-level security, for
-- the same reason as deleted_orgs.
CREATE TABLE removed_members (
  org_id     uuid        NOT NULL,
  user_id    uuid        NOT NULL,
  removed_at timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (org_id, user_id)
);

-- --------------------------------------------------------------- audit trail

-- The factory's own domain audit trail (REPO_*, TRACKER_*, JOB_*), written with
-- otto-tenant's `Tx::audit` in the same transaction as the change. Identity and
-- auth events are the platform's. Same shape as the platform's table so the
-- otto-tenant API works unchanged.
CREATE TABLE audit_events (
  id            bigserial PRIMARY KEY,
  -- Nullable for symmetry with the platform's table; every row the factory
  -- writes is org-scoped.
  org_id        uuid,
  actor_user_id uuid,
  actor_label   text,
  action        text        NOT NULL,
  target_type   text,
  target_id     text,
  ip            text,
  user_agent    text,
  detail        jsonb       NOT NULL DEFAULT '{}'::jsonb,
  created_at    timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX audit_events_org_time_idx ON audit_events (org_id, created_at DESC);
CREATE INDEX audit_events_actor_idx ON audit_events (actor_user_id, created_at DESC);
CREATE INDEX audit_events_action_idx ON audit_events (action, created_at DESC);

-- ----------------------------------------------------------- row-level security

DO $$
DECLARE
  t text;
  tenant_tables text[] := ARRAY[
    'org_counters',
    'repos',
    'repo_remotes',
    'repo_leases',
    'jobs',
    'job_dependencies',
    'messages',
    'message_cursors',
    'tracker_connections',
    'tracker_bindings',
    'platform_events'
  ];
BEGIN
  FOREACH t IN ARRAY tenant_tables LOOP
    EXECUTE format('ALTER TABLE %I ENABLE ROW LEVEL SECURITY', t);
    -- FORCE covers the case where the application role IS the table owner.
    EXECUTE format('ALTER TABLE %I FORCE ROW LEVEL SECURITY', t);
    EXECUTE format(
      'CREATE POLICY %I ON %I USING (org_id = current_org()) WITH CHECK (org_id = current_org())',
      t || '_tenant_isolation', t
    );
  END LOOP;
END $$;

-- The outbox is written by pinned transactions (a tool's own) and drained by the
-- shipper, which has no org: it ships every org's rows. So its policy has two
-- branches. A pinned transaction sees and writes only its own org's rows; an
-- unpinned one (current_org() IS NULL, which only background code is) sees all.
ALTER TABLE usage_outbox ENABLE ROW LEVEL SECURITY;
ALTER TABLE usage_outbox FORCE ROW LEVEL SECURITY;
CREATE POLICY usage_outbox_tenant_isolation ON usage_outbox
  USING (org_id = current_org() OR current_org() IS NULL)
  WITH CHECK (org_id = current_org() OR current_org() IS NULL);

ALTER TABLE usage_outbox_rejected ENABLE ROW LEVEL SECURITY;
ALTER TABLE usage_outbox_rejected FORCE ROW LEVEL SECURITY;
CREATE POLICY usage_outbox_rejected_tenant_isolation ON usage_outbox_rejected
  USING (org_id = current_org() OR current_org() IS NULL)
  WITH CHECK (org_id = current_org() OR current_org() IS NULL);

ALTER TABLE audit_events ENABLE ROW LEVEL SECURITY;
ALTER TABLE audit_events FORCE ROW LEVEL SECURITY;
CREATE POLICY audit_events_tenant_isolation ON audit_events
  FOR SELECT USING (org_id = current_org());
CREATE POLICY audit_events_append ON audit_events
  FOR INSERT WITH CHECK (current_org() IS NULL OR org_id = current_org());
-- Append-only, expressed as the deliberate ABSENCE of an UPDATE policy: under
-- FORCE that binds the table's owner too. DELETE is reachable only unpinned (a
-- retention job), never from a request.
CREATE POLICY audit_events_retention ON audit_events
  FOR DELETE USING (current_org() IS NULL);

-- --------------------------------------------------------------------- grants

DO $$
BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'otto_app') THEN
    -- `SET LOCAL ROLE otto_app` requires the connecting role to be a member.
    BEGIN
      EXECUTE 'GRANT otto_app TO CURRENT_USER';
    EXCEPTION WHEN insufficient_privilege THEN
      RAISE NOTICE
        'otto_app exists but could not be granted to the migrating role. Tenant '
        'transactions will fall back to FORCE ROW LEVEL SECURITY; of-server '
        'verifies that at startup.';
    END;

    EXECUTE 'GRANT USAGE ON SCHEMA public TO otto_app';
    EXECUTE 'GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO otto_app';
    EXECUTE 'GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA public TO otto_app';
    EXECUTE 'GRANT EXECUTE ON ALL FUNCTIONS IN SCHEMA public TO otto_app';
    EXECUTE 'ALTER DEFAULT PRIVILEGES IN SCHEMA public '
            'GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO otto_app';
    EXECUTE 'ALTER DEFAULT PRIVILEGES IN SCHEMA public '
            'GRANT USAGE, SELECT ON SEQUENCES TO otto_app';

    -- A grant is not a protection on managed Postgres (no otto_app there), but
    -- where the role exists it should not be able to edit the audit trail.
    EXECUTE 'REVOKE UPDATE, DELETE ON audit_events FROM otto_app';
  END IF;
END $$;
