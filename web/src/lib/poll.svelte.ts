/**
 * Polling that a page can hand to an `$effect`.
 *
 * Written for the overview — a queue that changes while nobody is touching the
 * browser, on the page most likely to be left open on a second monitor — and
 * kept out of it because the queue and repos pages have the same problem and
 * should not re-derive these rules one at a time.
 *
 * **Why polling, when the server already has a change stream.** `tower-core`'s
 * `watch.rs` exists so agents can react to queue changes without polling, and
 * the MCP `watch` tool carries the same data this page reads. The console
 * deliberately does not use it: a stream to the browser would need a new
 * console route in `catalog.rs` and a held connection per open tab, fanned out
 * to readers rather than to agents, for a page whose data is a second stale at
 * worst. Request/response over endpoints that already exist costs nothing new
 * on the server. This is a decision, not an oversight.
 *
 * Six rules, each of which is a way a polling page becomes worse than a static
 * one:
 *
 * 1. **A refresh is not a load.** `loading` is true only while the first result
 *    of a subscription is outstanding. Flipping it on every tick replaces a
 *    working page with a skeleton twice a minute — the numbers are supposed to
 *    change in place.
 * 2. **A failed refresh keeps the last good result** and says how old it is.
 *    `failed`/`error` mean "there is nothing to render"; a failure on top of
 *    data sets `stale` and leaves `value` alone. But see rule 3 — that applies
 *    only to failures worth retrying.
 * 3. **A failure the caller calls `fatal` stops the poll.** A `401` or a `404`
 *    is not a blip: the session or the membership is gone, and a page that
 *    keeps rendering the last good answer under a small warning is asserting
 *    access it no longer has. The classifier is the caller's, because this
 *    module knows nothing about `ApiError`.
 * 4. **A hidden tab does not poll**, and refreshes the moment it is shown
 *    again. The *first* load runs regardless of visibility — a tab opened in
 *    the background should have data ready when it is looked at; it is the
 *    repeat that is suppressed.
 * 5. **Refreshes never overlap, and a failing one backs off.** The interval
 *    runs from the end of one refresh to the start of the next — a chained
 *    `setTimeout`, never `setInterval` — so a slow response delays the next
 *    tick instead of stacking behind it, and consecutive failures widen the gap
 *    — doubling to a cap, jittered — so an outage is not met with undiminished
 *    pressure from every open tab, nor by all of them at the same instant. A
 *    healthy poll keeps the interval exactly; what is randomized there is the
 *    phase, once per subscription and again on recovery, so tabs a returning
 *    server answered together do not stay in lockstep forever.
 *    A load that never settles is failed by `timeout`, because a hung `fetch`
 *    would otherwise leave a dead poll wearing a healthy page's face.
 * 6. **A tab nobody has touched for hours parks itself.** `sessions.rs` slides
 *    the browser session's 14-day idle deadline forward on every authenticated
 *    request, which quietly assumes requests imply a human. An open console
 *    tab breaks that assumption — `visibilityState` stays `'visible'` on an
 *    unattended second monitor, which is exactly the case this feature is for —
 *    so a poll with no sign of a person behind it stops until there is one, and
 *    the server's idle timeout means what it says again.
 *
 * `load` is called on a timer, so it must be cheap and idempotent. Whether what
 * it does is billable is a property of the caller: `tower-billing` meters MCP tool
 * calls, and the console's `GET`s are not among them.
 */

/** Fast enough to feel live for jobs that run for minutes. */
export const REFRESH_INTERVAL = 30_000;

/** Shorter than the interval, so a hung load cannot stack behind itself. */
export const LOAD_TIMEOUT = 15_000;

/**
 * Long enough to survive a working day at a desk, far short of the server's
 * 14-day idle session deadline that rule 6 exists to keep meaningful.
 */
export const IDLE_AFTER = 4 * 60 * 60 * 1000;

/** A floor under `PollOptions.interval`: a zero would be a hot loop on the API. */
const MIN_INTERVAL = 1_000;

/** Backoff caps at 8× the interval — four minutes at the default. */
const MAX_BACKOFF = 8;

/** How far a subscription's phase may be pushed: up to 30% of one interval. */
const PHASE_SPREAD = 0.3;

/** What `load` rejects with when it does not answer within `timeout`. */
export class PollTimeout extends Error {
  constructor(ms: number) {
    super(`The request did not answer within ${ms}ms.`);
    this.name = 'PollTimeout';
  }
}

/**
 * Whether anyone is looking, injected rather than read from `document`.
 *
 * The same seam as `LocaleStore` in `locale.ts` — for the same reason rather
 * than in the same shape: a global a test has to stub is a global it has to
 * un-stub, and "pause while hidden, refresh on return" becomes ordinary state.
 */
export interface Visibility {
  hidden(): boolean;
  /** The returned function unsubscribes, and tolerates being called twice. */
  subscribe(onChange: () => void): () => void;
}

/** Any sign that a person is still there. See rule 6. */
export interface Activity {
  subscribe(onActive: () => void): () => void;
}

/**
 * The real thing.
 *
 * Guarded on `typeof document` because `adapter-static` builds in Node, where
 * reaching for it breaks the build rather than a page. **The no-`document`
 * answer is "hidden"**: somewhere with no document has nobody looking, and the
 * safe answer to "can I tell?" is the one that does not poll.
 */
export const documentVisibility: Visibility = {
  hidden: () => typeof document === 'undefined' || document.visibilityState === 'hidden',
  subscribe(onChange) {
    if (typeof document === 'undefined') return () => {};
    document.addEventListener('visibilitychange', onChange);
    return () => document.removeEventListener('visibilitychange', onChange);
  }
};

/**
 * Input events, as evidence of a person.
 *
 * Passive and capturing: these must never delay a scroll or be swallowed by a
 * handler that stops propagation, because a missed event reads as absence and
 * parks a poll somebody is watching.
 */
export const documentActivity: Activity = {
  subscribe(onActive) {
    if (typeof document === 'undefined') return () => {};
    const events = ['pointerdown', 'keydown', 'wheel', 'touchstart', 'focus'] as const;
    const options = { passive: true, capture: true } as const;
    for (const event of events) document.addEventListener(event, onActive, options);
    return () => {
      for (const event of events) document.removeEventListener(event, onActive, options);
    };
  }
};

export interface PollOptions {
  /** Milliseconds between the *end* of one refresh and the start of the next. */
  interval?: number;
  /** How long a single `load` may take before it is failed. */
  timeout?: number;
  /** How long with no user input before the poll parks itself (rule 6). */
  idleAfter?: number;
  /**
   * Failures that must not be retried — a revoked session, a membership that
   * is gone. Returning `true` stops the poll and surfaces the failure through
   * `error` instead of `stale`.
   */
  fatal?: (failure: unknown) => boolean;
  visibility?: Visibility;
  activity?: Activity;
  /** Randomness for the retry jitter and the phase offset, injectable for tests. */
  random?: () => number;
}

export class Poller<T> {
  // Private with getters: the class is the only thing that may write these, and
  // the legal combinations (see the rules above) are not expressible in the
  // types. Public `$state` fields would let a page put this object into a state
  // its own documentation calls impossible.
  #value = $state<T | undefined>(undefined);
  #loading = $state(false);
  #failed = $state(false);
  #error = $state<unknown>(undefined);
  #stale = $state(false);
  #failures = $state(0);
  #updatedAt = $state<number | undefined>(undefined);
  #parked = $state(false);
  #terminal = $state(false);

  /** The last successful result; `undefined` before the first one lands. */
  get value(): T | undefined {
    return this.#value;
  }

  /** True only while the first result of the current subscription is in flight. */
  get loading(): boolean {
    return this.#loading;
  }

  /**
   * There is nothing to render: the first load failed, or a later one failed
   * `fatal`. Distinct from `error` being set, so a failure whose value *is*
   * `undefined` cannot render as a confident empty page.
   */
  get failed(): boolean {
    return this.#failed;
  }

  /** Why. Mapped to a sentence by the caller — `messageFor` takes `unknown`. */
  get error(): unknown {
    return this.#error;
  }

  /** The last refresh failed and `value` is what worked before it. */
  get stale(): boolean {
    return this.#stale;
  }

  /** Consecutive failed refreshes; back to 0 on any success. */
  get failures(): number {
    return this.#failures;
  }

  /** When `value` was fetched, so a stale page can say how far behind it is. */
  get updatedAt(): number | undefined {
    return this.#updatedAt;
  }

  /** Parked for want of a human (rule 6). Any input resumes it. */
  get parked(): boolean {
    return this.#parked;
  }

  /**
   * The poll is over: a `fatal` failure ended it, and nothing will retry. The
   * page uses this to tell "failed, still trying" from "failed, that's final".
   */
  get stopped(): boolean {
    return this.#terminal;
  }

  #load: (() => Promise<T>) | undefined;
  #interval = REFRESH_INTERVAL;
  #timeout = LOAD_TIMEOUT;
  #idleAfter = IDLE_AFTER;
  #fatal: ((failure: unknown) => boolean) | undefined;
  #visibility: Visibility = documentVisibility;
  #random: () => number = Math.random;
  #timer: ReturnType<typeof setTimeout> | undefined;
  #unsubscribe: (() => void)[] = [];
  #inFlight = false;
  #lastActive = 0;
  #phaseOwed = 0;

  /**
   * Which subscription a result belongs to.
   *
   * Bumped by `start` and by teardown, so a response that arrives after the org
   * changed is dropped instead of rendered under the new org's heading — the
   * same hazard `o/[org]/+layout.svelte` still guards by hand after its own
   * fetch. Expressed here so a caller of `start` does not have to.
   */
  #generation = 0;

  /**
   * Poll `load` until the returned teardown is called.
   *
   * Written to be the whole body of an `$effect`, which is what ties the
   * subscription's lifetime to the page and to the org it is about:
   *
   * ```ts
   * $effect(() => {
   *   const slug = org.slug;
   *   if (!slug) return;
   *   return overview.start(() => load(slug));
   * });
   * ```
   */
  start(load: () => Promise<T>, options: PollOptions = {}): () => void {
    this.#stop();

    this.#load = load;
    this.#interval = Math.max(MIN_INTERVAL, options.interval ?? REFRESH_INTERVAL);
    this.#timeout = options.timeout ?? LOAD_TIMEOUT;
    this.#idleAfter = options.idleAfter ?? IDLE_AFTER;
    this.#fatal = options.fatal;
    this.#visibility = options.visibility ?? documentVisibility;
    this.#random = options.random ?? Math.random;
    this.#lastActive = Date.now();
    this.#drawPhase();

    const generation = this.#generation;
    this.#value = undefined;
    this.#error = undefined;
    this.#failed = false;
    this.#stale = false;
    this.#failures = 0;
    this.#updatedAt = undefined;
    this.#parked = false;
    this.#terminal = false;
    this.#loading = true;

    this.#unsubscribe.push(
      this.#visibility.subscribe(() => {
        if (generation !== this.#generation) return;
        if (this.#visibility.hidden()) this.#clearTimer();
        else void this.#run(generation);
      }),
      (options.activity ?? documentActivity).subscribe(() => {
        if (generation !== this.#generation) return;
        const wasParked = this.#parked;
        this.#lastActive = Date.now();
        // Only a parked poll needs waking; otherwise this is one of thousands
        // of keystrokes and must not re-phase the interval.
        if (wasParked) void this.#run(generation);
      })
    );

    // Rule 4: the first load ignores visibility. A backgrounded tab should have
    // something to show the moment it is looked at.
    void this.#run(generation);

    // Guarded, because an `$effect` cleanup that fired after a later `start`
    // would otherwise stop the subscription that replaced it.
    return () => {
      if (generation === this.#generation) this.#stop();
    };
  }

  /**
   * Refresh now, and restart the interval from when this one finishes.
   *
   * Answers whether it actually refreshed: `false` when one was already in
   * flight, or when there is no subscription to refresh. A caller that awaited
   * this and reported success would otherwise be reporting work that did not
   * happen.
   */
  async refresh(): Promise<boolean> {
    return await this.#run(this.#generation);
  }

  async #run(generation: number): Promise<boolean> {
    const load = this.#load;
    if (!load || this.#inFlight || generation !== this.#generation) return false;

    this.#inFlight = true;
    this.#clearTimer();

    try {
      const value = await this.#withTimeout(load);
      if (generation !== this.#generation) return false;
      this.#value = value;
      this.#updatedAt = Date.now();
      this.#error = undefined;
      this.#failed = false;
      this.#stale = false;
      // Recovery is a synchronizing event: the server that just came back
      // answered every waiting tab at once. Draw a new phase so they do not
      // stay in the instant it put them in.
      if (this.#failures > 0) this.#drawPhase();
      this.#failures = 0;
      return true;
    } catch (failure) {
      if (generation !== this.#generation) return false;
      this.#error = failure;
      this.#failures += 1;
      // Rule 3: a fatal failure, or one with nothing already on screen, is the
      // page's whole story. Anything else is a blip with data behind it.
      if (this.#fatal?.(failure) === true) this.#terminal = true;
      if (this.#value === undefined || this.#terminal) {
        this.#value = undefined;
        this.#failed = true;
        this.#stale = false;
      } else {
        this.#stale = true;
      }
      return false;
    } finally {
      // A superseded subscription owns none of this state any more: `#stop`
      // has already cleared `#inFlight` for whoever replaced it, and setting
      // `loading` here would report the old org's fetch as the new org's.
      if (generation === this.#generation) {
        this.#loading = false;
        this.#inFlight = false;
        // A fatal failure ends the subscription rather than widening its
        // interval: there is nothing on the other end of a retry, and a page
        // quietly re-asking with a dead credential is the fail-open direction.
        if (this.#terminal) this.#stop();
        else this.#schedule(generation);
      }
    }
  }

  /**
   * A load that never settles is a failure, not a pause.
   *
   * Without this, a `fetch` hung on a dead socket (a laptop resumed from sleep,
   * a captive portal) leaves `#inFlight` true with no timer armed: the poll is
   * dead, and every visible field still says the page is healthy.
   */
  async #withTimeout(load: () => Promise<T>): Promise<T> {
    let timer: ReturnType<typeof setTimeout> | undefined;
    try {
      return await Promise.race([
        load(),
        new Promise<never>((_, reject) => {
          timer = setTimeout(() => reject(new PollTimeout(this.#timeout)), this.#timeout);
        })
      ]);
    } finally {
      if (timer !== undefined) clearTimeout(timer);
    }
  }

  #schedule(generation: number): void {
    this.#clearTimer();
    if (this.#visibility.hidden()) return;

    // Rule 6. Parked rather than stopped: the activity subscription is still
    // live, and the next keystroke or click starts it again.
    if (Date.now() - this.#lastActive >= this.#idleAfter) {
      this.#parked = true;
      return;
    }
    this.#parked = false;

    this.#timer = setTimeout(() => void this.#run(generation), this.#delay());
  }

  /**
   * The gap before the next tick.
   *
   * A healthy poll runs at exactly the interval — "every 30 seconds" should mean
   * that, and a tab whose period wanders by ±15% a tick has a refresh rate
   * nobody can state. Randomness is spent on *phase* instead, once per
   * subscription and once more whenever a failing poll recovers, so the period
   * stays exact while two tabs do not.
   *
   * That second case is the one worth explaining, because it is not the one
   * jitter is usually reached for. An outage knocks every open tab over at the
   * same instant, and the failure path's jitter spreads their retries — but a
   * server coming back answers the backlog together and re-synchronizes them
   * all over again. With an exact period and nothing else, that lockstep would
   * be permanent: every console tab hitting the same endpoints in the same
   * millisecond, forever, against the instance that just recovered. Re-drawing
   * the phase on recovery is what makes it transient instead.
   */
  #delay(): number {
    if (this.#failures === 0) {
      const phase = this.#phaseOwed;
      this.#phaseOwed = 0;
      return this.#interval + phase;
    }
    const backoff = Math.min(2 ** this.#failures, MAX_BACKOFF);
    return Math.round(this.#interval * backoff * (0.85 + this.#random() * 0.3));
  }

  /** A one-off offset added to the next healthy gap. See `#delay`. */
  #drawPhase(): void {
    this.#phaseOwed = Math.round(this.#interval * this.#random() * PHASE_SPREAD);
  }

  #clearTimer(): void {
    if (this.#timer !== undefined) clearTimeout(this.#timer);
    this.#timer = undefined;
  }

  #stop(): void {
    this.#clearTimer();
    for (const unsubscribe of this.#unsubscribe) unsubscribe();
    this.#unsubscribe = [];
    // A stopped poller has nothing to refresh. Without this, `refresh()` — which
    // by definition passes the current generation — would run the *previous*
    // org's loader, write its result, and arm a timer that the teardown already
    // returned to the caller can no longer stop.
    this.#load = undefined;
    // Anything in flight belongs to the generation being abandoned; bumping
    // here is what makes its result unusable rather than merely unwanted.
    this.#generation += 1;
    this.#inFlight = false;
    this.#loading = false;
  }
}
