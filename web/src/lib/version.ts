import { version } from '../../package.json';

/**
 * The console bundle's own release version.
 *
 * `web/package.json`'s `version` and the workspace crate version move together
 * via release-please (see CLAUDE.md, "Releases & versioning"), so this value
 * with no network call is always the version the console was built from.
 *
 * Whether it is also the version the *accompanying server* was built from
 * depends on the deploy shape. Under the single-image Docker/Fly shape
 * (`Dockerfile`, `docs/deploy/fly.md`) the console bundle and the `tower-server`
 * binary are built in the same `Dockerfile` from the same source tree, so the
 * two cannot drift. Under the Cloudflare Worker shape (`docs/deploy/cloudflare.md`)
 * the console is deployed independently (`npm run deploy`) from `tower-server`
 * (`flyctl deploy`), on its own cadence — `docs/deploy/cloudflare.md`'s own
 * "What is still open" section notes the Worker's bundle and the image's
 * bundle can drift because nothing deploys both from one commit. So this is
 * the version the console was built from, not a guarantee about the origin
 * it happens to be talking to.
 */
export const APP_VERSION = version;
