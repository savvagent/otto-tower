<script lang="ts">
  import { goto } from '$app/navigation';
  import { page } from '$app/state';
  import { m } from '$lib/paraglide/messages';
  import { session } from '$lib/session.svelte';
  import Alert from '$lib/components/Alert.svelte';
  import Button from '$lib/components/Button.svelte';
  import Loading from '$lib/components/Loading.svelte';

  /**
   * `/` is Savvagent Tower's public front page.
   *
   * A signed-out visitor reads it — per `+layout.svelte`'s `UNGATED` list, it
   * is the one page a stranger can reach without a session. A signed-in
   * visitor is moved along: everything the console shows — the queue, repos,
   * trackers — is scoped to the one org the session is signed in to.
   *
   * It is also where a failed sign-in lands (`/?login_error=<code>`, from
   * `/auth/callback`). The code is one of a fixed set the server chooses; an
   * unknown one falls back to the generic message rather than being shown.
   */
  $effect(() => {
    if (!session.ready || !session.info) return;
    void goto(`/o/${session.info.org.slug}`, { replaceState: true });
  });

  const LOGIN_ERRORS: Record<string, () => string> = {
    access_denied: () => m.login_error_access_denied(),
    invalid_state: () => m.login_error_invalid_state(),
    platform_unavailable: () => m.login_error_platform_unavailable(),
    failed: () => m.login_error_failed()
  };

  const loginError = $derived.by(() => {
    const code = page.url.searchParams.get('login_error');
    if (!code) return undefined;
    const render = LOGIN_ERRORS[code] ?? LOGIN_ERRORS['failed'];
    return render ? render() : undefined;
  });

  const devPoints = [m.landing_dev_1, m.landing_dev_2, m.landing_dev_3, m.landing_dev_4];
  const teamPoints = [m.landing_team_1, m.landing_team_2, m.landing_team_3, m.landing_team_4];
  const leadershipPoints = [
    m.landing_leadership_1,
    m.landing_leadership_2,
    m.landing_leadership_3,
    m.landing_leadership_4
  ];
  const honestPoints = [
    m.landing_honest_1,
    m.landing_honest_2,
    m.landing_honest_3,
    m.landing_honest_4
  ];
</script>

<svelte:head><title>{m.landing_page_title()}</title></svelte:head>

{#if !session.ready || session.signedIn}
  <Loading what={m.home_finding_org()} />
{:else}
  <div class="space-y-20 pt-6 pb-12">
    {#if loginError}
      <div class="mx-auto max-w-3xl"><Alert>{loginError}</Alert></div>
    {/if}

    <section class="mx-auto max-w-3xl text-center">
      <p class="of-label text-accent">{m.landing_eyebrow()}</p>
      <h1 class="mt-3 text-4xl font-semibold tracking-tight text-ink sm:text-5xl">
        {m.landing_h1()}
      </h1>
      <p class="mx-auto mt-5 max-w-2xl text-base text-muted sm:text-lg">
        {m.landing_subhead()}
      </p>
      <div class="mt-8 flex flex-wrap items-center justify-center gap-3">
        <a href="/auth/login" data-sveltekit-reload>
          <Button>{m.landing_cta_primary()}</Button>
        </a>
        <a
          href="#how-it-works"
          class="rounded-md border border-edge px-3 py-2 text-sm font-medium text-muted transition hover:bg-raised hover:text-ink"
        >
          {m.landing_cta_secondary()}
        </a>
      </div>
    </section>

    <section>
      <h2 class="text-center text-2xl font-semibold text-ink">{m.landing_value_heading()}</h2>
      <div class="mt-8 grid gap-4 sm:grid-cols-3">
        <div class="of-card p-5">
          <h3 class="text-sm font-semibold text-accent">{m.landing_dev_heading()}</h3>
          <ul class="mt-3 space-y-3 text-sm text-muted">
            {#each devPoints as point (point)}
              <li class="flex gap-2.5">
                <span aria-hidden="true" class="mt-2 size-1.5 shrink-0 rounded-full bg-accent"
                ></span>
                <span>{point()}</span>
              </li>
            {/each}
          </ul>
        </div>

        <div class="of-card p-5">
          <h3 class="text-sm font-semibold text-accent">{m.landing_team_heading()}</h3>
          <ul class="mt-3 space-y-3 text-sm text-muted">
            {#each teamPoints as point (point)}
              <li class="flex gap-2.5">
                <span aria-hidden="true" class="mt-2 size-1.5 shrink-0 rounded-full bg-accent"
                ></span>
                <span>{point()}</span>
              </li>
            {/each}
          </ul>
        </div>

        <div class="of-card p-5">
          <h3 class="text-sm font-semibold text-accent">{m.landing_leadership_heading()}</h3>
          <ul class="mt-3 space-y-3 text-sm text-muted">
            {#each leadershipPoints as point (point)}
              <li class="flex gap-2.5">
                <span aria-hidden="true" class="mt-2 size-1.5 shrink-0 rounded-full bg-accent"
                ></span>
                <span>{point()}</span>
              </li>
            {/each}
          </ul>
        </div>
      </div>
    </section>

    <section id="how-it-works" class="scroll-mt-20">
      <h2 class="text-center text-2xl font-semibold text-ink">{m.landing_how_heading()}</h2>
      <ol class="mt-8 grid gap-6 sm:grid-cols-3">
        <li class="of-card p-5">
          <div
            class="flex size-8 items-center justify-center rounded-full bg-raised text-sm font-semibold text-accent"
          >
            1
          </div>
          <h3 class="mt-3 text-sm font-semibold text-ink">{m.landing_how_1_title()}</h3>
          <p class="mt-1.5 text-sm text-muted">{m.landing_how_1_body()}</p>
        </li>
        <li class="of-card p-5">
          <div
            class="flex size-8 items-center justify-center rounded-full bg-raised text-sm font-semibold text-accent"
          >
            2
          </div>
          <h3 class="mt-3 text-sm font-semibold text-ink">{m.landing_how_2_title()}</h3>
          <p class="mt-1.5 text-sm text-muted">{m.landing_how_2_body()}</p>
          <!-- A wire value, not prose — never translated, same as the rest of this console's commands and paths. -->
          <code
            class="of-mono mt-3 block overflow-x-auto rounded-md border border-edge bg-canvas px-3 py-2 text-xs whitespace-pre"
            >claude mcp add --transport http savvagent-tower https://mcp.your-domain.com/mcp</code
          >
        </li>
        <li class="of-card p-5">
          <div
            class="flex size-8 items-center justify-center rounded-full bg-raised text-sm font-semibold text-accent"
          >
            3
          </div>
          <h3 class="mt-3 text-sm font-semibold text-ink">{m.landing_how_3_title()}</h3>
          <p class="mt-1.5 text-sm text-muted">{m.landing_how_3_body()}</p>
        </li>
      </ol>
    </section>

    <section class="of-card mx-auto max-w-3xl p-6">
      <h2 class="text-lg font-semibold text-ink">{m.landing_honest_heading()}</h2>
      <p class="mt-1 text-sm text-faint">{m.landing_honest_intro()}</p>
      <ul class="mt-4 space-y-2.5 text-sm text-muted">
        {#each honestPoints as point (point)}
          <li class="flex gap-2.5">
            <span aria-hidden="true" class="mt-2 size-1.5 shrink-0 rounded-full bg-faint"></span>
            <span>{point()}</span>
          </li>
        {/each}
      </ul>
    </section>

    <section class="mx-auto max-w-xl text-center">
      <h2 class="text-2xl font-semibold text-ink">{m.landing_final_heading()}</h2>
      <p class="mt-2 text-sm text-muted">{m.landing_final_body()}</p>
      <div class="mt-6">
        <a href="/auth/login" data-sveltekit-reload>
          <Button>{m.landing_cta_primary()}</Button>
        </a>
      </div>
    </section>
  </div>
{/if}
