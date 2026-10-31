<script lang="ts">
  import { api } from '$lib/api';
  import { messageFor } from '$lib/errors';
  import { m } from '$lib/paraglide/messages';
  import { currentLocale } from '$lib/locale';
  import { useOrg } from '$lib/org.svelte';
  import { relative } from '$lib/format';
  import { roleLabel, statusLabel } from '$lib/labels';
  import { Poller } from '$lib/poll.svelte';
  import { fatalApiFailure } from '$lib/poll-fatal';
  import type { Job, QueueStats, Repo } from '$lib/types';
  import Alert from '$lib/components/Alert.svelte';
  import Card from '$lib/components/Card.svelte';
  import Empty from '$lib/components/Empty.svelte';
  import Loading from '$lib/components/Loading.svelte';
  import StatusPill from '$lib/components/StatusPill.svelte';

  /**
   * The overview: what the queue is doing, what it is doing it to, and how much
   * of the month's bucket that has cost.
   *
   * `blocked` gets a tile of its own even though those jobs are also counted in
   * `pending`. A queue with four pending jobs where three are waiting on a
   * dependency is not idle — it is stuck, and that is a different thing to go
   * and fix.
   *
   * It polls, because it is the page describing something that changes while
   * nobody is touching the browser and the page most likely to be left open. A
   * `Poller` rather than a `setInterval` here: what makes polling bearable is
   * six rules that are each easy to omit, and they are written down once, in
   * `poll.svelte.ts`, next to why each one exists.
   *
   * The three requests are one `Promise.all` and land as one value, so a tick
   * repaints the whole page or none of it. Three pollers, or a staggered fetch
   * to spread the load, would let the tiles and the job list come from different
   * ticks — a count beside a job that has already finished.
   *
   * Nothing polled here is billable: `tower-billing` meters MCP tool calls, and
   * these are console `GET`s.
   */

  const org = useOrg();

  interface Overview {
    stats: QueueStats;
    recent: Job[];
    repos: Repo[];
  }

  const overview = new Poller<Overview>();

  $effect(() => {
    const slug = org.slug;
    if (!slug) return;
    // Switching orgs runs this effect's cleanup, which stops the poll before
    // the next org's starts — see the generation counter in `poll.svelte.ts`.
    return overview.start(
      async () => {
        const [stats, recent, repos] = await Promise.all([
          api.queueStats(slug),
          api.jobs(slug, { limit: 8 }),
          api.repos(slug)
        ]);
        return { stats, recent, repos };
      },
      { fatal: fatalApiFailure }
    );
  });

  const stats = $derived(overview.value?.stats);
  const recent = $derived(overview.value?.recent ?? []);
  const repos = $derived(overview.value?.repos ?? []);
  const error = $derived(
    overview.failed ? messageFor(overview.error, m.overview_load_failed()) : undefined
  );

  /**
   * Why the refresh failed and how old what you are reading is.
   *
   * `error_network` rather than a generic sentence as the fallback: everything
   * that reaches here without being an `ApiError` is a timeout or a dead
   * socket, and "check your connection" is the actionable version of that.
   */
  const staleNote = $derived(
    m.overview_refresh_failed({
      reason: messageFor(overview.error, m.error_network()),
      age: relative(
        overview.updatedAt === undefined ? undefined : new Date(overview.updatedAt).toISOString()
      )
    })
  );

  const tiles = $derived(
    stats
      ? [
          // Four of these are job statuses, so they read from `statusLabel`
          // rather than from tile-specific keys: a tile that said one word and
          // the pill beside it another would look like two different things.
          // `Blocked` is not a status — it is a pending job with an unmet
          // dependency — and is the one label of its own.
          { label: statusLabel('pending'), value: stats.pending, tone: 'text-muted' },
          { label: statusLabel('in-progress'), value: stats.inProgress, tone: 'text-busy' },
          { label: statusLabel('active'), value: stats.active, tone: 'text-accent' },
          {
            label: m.overview_tile_blocked(),
            value: stats.blocked,
            tone: stats.blocked > 0 ? 'text-warn' : 'text-faint'
          },
          {
            label: statusLabel('failed'),
            value: stats.failed,
            tone: stats.failed > 0 ? 'text-bad' : 'text-faint'
          }
        ]
      : []
  );
</script>

<div class="space-y-6">
  <div class="flex flex-wrap items-start justify-between gap-2">
    <div>
      <h1 class="text-lg font-semibold">{org.title}</h1>
      <p class="mt-0.5 text-sm text-faint">
        <code class="of-mono">{org.slug}</code> · {m.overview_role_and_plan({
          role: org.role ? roleLabel(org.role) : '—',
          plan: org.org?.plan ?? '—'
        })}
      </p>
    </div>

    <!--
      Said only when there is something to say. A refresh failing on top of good
      data leaves the data on screen, so without this line the page would look
      current while quietly falling behind — and the page silently succeeding
      needs no announcement. There is deliberately no "updated 12 seconds ago"
      counterpart: keeping one honest needs a second timer at human resolution,
      which is a lot of machinery to say nothing has gone wrong.

      `role="status"` for the same reason `Alert` carries one — a warning that
      exists only visually is a warning a blind reader has to guess at.
    -->
    {#if overview.parked}
      <p role="status" class="max-w-xs text-right text-xs text-faint">{m.overview_paused()}</p>
    {:else if overview.stale}
      <p role="status" class="max-w-xs text-right text-xs text-warn">{staleNote}</p>
    {/if}
  </div>

  {#if error}
    <Alert>
      {error}
      {#if !overview.stopped}
        {m.overview_retrying()}
      {/if}
    </Alert>
  {:else if !overview.value}
    <Loading what={m.overview_loading()} />
  {:else}
    <div class="grid grid-cols-2 gap-3 sm:grid-cols-4">
      {#each tiles as tile (tile.label)}
        <div class="of-card px-4 py-3">
          <div class="text-2xl font-semibold {tile.tone}">
            {tile.value.toLocaleString(currentLocale())}
          </div>
          <div class="mt-0.5 text-xs text-faint">{tile.label}</div>
        </div>
      {/each}
    </div>

    {#if stats && stats.blocked > 0}
      <Alert tone="warn">
        {m.overview_blocked_note({ count: stats.blocked })}
        {m.overview_blocked_also_pending({ status: statusLabel('pending') })}
      </Alert>
    {/if}

    <div class="grid gap-6 lg:grid-cols-2">
      <Card title={m.overview_recent_title()} description={m.overview_recent_description()}>
        {#snippet actions()}
          <a class="text-xs text-muted underline hover:text-ink" href="/o/{org.slug}/queue">
            {m.overview_open_queue()}
          </a>
        {/snippet}

        {#if recent.length === 0}
          <Empty title={m.overview_empty_title()}>
            {m.overview_empty_hint()}
            <a class="text-muted underline hover:text-ink" href="/o/{org.slug}/connect">
              {m.overview_connect_one()}
            </a>.
          </Empty>
        {:else}
          <ul class="divide-y divide-edge/40">
            {#each recent as job (job.id)}
              <li class="flex items-center gap-3 py-2">
                <span class="min-w-24 shrink-0"><StatusPill status={job.status} /></span>
                <a
                  class="min-w-0 flex-1 truncate text-sm text-ink hover:underline"
                  href="/o/{org.slug}/queue/{job.id}"
                >
                  {job.title}
                </a>
                <span class="shrink-0 text-xs text-faint">{relative(job.createdAt)}</span>
              </li>
            {/each}
          </ul>
        {/if}
      </Card>

      <div class="space-y-6">
        <Card title={m.overview_repos_title()} description={m.overview_repos_description()}>
          {#snippet actions()}
            <a class="text-xs text-muted underline hover:text-ink" href="/o/{org.slug}/repos">
              {m.overview_repos_manage()}
            </a>
          {/snippet}

          {#if repos.length === 0}
            <Empty title={m.overview_repos_empty_title()}>
              {m.overview_repos_empty_hint()}
            </Empty>
          {:else}
            <ul class="flex flex-wrap gap-2">
              {#each repos as repo (repo.id)}
                <a
                  class="of-mono rounded-md border border-edge px-2 py-1 text-xs text-muted transition hover:bg-raised hover:text-ink"
                  href="/o/{org.slug}/queue?repo={encodeURIComponent(repo.slug)}"
                >
                  {repo.slug}
                </a>
              {/each}
            </ul>
          {/if}
        </Card>
      </div>
    </div>
  {/if}
</div>
