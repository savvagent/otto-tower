/**
 * Sending the browser through sign-in.
 *
 * The console does not sign anyone in itself. `of-web` runs an OAuth
 * authorization-code + PKCE flow against the Savvagent platform (`/auth/login` →
 * platform → `/auth/callback`) and, when it finishes, sets the session cookie and
 * redirects back. This module only builds the URL that starts it and decides when
 * starting it again would be a loop.
 *
 * **Always a full navigation, never `goto`.** `/auth/login` is a server route
 * that answers a redirect to another origin; the client router has no such page
 * and would render a 404 in the middle of an OAuth flow. (Links to it carry
 * `data-sveltekit-reload` for the same reason.)
 *
 * **`next` is only ever a path this app produced.** The server checks it again
 * and refuses anything that is not a same-origin path, so a crafted link cannot
 * bounce a signed-in visitor somewhere else.
 */

const LOOP_KEY = 'of.relogin';
/** How long a sign-in for an org counts as "just tried". */
const LOOP_WINDOW_MS = 60_000;

/** Where to send the browser to sign in, optionally to a particular org. */
export function loginUrl(org?: string, next?: string): string {
  const params = new URLSearchParams();
  if (org) params.set('org', org);
  if (next) params.set('next', next);
  const rendered = params.toString();
  return rendered.length > 0 ? `/auth/login?${rendered}` : '/auth/login';
}

/** The org slug in a console path (`/o/{slug}/…`), if it is one. */
export function orgOf(pathname: string): string | undefined {
  const match = /^\/o\/([^/]+)/.exec(pathname);
  if (!match?.[1]) return undefined;
  try {
    return decodeURIComponent(match[1]);
  } catch {
    return undefined;
  }
}

/**
 * Whether signing in again for `org` is worth trying.
 *
 * A session is bound to one org, so a visitor on `/o/globex` with an `acme`
 * session is sent to sign in for `globex`. If the platform then signs them in to
 * `acme` anyway — they are not a member of `globex` — they come straight back to
 * the same mismatch, and without this they would loop between the two forever.
 * One attempt per org per minute; after that the page says what is wrong.
 *
 * `sessionStorage` is per-tab and may throw (blocked site data); failing to
 * record the attempt just means a loop is not detected, so it is swallowed.
 */
export function mayRetryLogin(org: string | undefined, now = Date.now()): boolean {
  const key = org ?? '';
  try {
    const last = JSON.parse(sessionStorage.getItem(LOOP_KEY) ?? 'null') as {
      org: string;
      at: number;
    } | null;
    if (last && last.org === key && now - last.at < LOOP_WINDOW_MS) return false;
    sessionStorage.setItem(LOOP_KEY, JSON.stringify({ org: key, at: now }));
  } catch {
    // Not recorded; see above.
  }
  return true;
}

/**
 * Start sign-in for the page the visitor is on, coming back to it afterwards.
 * Returns false (and does nothing) when that would only repeat an attempt that
 * just failed to change anything.
 */
export function signIn(org?: string, next?: string): boolean {
  // Several things can notice a dead session in the same tick — the API client,
  // a poller, the root layout's guard. The first one navigates; the rest have
  // nothing to add, and must not be mistaken for a retry of a failed attempt.
  if (navigating) return true;
  if (!mayRetryLogin(org)) return false;
  navigating = true;
  location.assign(loginUrl(org, next));
  return true;
}

let navigating = false;

/**
 * The API said the session is gone or is for another org. Sign in again for the
 * org the page is about, and come back to the same URL.
 */
export function reauthenticate(): void {
  signIn(orgOf(location.pathname), location.pathname + location.search);
}
