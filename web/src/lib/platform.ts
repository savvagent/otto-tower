/**
 * Links into the Savvagent platform's own console.
 *
 * Identity is not this app's: members, teams, SSO, usage, tokens, the account,
 * and the list of orgs all live at the platform, which is also what signed this
 * person in. The Savvagent Tower console links there rather than re-implementing any of
 * it. Every such link is built here, from the `platformUrl` the server reports
 * (`GET /api/session`), so a deployment's platform address is never baked into
 * the bundle and there is one place to fix a path when the platform's console
 * moves it.
 */

export type PlatformPage = 'members' | 'teams' | 'sso' | 'usage' | 'account' | 'orgs';

/** The platform console page for `page`, for the org `slug` where it is org-scoped. */
export function platformLink(platformUrl: string, page: PlatformPage, slug: string): string {
  const base = platformUrl.replace(/\/+$/, '');
  const org = `${base}/o/${encodeURIComponent(slug)}`;
  switch (page) {
    case 'members':
    case 'teams':
    case 'sso':
    case 'usage':
      return `${org}/${page}`;
    case 'account':
      return `${base}/settings`;
    case 'orgs':
      return base;
  }
}

/**
 * The platform console's home, for the header's "Back to Savvagent" link — or
 * `undefined` when `url` is not an absolute `http(s)` URL.
 *
 * Both places the address comes from are this server's own answers, so the
 * scheme check is defence in depth: a misconfigured or mangled value must never
 * become a `javascript:` href. `undefined` hides the link; there is no guessed
 * fallback, because a hard-coded platform address is how a staging console ends
 * up sending people to production.
 */
export function platformHome(url: string | undefined): string | undefined {
  if (!url) return undefined;
  let parsed: URL;
  try {
    parsed = new URL(url);
  } catch {
    return undefined;
  }
  if (parsed.protocol !== 'https:' && parsed.protocol !== 'http:') return undefined;
  // Credentials in the configured URL would be printed into every visitor's
  // DOM, signed out included; refuse rather than strip, so the misconfiguration
  // shows up as a missing link instead of a working one that hides it.
  if (parsed.username || parsed.password) return undefined;
  return (parsed.origin + parsed.pathname).replace(/\/+$/, '');
}

/**
 * Where the platform is, for a visitor with no session to ask.
 *
 * `/api/session` reports `platformUrl` only to someone signed in. A signed-out
 * visitor — or one whose session lookup failed — can still be pointed back,
 * because the open `/.well-known/oauth-protected-resource` (RFC 9728) names the
 * platform as this service's authorization server, from the same
 * `OF_PLATFORM_URL`.
 *
 * Every failure is `undefined`, quietly. This is the one place silence is the
 * right answer: the link is a convenience, and hiding it is exactly what an
 * unknown address calls for.
 */
export async function discoverPlatformUrl(
  fetcher: typeof fetch = fetch
): Promise<string | undefined> {
  try {
    const response = await fetcher('/.well-known/oauth-protected-resource', {
      headers: { accept: 'application/json' }
    });
    if (!response.ok) return undefined;
    const body: unknown = await response.json();
    if (typeof body !== 'object' || body === null) return undefined;
    const servers = (body as { authorization_servers?: unknown }).authorization_servers;
    if (!Array.isArray(servers) || typeof servers[0] !== 'string') return undefined;
    return platformHome(servers[0]);
  } catch {
    return undefined;
  }
}
