<!-- A knowledge source's docs (#/knowledge/<source>), or one doc's chunks as agents get them,
     each with the citation they quote (#/knowledge/<source>/<doc>). -->
<script lang="ts">
  import { Button, Icon } from '@hryer/madlabs-design-system/svelte';
  import PageHeader from '../lib/ui/PageHeader.svelte';
  import { api } from '../lib/api/client';
  import { error_text } from '../lib/app_state.svelte';
  import { day, safe_url } from '../lib/format';
  import { href } from '../lib/router.svelte';

  let { source_id, doc_id }: { source_id: string; doc_id: string | null } = $props();

  const detail = $derived(doc_id ? api.get_doc(doc_id) : null);
  const docs = $derived(
    doc_id
      ? null
      : Promise.all([api.list_sources(), api.list_docs(source_id)]).then(([sources, docs]) => ({
          name: sources.find((s) => s.id === source_id)?.name ?? 'Source',
          docs,
        })),
  );
</script>

{#snippet failed(e: unknown)}
  <PageHeader title="Knowledge" />
  <div class="sun-page"><div class="sun-empty" role="alert">{error_text(e)}</div></div>
{/snippet}

{#if detail}
  {#await detail}
    <PageHeader title="Knowledge" />
    <div class="sun-page"><div class="sun-empty" role="status">Loading…</div></div>
  {:then { doc, chunks }}
    {@const url = safe_url(doc.url)}
    <PageHeader title={doc.title} subtitle="{doc.version} · {chunks.length} chunks{doc.updated_at ? ` · updated ${day(doc.updated_at)}` : ''}">
      {#if url}<Button variant="secondary" iconLeft="external-link" href={url} target="_blank" rel="noopener noreferrer">Open the original</Button>{/if}
    </PageHeader>
    <div class="sun-page">
      <div><Button variant="ghost" iconLeft="arrow-left" href={href('knowledge', source_id)}>All docs</Button></div>
      {#each chunks as c, i (i)}
        <article class="sun-row chunk" aria-label="Chunk {i + 1}">
          <div class="sun-row__body">
            <div class="sun-row__title">{c.heading_path || doc.title}</div>
            <div class="sun-row__meta">Chunk {i + 1} · ~<span class="ml-mono">{c.token_estimate}</span> tokens</div>
            <div class="chunk__body">{c.body}</div>
            <p class="chunk__cite ml-mono"><Icon name="file-text" size={12} /><span>{c.citation}</span></p>
          </div>
        </article>
      {:else}
        <div class="sun-empty">This doc has no text to index.</div>
      {/each}
    </div>
  {:catch e}
    {@render failed(e)}
  {/await}
{:else if docs}
  {#await docs}
    <PageHeader title="Knowledge" />
    <div class="sun-page"><div class="sun-empty" role="status">Loading…</div></div>
  {:then { name, docs }}
    <PageHeader title={name} subtitle="{docs.length} {docs.length === 1 ? 'doc' : 'docs'} agents can cite" />
    <div class="sun-page">
      <div><Button variant="ghost" iconLeft="arrow-left" href={href('knowledge')}>All sources</Button></div>
      {#each docs as d (d.id)}
        {@const url = safe_url(d.url)}
        <article class="sun-row doc" aria-label={d.title}>
          <span class="sun-tile"><Icon name="file-text" size={18} /></span>
          <div class="sun-row__body">
            <div class="sun-row__title"><a class="doc__title" href={href('knowledge', source_id, d.id)}>{d.title}</a></div>
            <div class="sun-row__meta">
              <span class="ml-mono">{d.version}</span> · <span class="ml-mono">{d.chunks}</span> chunks
              {#if d.updated_at} · updated <span class="ml-mono">{day(d.updated_at)}</span>{/if}
            </div>
          </div>
          {#if url}
            <a class="doc__external" href={url} target="_blank" rel="noopener noreferrer" aria-label="Open the original of {d.title}">
              <Icon name="external-link" size={18} />
            </a>
          {/if}
        </article>
      {:else}
        <div class="sun-empty">No docs yet. Upload files or re-index the source.</div>
      {/each}
    </div>
  {:catch e}
    {@render failed(e)}
  {/await}
{/if}

<style>
  .doc {
    align-items: center;
  }
  .doc__title {
    display: inline-flex;
    align-items: center;
    min-height: 44px;
    color: var(--fg-1);
    text-decoration: underline;
    text-decoration-color: var(--border-default);
    text-underline-offset: 3px;
    overflow-wrap: anywhere;
  }
  .doc__title:hover {
    text-decoration-color: currentColor;
  }
  .doc__external {
    display: grid;
    place-items: center;
    width: 44px;
    height: 44px;
    flex: none;
    border-radius: var(--radius-md);
    color: var(--fg-2);
  }
  .doc__external:hover {
    background: var(--bg-subtle);
    color: var(--fg-1);
  }
  .chunk__body {
    margin-top: var(--space-3);
    font-size: 15px;
    line-height: 1.6;
    color: var(--fg-1);
    white-space: pre-wrap;
    overflow-wrap: anywhere;
    max-width: 72ch;
  }
  .chunk__cite {
    display: flex;
    align-items: baseline;
    gap: 6px;
    margin: var(--space-3) 0 0;
    font-size: 12px;
    color: var(--fg-2);
    overflow-wrap: anywhere;
    min-width: 0;
  }
  .chunk__cite :global(.ml-icon) {
    flex: none;
    color: var(--fg-3);
  }
</style>
