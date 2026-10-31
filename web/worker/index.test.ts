import { afterEach, describe, expect, it, vi } from 'vitest';

import worker, { belongsToOrigin, movesToCanonical, parseLegacyHosts } from './index';

/**
 * `belongsToOrigin` is a second copy of `API_PREFIXES` in
 * `crates/tower-server/src/lib.rs`, in a different language, that nothing forces to
 * agree with the first. The Rust side has
 * `api_prefixes_do_not_match_by_string_prefix_alone` asserting exactly these
 * cases; this is its other half.
 *
 * Drift here is silent in the worst direction. If the edge stops recognising a
 * path as the origin's, the asset router answers it from `index.html` with a
 * `200`, and an agent polling `/api/orgs/nope` gets HTML forever instead of the
 * `404` it can parse.
 */
describe('belongsToOrigin', () => {
  it('claims the API surfaces, bare and nested', () => {
    for (const path of [
      '/api',
      '/api/',
      '/api/orgs/acme',
      '/api/orgs/acme/jobs/job-1',
      '/auth/login',
      '/auth/callback',
      '/auth/logout',
      '/platform/webhooks',
      '/webhooks/github',
      '/oauth',
      '/oauth/authorize',
      '/oauth/token',
      '/mcp',
      '/.well-known',
      '/.well-known/oauth-protected-resource',
      '/.well-known/oauth-authorization-server',
      '/platform/webhooks',
      '/webhooks/github',
      '/webhooks/jira',
      '/healthz',
      '/readyz'
    ]) {
      expect(belongsToOrigin(path), path).toBe(true);
    }
  });

  it('leaves the console its own routes, including the look-alikes', () => {
    for (const path of [
      '/',
      '/authors',
      '/platformer',
      '/webhooks-guide',
      '/o/acme/queue',
      // `/apiary` is a legal org slug and `/mcp-guide` a legal page. A prefix
      // test that is not segment-aware sends both to the origin, which answers
      // a JSON 404 for a page the SPA was going to render.
      '/apiary',
      '/apiary/queue',
      '/mcp-guide',
      '/oauthentication',
      '/readyzzz',
      '/healthzcheck',
      '/.well-knownish',
      '/platformer',
      '/webhookstuff'
    ]) {
      expect(belongsToOrigin(path), path).toBe(false);
    }
  });

  it('matches the prefix list the server keeps, plus the two health routes', () => {
    // The server does not list /healthz and /readyz in API_PREFIXES because it
    // mounts them as real routes ahead of its SPA fallback. The edge has no such
    // precedence, so it must name them or answer them with the console's HTML.
    const serverPrefixes = [
      '/api',
      '/auth',
      '/oauth',
      '/mcp',
      '/.well-known',
      '/platform',
      '/webhooks'
    ];
    const edgeOnly = ['/healthz', '/readyz'];

    for (const prefix of [...serverPrefixes, ...edgeOnly]) {
      expect(belongsToOrigin(prefix), prefix).toBe(true);
      expect(belongsToOrigin(`${prefix}/x`), `${prefix}/x`).toBe(true);
      expect(belongsToOrigin(`${prefix}x`), `${prefix}x`).toBe(false);
    }
  });
});

/**
 * The proxy itself, driven with a stubbed `fetch` and asset binding.
 *
 * Two promises are kept here and nowhere else. The origin learns which public
 * host a request used only from `X-Forwarded-Host` — a Worker cannot choose the
 * `Host` of its subrequest — so the Worker must set it from the URL and never
 * pass on a caller's own value. And during a rename's deprecation window the
 * old host keeps every machine surface working (agents, discovery, webhooks)
 * while a browser is sent to the new one.
 */
describe('the proxy', () => {
  const CANONICAL = 'https://tower.example.com';
  const LEGACY = 'factory.example.com';

  type Env = Parameters<typeof worker.fetch>[1];

  function setup(vars: Partial<Env> = {}) {
    const forwarded: Request[] = [];
    vi.stubGlobal(
      'fetch',
      vi.fn(async (input: Request) => {
        forwarded.push(input);
        return new Response('from origin', { status: 200 });
      })
    );
    const assets = vi.fn(async () => new Response('<!doctype html>', { status: 200 }));
    const env = {
      ASSETS: { fetch: assets } as unknown as Fetcher,
      OF_ORIGIN: 'https://origin.fly.dev',
      ...vars
    } as Env;
    const call = (url: string, init?: RequestInit) => worker.fetch(new Request(url, init), env);
    return { forwarded, assets, call };
  }

  const legacyVars = { OF_PUBLIC_URL: CANONICAL, OF_LEGACY_HOSTS: `${LEGACY}, other.example.com` };

  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it('sets X-Forwarded-Host from the request URL, overwriting what the caller sent', async () => {
    const { forwarded, call } = setup();
    await call(`https://${LEGACY}/mcp`, {
      method: 'POST',
      headers: { 'x-forwarded-host': 'evil.example.com' }
    });
    expect(forwarded).toHaveLength(1);
    expect(forwarded[0].headers.get('x-forwarded-host')).toBe(LEGACY);
    expect(new URL(forwarded[0].url).host).toBe('origin.fly.dev');

    await call('https://tower.example.com:8443/api/orgs/acme');
    expect(forwarded[1].headers.get('x-forwarded-host')).toBe('tower.example.com:8443');
  });

  it('308s a console page on a legacy host to the canonical host, path and query kept', async () => {
    const { forwarded, assets, call } = setup(legacyVars);
    for (const path of ['/', '/o/acme/queue?tab=claimed', '/apiary']) {
      const response = await call(`https://${LEGACY}${path}`);
      expect(response.status, path).toBe(308);
      expect(response.headers.get('location'), path).toBe(`${CANONICAL}${path}`);
    }
    // The second configured legacy host is legacy too, matched without case.
    const other = await call('https://OTHER.example.com/');
    expect(other.status).toBe(308);
    expect(assets).not.toHaveBeenCalled();
    expect(forwarded).toHaveLength(0);
  });

  it('308s /auth/login on a legacy host, so console sign-in starts on the canonical one', async () => {
    const { forwarded, call } = setup(legacyVars);
    const response = await call(`https://${LEGACY}/auth/login?org=acme`);
    expect(response.status).toBe(308);
    expect(response.headers.get('location')).toBe(`${CANONICAL}/auth/login?org=acme`);
    expect(forwarded).toHaveLength(0);
  });

  it('proxies every origin prefix on a legacy host without redirecting, webhooks included', async () => {
    const { forwarded, call } = setup(legacyVars);
    const paths = [
      '/mcp',
      '/.well-known/oauth-protected-resource',
      '/api/orgs/acme/repos',
      '/auth/callback?code=x&state=y',
      '/auth/logout',
      '/oauth/token',
      '/platform/webhooks',
      '/webhooks/github',
      '/webhooks/jira',
      '/healthz',
      '/readyz'
    ];
    for (const path of paths) {
      const response = await call(`https://${LEGACY}${path}`, { method: 'POST' });
      expect(response.status, path).toBe(200);
      expect(await response.text(), path).toBe('from origin');
    }
    expect(forwarded.map((r) => new URL(r.url).pathname + new URL(r.url).search)).toEqual(paths);
    for (const request of forwarded) {
      expect(request.headers.get('x-forwarded-host')).toBe(LEGACY);
    }
  });

  it('serves the canonical host as before', async () => {
    const { forwarded, assets, call } = setup(legacyVars);
    const page = await call(`${CANONICAL}/o/acme/queue`);
    expect(page.status).toBe(200);
    expect(assets).toHaveBeenCalledTimes(1);
    const login = await call(`${CANONICAL}/auth/login`);
    expect(login.status).toBe(200);
    expect(forwarded).toHaveLength(1);
  });

  it('redirects nothing while no legacy host is configured', async () => {
    const { assets, call } = setup();
    const response = await call(`https://${LEGACY}/o/acme/queue`);
    expect(response.status).toBe(200);
    expect(assets).toHaveBeenCalledTimes(1);
  });

  it('stamps its own 308 with the deprecation notice', async () => {
    const { call } = setup(legacyVars);
    const response = await call(`https://${LEGACY}/o/acme`);
    expect(response.status).toBe(308);
    expect(response.headers.get('deprecation')).toBe('true');
    expect(response.headers.get('link')).toBe(`<${CANONICAL}/o/acme>; rel="successor-version"`);
  });

  it('redirects exactly /auth/login, and proxies /auth/login/ like any other origin path', async () => {
    const { forwarded, call } = setup(legacyVars);
    const response = await call(`https://${LEGACY}/auth/login/`);
    expect(response.status).toBe(200);
    expect(forwarded).toHaveLength(1);
    expect(movesToCanonical('/auth/login')).toBe(true);
    expect(movesToCanonical('/auth/login/')).toBe(false);
  });

  it('with an unusable OF_PUBLIC_URL, refuses only a legacy host’s console', async () => {
    for (const OF_PUBLIC_URL of [undefined, '', 'tower.example.com']) {
      const { forwarded, assets, call } = setup({ OF_LEGACY_HOSTS: LEGACY, OF_PUBLIC_URL });
      const refused = await call(`https://${LEGACY}/`);
      expect(refused.status, String(OF_PUBLIC_URL)).toBe(500);
      expect(((await refused.json()) as { error: string }).error).toBe('misconfigured');

      // Machine surfaces on the legacy host, and every host's own pages, are untouched.
      for (const url of [
        `https://${LEGACY}/mcp`,
        `https://${LEGACY}/webhooks/github`,
        'https://tower.example.com/mcp',
        'https://tower.example.com/.well-known/oauth-protected-resource'
      ]) {
        expect((await call(url, { method: 'POST' })).status, url).toBe(200);
      }
      expect((await call('https://tower.example.com/o/acme')).status).toBe(200);
      expect(forwarded).toHaveLength(4);
      expect(assets).toHaveBeenCalledTimes(1);
    }
  });

  it('refuses a legacy list that names the canonical host, as the origin does at boot', async () => {
    const { forwarded, assets, call } = setup({
      OF_PUBLIC_URL: CANONICAL,
      OF_LEGACY_HOSTS: `${LEGACY}, TOWER.example.com`
    });
    for (const url of [`${CANONICAL}/o/acme`, `${CANONICAL}/auth/login`, `https://${LEGACY}/`]) {
      const refused = await call(url);
      expect(refused.status, url).toBe(500);
      const body = (await refused.json()) as { error: string; error_description: string };
      expect(body.error, url).toBe('misconfigured');
      expect(body.error_description).toContain('OF_PUBLIC_URL');
    }
    expect(assets).not.toHaveBeenCalled();

    // Agents, discovery and webhooks keep working whatever the vars say.
    for (const url of [`${CANONICAL}/mcp`, `https://${LEGACY}/webhooks/github`]) {
      expect((await call(url, { method: 'POST' })).status, url).toBe(200);
    }
    expect(forwarded).toHaveLength(2);
  });

  it('parses OF_LEGACY_HOSTS as strictly as the origin does', () => {
    expect(parseLegacyHosts(undefined)).toEqual([]);
    expect(parseLegacyHosts('  ')).toEqual([]);
    expect(parseLegacyHosts(' Factory.Example.com , old.example.com')).toEqual([
      'factory.example.com',
      'old.example.com'
    ]);
    for (const bad of [
      'https://factory.example.com',
      'factory.example.com:443',
      'factory.example.com/mcp',
      'user@factory.example.com',
      'factory.example.com.',
      'factory..example.com',
      'factory.example.com,',
      'a.example.com,,b.example.com',
      'factory example.com'
    ]) {
      expect(parseLegacyHosts(bad), bad).toBeNull();
    }
  });

  it('with a malformed OF_LEGACY_HOSTS, still serves the canonical host and every origin path', async () => {
    const { forwarded, assets, call } = setup({
      OF_LEGACY_HOSTS: `https://${LEGACY}`,
      OF_PUBLIC_URL: CANONICAL
    });
    expect((await call(`${CANONICAL}/o/acme`)).status).toBe(200);
    expect(assets).toHaveBeenCalledTimes(1);
    for (const url of [`https://${LEGACY}/mcp`, `https://${LEGACY}/platform/webhooks`]) {
      expect((await call(url, { method: 'POST' })).status, url).toBe(200);
    }
    expect(forwarded).toHaveLength(2);

    // A page on a host it cannot classify is refused rather than guessed at.
    const refused = await call(`https://${LEGACY}/o/acme`);
    expect(refused.status).toBe(500);
  });
});
