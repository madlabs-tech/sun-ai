<script lang="ts">
  import { Badge, Button, Icon } from '@hryer/madlabs-design-system/svelte';
  import PageHeader from '../lib/ui/PageHeader.svelte';
  import { app, approve, reject } from '../lib/app_state.svelte';
  import { money, needs_approval } from '../lib/format';
  import { href } from '../lib/router.svelte';
  import { api } from '../lib/api/client';
  import type { MemoryItem } from '../lib/api/types';
  import MemoryCard from '../lib/ui/MemoryCard.svelte';

  const waiting = $derived(app.tasks.filter(needs_approval));

  // Memory proposals wait here too; advisory, so an older kernel just shows none.
  let proposals = $state<MemoryItem[]>([]);
  const load_proposals = () =>
    api.memory().then((items) => (proposals = items.filter((m) => m.status === 'proposed')), () => {});
  load_proposals();
</script>

<PageHeader title="Approvals" subtitle="Gates passed. Only you can close these." />

<div class="sun-page">
  {#if app.error}
    <div class="sun-empty" role="alert">{app.error}</div>
  {/if}
  {#each waiting as t (t.id)}
    <article class="sun-row">
      <span class="sun-tile" style="background:var(--amber-400);color:var(--on-amber)"><Icon name="user-check" size={18} /></span>
      <div class="sun-row__body">
        <div class="sun-row__title">
          <span class="ml-mono" style="font-size:12px;color:var(--fg-2)">{t.key}</span>
          <a href={href('tasks', t.key)}>{t.title}</a>
        </div>
        <div class="sun-row__meta">
          {t.workflow?.name ?? 'No workflow'} · <span class="ml-mono">{money(t.run?.cost_usd ?? 0)}</span>
        </div>
      </div>
      <div class="approve-actions">
        <Button size="sm" variant="secondary" iconLeft="x" onclick={() => reject(t)}>Reject</Button>
        <Button size="sm" variant="mint" iconLeft="check" onclick={() => approve(t)}>Approve</Button>
      </div>
    </article>
  {:else}
    {#if app.loaded && !app.error && !proposals.length}
      <div class="sun-empty"><Badge tone="success" dot>All clear</Badge><div style="margin-top:8px">Nothing is waiting on you.</div></div>
    {/if}
  {/each}
  {#if proposals.length}
    <section class="proposals" aria-labelledby="memory-head">
      <h2 class="sun-label" id="memory-head"><a href={href('memory')}>Memory proposals</a> <Badge tone="warning" count>{proposals.length}</Badge></h2>
      {#each proposals as m (m.id)}
        <MemoryCard item={m} on_change={load_proposals} />
      {/each}
    </section>
  {/if}
</div>

<style>
  .proposals {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
    margin-top: var(--space-2);
  }
  .proposals h2 {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    margin: 0;
  }
  .proposals h2 a {
    color: inherit;
    min-height: 44px;
    display: inline-flex;
    align-items: center;
  }
  .sun-row__title a {
    display: inline-flex;
    align-items: center;
    min-height: 44px;
  }
  .approve-actions {
    display: flex;
    gap: var(--space-2);
    flex: 1 1 100%;
  }
  .approve-actions > :global(*) {
    flex: 1;
  }
  @media (min-width: 640px) {
    .approve-actions {
      flex: none;
    }
  }
</style>
