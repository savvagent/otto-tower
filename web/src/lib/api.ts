/**
 * The one place the console talks to `of-web`.
 *
 * Three things hold here, and each of them is a rule the server also keeps.
 *
 * **No credential is ever spent on a GET.** A single-use redemption (the tracker
 * callback's authorization code) is a `POST`: a mail scanner or link preview that
 * follows a URL loads a page and burns nothing.
 *
 * **The session is never touched by script.** It is an `HttpOnly`,
 * `__Host-`-prefixed cookie that `of-web` sets when the sign-in through the
 * Savvagent platform finishes (`/auth/login` → `/auth/callback`), so there is no token
 * to attach and nothing to store. Requests are same-origin and the browser sends
 * it; `credentials` is left at its default for exactly that reason, and a change
 * to `'include'` would be a sign someone has moved the API to another origin,
 * where the `__Host-` prefix means the cookie could not follow anyway. Writes
 * carry the browser's own `Origin` header, which the server checks.
 *
 * **An error has a code before it has a message.** `ApiError.code` is the
 * stable branch point — `not_found`, `org_session_mismatch`, `rate_limited` —
 * and `message` is written by the server to be shown to a person. Pages branch
 * on the code and render the message; they never parse the message.
 *
 * **A dead session is handled here, once.** Any response saying the session is
 * gone (`unauthenticated`) or is for another org (`org_session_mismatch`) sends
 * the whole tab through sign-in again (`$lib/login`), so no page has to.
 */

import { reauthenticate } from './login';
import type {
  AuditEvent,
  Job,
  JobDetail,
  JobStatus,
  Lease,
  QueueStats,
  Repo,
  RepoListItem,
  SessionInfo,
  TrackerBinding,
  TrackerConnection,
  TrackerConnections,
  TrackerProvider
} from './types';

/**
 * A failure the server described. Carries the HTTP status too, because a few
 * callers need to tell "no such thing" (`404`) from "not allowed" (`403`) even
 * though the console's own rule is that an org you are not in answers `404`.
 */
export class ApiError extends Error {
  readonly status: number;
  readonly code: string;

  constructor(status: number, code: string, message: string) {
    super(message);
    this.name = 'ApiError';
    this.status = status;
    this.code = code;
  }

  /** No session, or one the server no longer honours. */
  get isUnauthenticated(): boolean {
    return this.status === 401;
  }

  get isNotFound(): boolean {
    return this.status === 404;
  }

  /**
   * The session is gone, or is signed in to a different org than the one asked
   * for — either way the answer is to sign in again.
   */
  get isSessionLost(): boolean {
    return (
      this.status === 401 &&
      (this.code === 'unauthenticated' || this.code === 'org_session_mismatch')
    );
  }
}

interface ErrorBody {
  error?: { code?: string; message?: string };
}

async function request<T>(method: string, path: string, body?: unknown): Promise<T> {
  let response: Response;
  try {
    response = await fetch(path, {
      method,
      headers: body === undefined ? {} : { 'content-type': 'application/json' },
      body: body === undefined ? undefined : JSON.stringify(body)
    });
  } catch {
    // A network failure is not a server answer, and telling a user their
    // credentials were wrong when the wifi dropped sends them to reset
    // something that was never broken.
    throw new ApiError(0, 'network', 'Could not reach the server. Check your connection.');
  }

  if (response.status === 204) {
    return undefined as T;
  }

  const text = await response.text();
  const parsed: unknown = text.length > 0 ? safeJson(text) : undefined;

  if (!response.ok) {
    const described = parsed as ErrorBody | undefined;
    const failure = new ApiError(
      response.status,
      described?.error?.code ?? 'unknown',
      described?.error?.message ?? `The server answered ${response.status}.`
    );
    // `GET /api/session` is how the app *asks* whether it is signed in, and
    // `POST /auth/logout` is how it signs out: a 401 from either is an answer,
    // not a lost session.
    if (failure.isSessionLost && path !== '/api/session' && path !== '/auth/logout') {
      reauthenticate();
    }
    throw failure;
  }

  return parsed as T;
}

function safeJson(text: string): unknown {
  try {
    return JSON.parse(text);
  } catch {
    return undefined;
  }
}

/**
 * Build a query string, dropping anything absent.
 *
 * Absent, not empty: `?repo=` would ask the server to resolve the empty slug,
 * and a filter nobody set must not narrow anything.
 */
function query(params: Record<string, string | number | boolean | undefined>): string {
  const search = new URLSearchParams();
  for (const [key, value] of Object.entries(params)) {
    if (value !== undefined && value !== '') search.set(key, String(value));
  }
  const rendered = search.toString();
  return rendered.length > 0 ? `?${rendered}` : '';
}

const get = <T>(path: string) => request<T>('GET', path);
const post = <T>(path: string, body?: unknown) => request<T>('POST', path, body);
const patch = <T>(path: string, body?: unknown) => request<T>('PATCH', path, body);
const put = <T>(path: string, body?: unknown) => request<T>('PUT', path, body);
const del = <T>(path: string) => request<T>('DELETE', path);

/** Percent-encode one path segment. A slug is user-chosen; a repo named `a/b` must not become two segments. */
const seg = (value: string) => encodeURIComponent(value);

export const api = {
  // -------------------------------------------------------------- session
  /** Who is signed in, and to which org. `401` when nobody is. */
  session: () => get<SessionInfo>('/api/session'),

  /** Revoke at the platform, delete the session, clear the cookie. */
  logout: () => post<void>('/auth/logout'),

  // ---------------------------------------------------------------- repos
  /**
   * `includeLeaseStatus` costs the server an extra org-wide lease read, so it
   * defaults to off — only the Repos page, which renders the presence pill,
   * passes `true`. The overview poller and the queue pages' repo picker call
   * this same endpoint far more often and never read `hasActiveLease`.
   */
  repos: (org: string, includeInactive = false, includeLeaseStatus = false) =>
    get<RepoListItem[]>(
      `/api/orgs/${seg(org)}/repos${query({ includeInactive, includeLeaseStatus })}`
    ),
  registerRepo: (org: string, body: Record<string, unknown>) =>
    post<Repo>(`/api/orgs/${seg(org)}/repos`, body),
  updateRepo: (org: string, repo: string, body: Record<string, unknown>) =>
    patch<Repo>(`/api/orgs/${seg(org)}/repos/${seg(repo)}`, body),
  leases: (org: string, repo: string) =>
    get<Lease[]>(`/api/orgs/${seg(org)}/repos/${seg(repo)}/leases`),

  // ------------------------------------------------------------- trackers
  trackerConnections: (org: string) =>
    get<TrackerConnections>(`/api/orgs/${seg(org)}/tracker-connections`),
  /**
   * Redeem what the provider handed the browser.
   *
   * A POST, like every other single-use redemption in this console: the code
   * arrives in a URL, and a link preview that followed that URL must not be
   * able to spend it.
   */
  connectTracker: (
    org: string,
    provider: TrackerProvider,
    body: { code: string; installationId?: number }
  ) => post<TrackerConnection>(`/api/orgs/${seg(org)}/tracker-connections/${seg(provider)}`, body),
  disconnectTracker: (org: string, provider: TrackerProvider) =>
    del<void>(`/api/orgs/${seg(org)}/tracker-connections/${seg(provider)}`),
  trackerBindings: (org: string, repo: string) =>
    get<TrackerBinding[]>(`/api/orgs/${seg(org)}/repos/${seg(repo)}/tracker-bindings`),
  bindRepo: (
    org: string,
    repo: string,
    provider: TrackerProvider,
    body: { externalRef: string; triggerLabel?: string }
  ) =>
    put<TrackerBinding>(
      `/api/orgs/${seg(org)}/repos/${seg(repo)}/tracker-bindings/${seg(provider)}`,
      body
    ),
  unbindRepo: (org: string, repo: string, provider: TrackerProvider) =>
    del<void>(`/api/orgs/${seg(org)}/repos/${seg(repo)}/tracker-bindings/${seg(provider)}`),

  // ---------------------------------------------------------------- queue
  jobs: (
    org: string,
    filters: {
      status?: JobStatus;
      repo?: string;
      team?: string;
      mine?: boolean;
      limit?: number;
    } = {}
  ) => get<Job[]>(`/api/orgs/${seg(org)}/jobs${query({ ...filters })}`),
  queueStats: (org: string, repo?: string) =>
    get<QueueStats>(`/api/orgs/${seg(org)}/jobs/stats${query({ repo })}`),
  job: (org: string, id: string) => get<JobDetail>(`/api/orgs/${seg(org)}/jobs/${seg(id)}`),

  audit: (org: string, limit = 100) =>
    get<AuditEvent[]>(`/api/orgs/${seg(org)}/audit${query({ limit })}`)
};
