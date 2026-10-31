/**
 * The console API's wire types.
 *
 * Hand-written, and mirroring `crates/tower-web/src/openapi.rs` rather than
 * generated from it. Generation would be the reflex; it is the wrong trade at
 * this size. A generator has to run in CI to be worth anything, and until
 * `tower-server` binds a port there is no document to fetch — so the "generated"
 * file would in practice be a checked-in artifact nobody regenerates, which is
 * the same hand-written file with a comment claiming otherwise.
 * `GET /api/openapi.json` is the authority either way; this is a transcription
 * of it, and `npm run check` fails when a page reads a field not declared here.
 *
 * Every name is camelCase because every response body is: `tower-web`'s structs
 * carry `#[serde(rename_all = "camelCase")]`.
 */

export type Role = 'owner' | 'admin' | 'member';
export type JobStatus = 'pending' | 'in-progress' | 'active' | 'completed' | 'failed' | 'cancelled';
export type Provider = 'github' | 'gitlab' | 'bitbucket' | 'other';

/** The org a console session is signed in to. A session opens exactly one. */
export interface Org {
  id: string;
  slug: string;
  name: string;
  plan: string;
}

/** The person signed in. The platform's record; the console keeps no copy. */
export interface SessionUser {
  id: string;
  email: string | null;
  name: string | null;
}

/**
 * `GET /api/session`: who is signed in to the console, in which org, and what
 * they may do there.
 *
 * `platformUrl` is where the Savvagent platform's own console lives — members, teams,
 * SSO, usage, tokens, the account, and the list of orgs are managed there, not
 * here (`$lib/platform`).
 */
export interface SessionInfo {
  user: SessionUser;
  org: Org;
  role: Role;
  scopes: string[];
  platformUrl: string;
}

export interface Repo {
  id: string;
  orgId: string;
  slug: string;
  name: string;
  provider: Provider;
  defaultBranch: string;
  teamId: string | null;
  defaultAgentType: string | null;
  active: boolean;
  createdAt: string;
  createdBy: string | null;
}

export interface RepoListItem extends Repo {
  /** Present only when `api.repos()` was called with `includeLeaseStatus: true`. */
  hasActiveLease?: boolean;
}

export type TrackerProvider = 'github' | 'jira';

/**
 * One org's connection to a tracker.
 *
 * There is no field for the stored credential and there is not meant to be:
 * the server returns `hasCredentials` and never the ciphertext itself.
 */
export interface TrackerConnection {
  id: string;
  provider: TrackerProvider;
  /** GitHub: the App installation id. JIRA: the cloud site id. */
  externalId: string;
  hasCredentials: boolean;
  createdAt: string;
  updatedAt: string;
}

/**
 * What this deployment can take an admin through, for one provider.
 *
 * Read at runtime rather than baked into the bundle: `startUrl` carries the
 * App slug or OAuth client id of whichever deployment served the page, and a
 * hard-coded one is how a staging console sends an admin to install the
 * production App.
 */
export interface ProviderSetup {
  configured: boolean;
  /** Where to send the browser to begin, minus its `state`. */
  startUrl: string | null;
}

export interface TrackerConnections {
  connections: TrackerConnection[];
  github: ProviderSetup;
  jira: ProviderSetup;
}

export interface TrackerBinding {
  id: string;
  repoId: string;
  provider: TrackerProvider;
  /** GitHub: `owner/repo`. JIRA: a project key. */
  externalRef: string;
  triggerLabel: string;
  /** False while the org has no connection for this provider. */
  live: boolean;
  createdAt: string;
  updatedAt: string;
}

/**
 * An advisory, time-bounded claim on one resource. The server cannot enforce
 * it against what an agent actually does with the resource, which it cannot
 * see; it makes collisions visible rather than impossible.
 */
export interface Lease {
  id: string;
  repoId: string;
  resource: string;
  holderUserId: string;
  holderLabel: string | null;
  jobId: string | null;
  acquiredAt: string;
  renewedAt: string;
  expiresAt: string;
}

export interface Job {
  id: string;
  orgId: string;
  repoId: string;
  teamId: string | null;
  title: string;
  description: string | null;
  status: JobStatus;
  ticketRef: string | null;
  tracker: 'jira' | 'github' | null;
  agentType: string | null;
  /** Opaque to Savvagent Tower. A customer's own skill owns the shape. */
  metadata: Record<string, unknown>;
  createdAt: string;
  startedAt: string | null;
  completedAt: string | null;
  attempts: number;
  result: string | null;
  error: string | null;
  createdBy: string | null;
  claimedBy: string | null;
  claimedByLabel: string | null;
  claimExpiresAt: string | null;
  cancelRequestedAt: string | null;
  cancelRequestedBy: string | null;
  cancelReason: string | null;
}

export interface JobDetail extends Job {
  dependsOn: string[];
}

/**
 * `blocked` overlaps `pending` rather than partitioning it: it counts the
 * pending jobs still waiting on a dependency. Two pending jobs where one cannot
 * start is not the same queue as two that can.
 */
export interface QueueStats {
  pending: number;
  inProgress: number;
  active: number;
  completed: number;
  failed: number;
  cancelled: number;
  blocked: number;
  total: number;
}

export interface AuditEvent {
  id: number;
  orgId: string | null;
  actorUserId: string | null;
  actorLabel: string | null;
  action: string;
  targetType: string | null;
  targetId: string | null;
  ip: string | null;
  userAgent: string | null;
  detail: Record<string, unknown>;
  createdAt: string;
}
