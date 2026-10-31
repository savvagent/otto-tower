/**
 * The console at the edge, and the one origin a browser is ever allowed to see.
 *
 * Cloudflare serves the built SPA from its own network and forwards everything
 * dynamic — `/api`, `/auth` (console sign-in), `/oauth`, `/.well-known`, `/mcp`,
 * `/platform` and `/webhooks` (the signed webhook receivers), and the health
 * probes — to
 * `tower-server`. The browser talks to exactly one hostname, which is not a
 * performance decision: the console's session is an `HttpOnly`, `__Host-`
 * prefixed cookie, and `__Host-` means the browser refuses to store it unless it
 * is `Secure`, has `Path=/`, and carries no `Domain`. Put the SPA on one
 * hostname and the API on another and that cookie cannot be sent at all — no
 * CORS header rescues it, and the fix would be a different auth transport, which
 * `docs/specs/2026-09-01-otto-factory-design.md` refuses on purpose.
 *
 * This Worker therefore proxies rather than redirects, and it is the reason the
 * split-origin option in the issue was never really an option. The one
 * exception is a rename's deprecation window: on a legacy host the console's
 * pages are redirected to the canonical one (see `movesToCanonical`), because a
 * session cookie bound to the old host is a session the new one cannot use.
 *
 * That redirect applies only to a deployment fronted by this Worker. Production
 * today is not (the public hosts resolve straight to Fly), so `tower-server` makes
 * the same redirect itself (`legacy_console_redirect` in
 * `crates/tower-server/src/lib.rs`); with the Worker in front, the Worker answers
 * first and the origin never sees the request.
 */

/**
 * Path prefixes that belong to `tower-server` rather than to the console's own
 * client-side routing. **This list mirrors `API_PREFIXES` in
 * `crates/tower-server/src/lib.rs`** and must not drift from it: the server keeps
 * the same list to decide what gets a JSON `404` instead of `index.html`, and a
 * prefix that exists on one side only is a route that behaves differently
 * depending on whether Cloudflare is in front.
 *
 * `/healthz` and `/readyz` are here and not on the server's list because the
 * server mounts them as real routes ahead of its SPA fallback. At the edge there
 * is no such precedence — anything not proxied is answered from the asset
 * bundle — so leaving them out would make `/readyz` return the console's HTML
 * with a `200`, which is the shape that keeps a health check green while the
 * database is gone.
 */
const ORIGIN_PREFIXES = [
  '/api',
  '/auth',
  '/oauth',
  '/mcp',
  '/.well-known',
  '/platform',
  '/webhooks',
  '/healthz',
  '/readyz'
];

/**
 * Prefix matching on segment boundaries, exactly as the server does it.
 *
 * `startsWith` alone is wrong and quietly so: `/apiary` is a legal org slug and
 * belongs to the console, and `/mcp-guide` is a page. Sending either to the
 * origin would answer a JSON `404` for a route the SPA was going to render.
 */
export function belongsToOrigin(path: string): boolean {
  return ORIGIN_PREFIXES.some((prefix) => path === prefix || path.startsWith(`${prefix}/`));
}

export interface Env {
  /** The built `web/build` bundle, uploaded with the Worker. */
  ASSETS: Fetcher;
  /** Where `tower-server` actually listens, e.g. `https://otto-factory-mcp.fly.dev`. */
  OF_ORIGIN: string;
  /**
   * Deprecated public hostnames, comma separated — the same value as the
   * origin's `OF_LEGACY_HOSTS`. Unset outside a rename's deprecation window,
   * and then nothing is redirected.
   */
  OF_LEGACY_HOSTS?: string;
  /**
   * The canonical public origin a legacy host's browser traffic is sent to —
   * the same value as the origin's `OF_PUBLIC_URL`. Read only when
   * `OF_LEGACY_HOSTS` is set.
   */
  OF_PUBLIC_URL?: string;
}

/**
 * Whether a path is one a legacy host sends to the canonical host: the
 * console's pages, and exactly `/auth/login`.
 *
 * Only a browser's routes move, and `/auth/login` so sign-in starts on — and
 * its `__Host-` session cookie lands on — the canonical host. Everything else
 * under an origin prefix keeps working where it is until the sunset, because a
 * machine sent it: an agent on `/mcp` and `/.well-known` that would not follow
 * a redirect with its bearer token, a tracker or the platform delivering a
 * webhook that would not follow one at all, and the rest of `/auth`
 * (`/auth/callback` must complete on the host that set the PKCE cookie). The
 * origin stamps those responses with the deprecation notice.
 *
 * `/auth/login/` is deliberately *not* redirected: the origin serves no such
 * route, so it is proxied and answers its JSON `404` (with the notice) exactly
 * as on the canonical host. Matching it here too would mean a redirect to a
 * page that does not exist.
 */
export function movesToCanonical(path: string): boolean {
  return !belongsToOrigin(path) || path === '/auth/login';
}

/**
 * One label-dotted bare hostname: what the origin's `OF_LEGACY_HOSTS` parser
 * accepts, and nothing it refuses — no scheme, port, path, credentials, empty
 * entry or trailing dot.
 */
const BARE_HOSTNAME = /^[a-z0-9-]+(\.[a-z0-9-]+)*$/;

/**
 * `OF_LEGACY_HOSTS`, parsed as strictly as `tower-server` parses it. `[]` when it
 * is unset or blank (outside a rename), `null` when it is set but not a list of
 * bare hostnames.
 */
export function parseLegacyHosts(raw: string | undefined): string[] | null {
  if (!raw || raw.trim() === '') {
    return [];
  }
  const hosts = raw.split(',').map((host) => host.trim().toLowerCase());
  return hosts.every((host) => BARE_HOSTNAME.test(host)) ? hosts : null;
}

/** `OF_PUBLIC_URL` as a bare origin, or `null` when it is not an http(s) URL. */
function canonicalOrigin(env: Env): string | null {
  try {
    const url = new URL(env.OF_PUBLIC_URL ?? '');
    return url.protocol === 'https:' || url.protocol === 'http:' ? url.origin : null;
  } catch {
    return null;
  }
}

/**
 * What to do with a request during a rename's deprecation window: a `308` to
 * the canonical host, a refusal because the configuration cannot say, or
 * `null` to handle it as on any other day.
 *
 * A broken configuration is refused only where it matters — a console request
 * that might have been redirected and whose host cannot be vouched for. Every
 * origin prefix (agents, discovery, webhooks) is proxied whatever the vars say,
 * so a typo in these two vars cannot take agents or webhooks down. How much of
 * the console is refused depends on the mistake:
 *
 * - `OF_LEGACY_HOSTS` malformed: every host's console except the canonical
 *   one, which is still served when `OF_PUBLIC_URL` names it.
 * - `OF_LEGACY_HOSTS` names `OF_PUBLIC_URL`'s own host (refused by tower-server at
 *   boot): every host's console, the canonical one included, since serving it
 *   would mean redirecting it to itself.
 * - `OF_PUBLIC_URL` unusable: only a legacy host's console, which has nowhere
 *   to be sent.
 */
function legacyDecision(url: URL, env: Env): Response | null {
  if (!env.OF_LEGACY_HOSTS || env.OF_LEGACY_HOSTS.trim() === '') {
    return null;
  }
  if (!movesToCanonical(url.pathname)) {
    return null;
  }
  const host = url.hostname.toLowerCase();
  const canonical = canonicalOrigin(env);
  const hosts = parseLegacyHosts(env.OF_LEGACY_HOSTS);

  if (hosts === null) {
    if (canonical && new URL(canonical).hostname === host) {
      return null;
    }
    return misconfigured(
      'this Worker’s OF_LEGACY_HOSTS is not a comma-separated list of bare hostnames ' +
        '(no scheme, port, path, empty entry or trailing dot), so it cannot tell whether ' +
        'this host is a legacy one. Set it to the same value as tower-server’s.'
    );
  }
  if (canonical && hosts.includes(new URL(canonical).hostname)) {
    // tower-server refuses to boot with this (`the_canonical_host_cannot_also_be_legacy`).
    // Here it would 308 the canonical host's console to itself, forever.
    return misconfigured(
      'this Worker’s OF_LEGACY_HOSTS names OF_PUBLIC_URL’s own host, so the canonical ' +
        'console would be redirected to itself. Remove the canonical host from ' +
        'OF_LEGACY_HOSTS; tower-server refuses the same value at startup.'
    );
  }
  if (!hosts.includes(host)) {
    return null;
  }
  if (!canonical) {
    return misconfigured(
      'this Worker has OF_LEGACY_HOSTS but no usable OF_PUBLIC_URL, so it does not ' +
        'know the canonical host to send a legacy host’s console to. Set OF_PUBLIC_URL ' +
        'to the same https:// origin as tower-server’s.'
    );
  }
  const location = `${canonical}${url.pathname}${url.search}`;
  // `308` rather than `301`: it keeps the method and is never turned into a
  // `GET`. It carries the same notice the origin puts on everything else from a
  // legacy host; `Sunset` is the origin's to announce, since only it is told
  // the date.
  return new Response(null, {
    status: 308,
    headers: {
      location,
      deprecation: 'true',
      link: `<${location}>; rel="successor-version"`
    }
  });
}

function misconfigured(description: string): Response {
  return new Response(JSON.stringify({ error: 'misconfigured', error_description: description }), {
    status: 500,
    headers: { 'content-type': 'application/json' }
  });
}

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const url = new URL(request.url);

    const legacy = legacyDecision(url, env);
    if (legacy) {
      return legacy;
    }

    if (!belongsToOrigin(url.pathname)) {
      // `not_found_handling: "single-page-application"` turns an unknown path
      // into `index.html`, which is what makes a hard refresh of
      // `/o/acme/queue` work.
      return env.ASSETS.fetch(request);
    }

    if (!env.OF_ORIGIN) {
      // Loud, and in the one place it can be seen. Without this the failure is
      // a URL constructor throwing inside a proxy hop, which reaches the caller
      // as a bare 500 with nothing naming the cause.
      return misconfigured(
        'this Worker has no OF_ORIGIN, so it does not know where tower-server is. ' +
          'Deploy with --env production, or pass --var OF_ORIGIN:https://…'
      );
    }

    const target = new URL(url.pathname + url.search, env.OF_ORIGIN);
    const headers = new Headers(request.headers);

    // The origin keys every per-IP throttle on this header
    // (`OF_CLIENT_IP_HEADER=cf-connecting-ip`), so what it contains has to be
    // the platform's value and never the caller's. Cloudflare overwrites
    // `CF-Connecting-IP` before the Worker is invoked, which is what makes the
    // inbound value safe to forward: a request sent here with
    // `cf-connecting-ip: 9.9.9.9` reached the origin as `127.0.0.1` under
    // `wrangler dev`, the caller's value having been discarded on the way in.
    // The `delete` is for the case where there is no value at all — an empty
    // header would become one throttle bucket shared by everyone.
    //
    // None of this holds if the origin can be reached without going through
    // Cloudflare: a direct caller sets the header itself and every throttle then
    // counts something the attacker chose. Locking the origin down is therefore
    // part of the deployment, not an optimisation — see docs/deploy/cloudflare.md.
    const clientIp = request.headers.get('cf-connecting-ip');
    if (clientIp) {
      headers.set('cf-connecting-ip', clientIp);
    } else {
      headers.delete('cf-connecting-ip');
    }

    // The public host the caller used. The subrequest's `Host` is the origin's
    // own name (a Worker cannot choose it), so this is the only way the origin
    // can tell the canonical host from a legacy one and attach the deprecation
    // notice. It is *set*, from the URL Cloudflare routed on, never passed
    // through: a caller's own `X-Forwarded-Host` would otherwise decide which
    // host the origin believes it was reached on. (The origin only lets that
    // choose among hosts its operator configured, never the token audience, so
    // a forged value is harmless there too — but it should not be possible.)
    headers.set('x-forwarded-host', url.host);

    return fetch(
      new Request(target, {
        method: request.method,
        headers,
        body: request.body,
        // A proxy that follows redirects is not a proxy. `/auth/login` answers
        // `303` to the platform's authorize endpoint, and `/auth/callback`
        // answers `303` into the console with the session cookie on it —
        // following either here would swallow the `Set-Cookie` and fetch the
        // destination from Cloudflare instead of the browser. (Back when this
        // service was its own authorization server, `/oauth/authorize`'s `303`
        // to a loopback address burned a single-use code the same way.)
        redirect: 'manual',
        // Nothing on these paths is cacheable and some of it is per-session.
        // A heuristically cached `GET /api/me` is one user's identity served to
        // another. A negative TTL is the documented way to say "never", where
        // `cacheTtl: 0` only means "expired".
        cf: { cacheTtlByStatus: { '200-599': -1 } }
      })
    );
  }
} satisfies ExportedHandler<Env>;
