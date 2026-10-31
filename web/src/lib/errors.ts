/**
 * Turning a failure into a sentence the reader can act on, in their language.
 *
 * **This works at all because of a rule the server already keeps.** `api.ts`
 * states it: *an error has a code before it has a message.* `ApiError.code` is
 * the stable branch point — `not_found`, `rate_limited`, `forbidden` — and the
 * `message` beside it is English prose written by the server. Pages have always
 * been forbidden from parsing the message; this module is what they switch on
 * the code with instead.
 *
 * **An unknown code renders the thrower's English message rather than nothing.**
 * The server can add an error tomorrow and this bundle will not know its code;
 * an untranslated sentence is a far better outcome than a blank alert, and it
 * is the reason the code/message split is worth keeping on the wire. There is a
 * test for exactly that path.
 */

import { ApiError } from './api';
import { m } from './paraglide/messages';

/**
 * Every code this bundle has a translation for.
 *
 * Enumerated from `tower_core::Error::code()`, `tower-web`'s own error codes, and the
 * two codes `api.ts` mints for itself. Deliberately not
 * exhaustive over what the server *could* send — see the fallback above. A code
 * missing from here is a sentence in English, not a missing sentence.
 */
const KNOWN: Record<string, () => string> = {
  // ---- transport, minted by api.ts rather than received ----
  network: () => m.error_network(),
  unknown: () => m.error_unknown(),

  // ---- tower-core ----
  job_not_found: () => m.error_job_not_found(),
  repo_not_found: () => m.error_repo_not_found(),
  repo_unresolved: () => m.error_repo_unresolved(),
  repo_slug_taken: () => m.error_repo_slug_taken(),
  remote_taken: () => m.error_remote_taken(),
  wrong_status: () => m.error_wrong_status(),
  already_claimed: () => m.error_already_claimed(),
  ticket_already_linked: () => m.error_ticket_already_linked(),
  dependency_cycle: () => m.error_dependency_cycle(),
  lease_held: () => m.error_lease_held(),
  lease_not_held: () => m.error_lease_not_held(),
  org_not_found: () => m.error_org_not_found(),
  team_not_found: () => m.error_team_not_found(),
  invalid_argument: () => m.error_invalid_argument(),
  internal_error: () => m.error_internal(),

  // ---- tower-web ----
  unauthenticated: () => m.error_unauthenticated(),
  org_session_mismatch: () => m.error_org_session_mismatch(),
  console_login_disabled: () => m.error_console_login_disabled(),
  platform_unavailable: () => m.error_platform_unavailable(),
  not_found: () => m.error_not_found(),
  forbidden: () => m.error_forbidden(),
  rate_limited: () => m.error_rate_limited(),
  invalid_request: () => m.error_invalid_request(),
  tracker_unreachable: () => m.error_tracker_unreachable()
};

/**
 * The sentence to show for a failure.
 *
 * `fallback` is what to say for something that is not an `ApiError` — a bug in this bundle, a `TypeError`, anything whose message
 * was written for a developer rather than a reader. Callers pass a keyed
 * message; never a raw `String(e)`.
 */
export function messageFor(error: unknown, fallback: string): string {
  if (error instanceof ApiError) {
    // The server's own English message, for a code this bundle predates.
    return KNOWN[error.code]?.() ?? error.message;
  }

  return fallback;
}
