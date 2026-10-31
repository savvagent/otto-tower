<script lang="ts">
  import { page } from '$app/state';

  import { api, ApiError } from '$lib/api';
  import { messageFor } from '$lib/errors';
  import { m } from '$lib/paraglide/messages';
  import { useOrg } from '$lib/org.svelte';
  import { absolute, relative } from '$lib/format';
  import { isClaimStranded } from '$lib/jobs';
  import type { JobDetail, Repo } from '$lib/types';
  import Alert from '$lib/components/Alert.svelte';
  import Card from '$lib/components/Card.svelte';
  import Loading from '$lib/components/Loading.svelte';
  import StatusPill from '$lib/components/StatusPill.svelte';

  /**
   * One job.
   *
   * `metadata` is rendered as raw JSON on purpose. Savvagent Tower never interprets
   * it — it is where a customer's own skill keeps whatever its methodology needs
   * — so any attempt to lay it out prettily would be the console inventing a
   * schema the server promised not to have. Showing it verbatim is the honest
   * rendering of an opaque field.
   */

  const org = useOrg();
  const id = $derived(page.params.job ?? '');

  let job = $state<JobDetail | undefined>(undefined);
  let repo = $state<Repo | undefined>(undefined);
  let loading = $state(true);
  let missing = $state(false);
  let error = $state<string | undefined>(undefined);

  $effect(() => {
    const slug = org.slug;
    const wanted = id;
    if (!slug || !wanted) return;

    loading = true;
    missing = false;
    error = undefined;

    void (async () => {
      try {
        const found = await api.job(slug, wanted);
        if (org.slug !== slug || id !== wanted) return;
        job = found;

        // The job carries a repo *id*; every link and label in this console
        // uses slugs, so the repo list is what turns one into the other.
        const repos = await api.repos(slug, true);
        repo = repos.find((r) => r.id === found.repoId);
      } catch (e) {
        if (e instanceof ApiError && e.isNotFound) missing = true;
        else error = messageFor(e, m.job_load_failed());
      } finally {
        loading = false;
      }
    })();
  });

  const metadata = $derived(
    job && Object.keys(job.metadata ?? {}).length > 0
      ? JSON.stringify(job.metadata, null, 2)
      : undefined
  );
</script>

<div class="space-y-5">
  <p class="text-xs">
    <a class="text-muted underline hover:text-ink" href="/o/{org.slug}/queue">{m.job_back()}</a>
  </p>

  {#if missing}
    <div class="py-10 text-center">
      <h1 class="text-lg font-semibold">{m.job_missing_title()}</h1>
      <!--
        The org name and the id sit inside the sentence rather than either side
        of a `<code>`: where they fall in it is a fact about the language.
      -->
      <p class="mt-2 text-sm text-faint">{m.job_missing_body({ org: org.title, id })}</p>
    </div>
  {:else if error}
    <Alert>{error}</Alert>
  {:else if !job}
    <Loading what={m.job_loading({ id })} />
  {:else}
    <div>
      <div class="flex flex-wrap items-center gap-3">
        <h1 class="text-lg font-semibold">{job.title}</h1>
        <StatusPill status={job.status} />
      </div>
      <p class="of-mono mt-1 text-xs text-faint">{job.id}</p>
      {#if (job.status === 'in-progress' || job.status === 'active') && job.cancelRequestedAt}
        <p class="mt-1 text-xs font-medium text-bad">{m.job_cancellation_requested()}</p>
      {/if}
      {#if isClaimStranded(job)}
        <p class="mt-1 text-xs font-medium text-bad">{m.job_claim_stranded()}</p>
      {/if}
    </div>

    {#if job.description}
      <Card title={m.job_description_title()}>
        <p class="text-sm whitespace-pre-wrap text-muted">{job.description}</p>
      </Card>
    {/if}

    {#if (job.status === 'failed' || job.status === 'cancelled') && job.error}
      <Alert>{job.error}</Alert>
    {/if}

    {#if job.status === 'completed' && job.result}
      <Card title={m.job_result_title()}>
        <p class="text-sm whitespace-pre-wrap text-muted">{job.result}</p>
      </Card>
    {/if}

    <Card title={m.job_details_title()}>
      <dl class="grid grid-cols-1 gap-x-8 gap-y-3 text-sm sm:grid-cols-2">
        <div>
          <dt class="of-label">{m.job_field_repo()}</dt>
          <dd>
            {#if repo}
              <a
                class="of-mono text-muted underline hover:text-ink"
                href="/o/{org.slug}/queue?repo={encodeURIComponent(repo.slug)}"
              >
                {repo.slug}
              </a>
            {:else}
              <span class="text-faint">—</span>
            {/if}
          </dd>
        </div>
        <div>
          <dt class="of-label">{m.job_field_ticket()}</dt>
          <dd class="text-muted">{job.ticketRef ?? '—'}{job.tracker ? ` (${job.tracker})` : ''}</dd>
        </div>
        <div>
          <dt class="of-label">{m.job_field_claimed_by()}</dt>
          <dd class="text-muted">{job.claimedByLabel ?? '—'}</dd>
        </div>
        <div>
          <dt class="of-label">{m.job_field_agent_type()}</dt>
          <dd class="text-muted">{job.agentType ?? m.job_agent_type_any()}</dd>
        </div>
        <div>
          <dt class="of-label">{m.job_field_queued()}</dt>
          <dd class="text-muted" title={absolute(job.createdAt)}>{relative(job.createdAt)}</dd>
        </div>
        <div>
          <dt class="of-label">{m.job_field_started()}</dt>
          <dd class="text-muted" title={absolute(job.startedAt)}>{relative(job.startedAt)}</dd>
        </div>
        <div>
          <dt class="of-label">{m.job_field_finished()}</dt>
          <dd class="text-muted" title={absolute(job.completedAt)}>{relative(job.completedAt)}</dd>
        </div>
        <div>
          <dt class="of-label">{m.job_field_attempts()}</dt>
          <dd class="text-muted">{job.attempts}</dd>
        </div>
        {#if job.cancelReason}
          <div>
            <dt class="of-label">{m.job_cancellation_requested()}</dt>
            <dd class="text-muted">{job.cancelReason}</dd>
          </div>
        {/if}
      </dl>
    </Card>

    {#if job.dependsOn.length > 0}
      <Card title={m.job_waiting_on_title()} description={m.job_waiting_on_description()}>
        <ul class="flex flex-wrap gap-2">
          {#each job.dependsOn as dependency (dependency)}
            <a
              class="of-mono rounded-md border border-edge px-2 py-1 text-xs text-muted transition hover:bg-raised hover:text-ink"
              href="/o/{org.slug}/queue/{dependency}"
            >
              {dependency}
            </a>
          {/each}
        </ul>
      </Card>
    {/if}

    {#if metadata}
      <Card title={m.job_metadata_title()} description={m.job_metadata_description()}>
        <pre class="of-mono overflow-x-auto whitespace-pre text-muted">{metadata}</pre>
      </Card>
    {/if}
  {/if}
</div>
