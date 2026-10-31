import adapter from '@sveltejs/adapter-static';
import { vitePreprocess } from '@sveltejs/vite-plugin-svelte';

/**
 * The console is a single-page app, and that is a security decision before it
 * is an architectural one.
 *
 * The session is an `HttpOnly`, `__Host-`-prefixed cookie bound to one origin.
 * A SvelteKit server rendering these pages would have to hold that credential
 * to fetch on the user's behalf — a second process with the keys to every
 * console session, for pages that are behind a login and cannot be cached
 * anyway. Building to static files that `tower-server` serves on the same origin
 * as `/api` keeps the cookie in exactly one place: the browser.
 *
 * `fallback` is what makes deep links work. Every route below `/` is resolved
 * by the client router, so the server answers any unmatched path with the same
 * shell — `/o/acme/queue` typed into the address bar has to load the app, not a
 * 404 from a static file server.
 *
 * @type {import('@sveltejs/kit').Config}
 */
export default {
  preprocess: vitePreprocess(),
  kit: {
    adapter: adapter({ fallback: 'index.html', strict: false }),
    typescript: {
      config(config) {
        // `strict` is set here rather than in tsconfig.json because SvelteKit
        // regenerates the base config on every `svelte-kit sync` and a hand
        // edit to it is silently discarded.
        config.compilerOptions = { ...config.compilerOptions, strict: true };
        return config;
      }
    }
  }
};
