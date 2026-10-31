<script lang="ts">
  import { tick } from 'svelte';
  import { groupByTag, schemaSummary, type OpenApiDocument, type TagGroup } from '$lib/openapi';
  import Alert from '$lib/components/Alert.svelte';
  import Loading from '$lib/components/Loading.svelte';

  /**
   * A human-readable rendering of `/api/openapi.json` — the raw document is
   * fetched at runtime, exactly like the MCP endpoint and grantable scopes are
   * read at runtime from `/.well-known/oauth-protected-resource` elsewhere in
   * this console. Nothing about the API is baked into this bundle.
   *
   * This route is `UNGATED` in `+layout.svelte`: it renders before
   * `session.ready` resolves and regardless of sign-in state, because the
   * document it mirrors is `Auth::Public` and untouched by session state.
   */

  let groups = $state<TagGroup[]>([]);
  let doc = $state<OpenApiDocument | undefined>(undefined);
  let loading = $state(true);
  let error = $state<string | undefined>(undefined);

  $effect(() => {
    void load();
  });

  async function load() {
    loading = true;
    error = undefined;
    try {
      const response = await fetch('/api/openapi.json');
      if (!response.ok) {
        throw new Error(`The API reference could not be loaded (status ${response.status}).`);
      }
      const parsed = (await response.json()) as OpenApiDocument;
      doc = parsed;
      groups = groupByTag(parsed);
      // The element the hash targets doesn't exist until this render commits,
      // so a hard refresh onto `#operationId` needs its own scroll — the
      // browser's native scroll-on-load has nothing to find at load time.
      // `tick()` explicitly waits for Svelte's pending DOM flush from the
      // `groups` assignment above, rather than racing it with a bare
      // microtask that only happens to be scheduled after Svelte's own.
      await tick();
      scrollToHash();
    } catch (e) {
      error = e instanceof Error ? e.message : 'The API reference could not be loaded.';
    } finally {
      loading = false;
    }
  }

  function scrollToHash() {
    if (!location.hash) return;
    // `location.hash` includes its leading `#`, which `getElementById` never
    // matches — it must be stripped before the lookup.
    document.getElementById(location.hash.slice(1))?.scrollIntoView();
  }
</script>

<svelte:head>
  <title>API reference · Savvagent Tower</title>
</svelte:head>

<div class="space-y-6">
  <div>
    <h1 class="text-lg font-semibold text-ink">API reference</h1>
    <p class="mt-1 text-sm text-faint">
      The console's REST API, rendered from the same document served at
      <code class="rounded bg-raised px-1 py-0.5 text-xs">/api/openapi.json</code>.
    </p>
  </div>

  {#if loading}
    <Loading what="Loading the API reference" />
  {:else if error}
    <Alert>
      {error}
      <button class="ml-2 underline" onclick={() => void load()}>Try again</button>
    </Alert>
  {:else}
    {#each groups as group (group.tag)}
      <section class="space-y-3">
        <h2 class="text-sm font-semibold tracking-wide text-faint uppercase">{group.tag}</h2>
        <div class="space-y-4">
          {#each group.endpoints as endpoint (endpoint.operationId)}
            <article id={endpoint.operationId} class="of-card scroll-mt-4 px-4 py-3">
              <div class="flex flex-wrap items-center gap-2">
                <span
                  class="rounded bg-raised px-1.5 py-0.5 font-mono text-xs font-semibold text-ink uppercase"
                >
                  {endpoint.method}
                </span>
                <span class="font-mono text-sm text-ink">{endpoint.path}</span>
                <span
                  class="ml-auto rounded-full border border-edge px-2 py-0.5 text-xs font-medium text-muted"
                  title="Auth level required to call this endpoint"
                >
                  {endpoint.auth}
                </span>
              </div>

              {#if endpoint.summary}
                <h3 class="mt-2 text-sm font-semibold text-ink">{endpoint.summary}</h3>
              {/if}
              {#if endpoint.description}
                <p class="mt-1 text-sm text-muted">{endpoint.description}</p>
              {/if}

              {#if endpoint.parameters.length > 0}
                <div class="mt-3">
                  <h4 class="text-xs font-semibold tracking-wide text-faint uppercase">
                    Parameters
                  </h4>
                  <table class="mt-1 w-full text-left text-xs">
                    <tbody>
                      {#each endpoint.parameters as param (param.name)}
                        <tr class="border-t border-edge/40">
                          <td class="py-1 pr-3 font-mono text-ink">{param.name}</td>
                          <td class="py-1 text-muted">{param.description}</td>
                        </tr>
                      {/each}
                    </tbody>
                  </table>
                </div>
              {/if}

              {#if endpoint.requestSchema && doc}
                <div class="mt-3">
                  <h4 class="text-xs font-semibold tracking-wide text-faint uppercase">
                    Request body — {endpoint.requestSchema}
                  </h4>
                  <table class="mt-1 w-full text-left text-xs">
                    <tbody>
                      {#each schemaSummary(doc, endpoint.requestSchema) as prop (prop.name)}
                        <tr class="border-t border-edge/40">
                          <td class="py-1 pr-3 font-mono text-ink">
                            {prop.name}{prop.required ? ' *' : ''}
                          </td>
                          <td class="py-1 pr-3 text-muted">{prop.type}</td>
                          <td class="py-1 text-muted">{prop.description ?? ''}</td>
                        </tr>
                      {/each}
                    </tbody>
                  </table>
                </div>
              {/if}

              {#if endpoint.responseSchema && doc}
                <div class="mt-3">
                  <h4 class="text-xs font-semibold tracking-wide text-faint uppercase">
                    Response body — {endpoint.responseSchema}
                  </h4>
                  <table class="mt-1 w-full text-left text-xs">
                    <tbody>
                      {#each schemaSummary(doc, endpoint.responseSchema) as prop (prop.name)}
                        <tr class="border-t border-edge/40">
                          <td class="py-1 pr-3 font-mono text-ink">
                            {prop.name}{prop.required ? ' *' : ''}
                          </td>
                          <td class="py-1 pr-3 text-muted">{prop.type}</td>
                          <td class="py-1 text-muted">{prop.description ?? ''}</td>
                        </tr>
                      {/each}
                    </tbody>
                  </table>
                </div>
              {/if}
            </article>
          {/each}
        </div>
      </section>
    {/each}
  {/if}
</div>
