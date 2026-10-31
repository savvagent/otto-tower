import { discoverPlatformUrl } from './platform';

/**
 * Where the header's "Back to Savvagent" link goes, and when to go looking for it.
 *
 * A signed-in session already says where the platform is. A visitor without a
 * usable address — signed out, a failed session lookup, or an address
 * `platformHome` refused — asks the open discovery document instead, once. The
 * last address known is remembered, so signing out does not blank a link while
 * nothing about where the platform is has changed, and need fetch nothing.
 *
 * A class rather than inline effect state so the decision can be unit-tested:
 * the layout feeds it the session's scheme-checked address and readiness from
 * an `$effect`, and reads `url()` from a `$derived`.
 */
export class PlatformHome {
  /** The last address known: the session's while signed in, else discovery's. */
  #known = $state<string | undefined>(undefined);
  /** Plain, not `$state`: a once-only latch nothing needs to react to. */
  #started = false;
  readonly #discover: () => Promise<string | undefined>;

  constructor(discover: () => Promise<string | undefined> = () => discoverPlatformUrl()) {
    this.#discover = discover;
  }

  /** The link's address, given the session's checked one: that wins, else the last known. */
  url(base: string | undefined): string | undefined {
    return base ?? this.#known;
  }

  /** Report the session's checked address and whether the session lookup has resolved. */
  observe(base: string | undefined, ready: boolean): void {
    if (base) {
      this.#known = base;
      this.#started = true;
      return;
    }
    if (!ready || this.#started) return;
    this.#started = true;
    // `??=`, not `=`: an answer arriving after the session has supplied an
    // address must not replace it — least of all with a failure's `undefined`.
    void this.#discover().then((url) => {
      this.#known ??= url;
    });
  }
}
