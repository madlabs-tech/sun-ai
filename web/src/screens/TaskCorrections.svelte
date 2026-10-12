<!-- Execution log: where a gate, a reviewer or a revert corrected the agent, by cause; each can be proposed
     as memory. Styles: TaskRunLog's shared section classes. -->
<script lang="ts">
  import { Badge, Button, Icon } from '@hryer/madlabs-design-system/svelte';
  import IconButton from '../lib/ui/IconButton.svelte';
  import type { Correction } from '../lib/api/types';
  import { CAUSE_BADGE, clock } from '../lib/format';

  let { corrections, starred, star }: { corrections: Correction[]; starred: Record<string, boolean>; star: (c: { id: string }) => void } = $props();

  let open = $state<Record<string, boolean>>({});

  const CORRECTION_KIND = {
    gate_fail: ['circle-x', 'Gate failed'],
    reject: ['square-x', 'Rejected'],
    revert: ['rotate-ccw', 'Reverted'],
    steer: ['message-square', 'Steered'],
    decision_override: ['pencil', 'Decision corrected'],
    review_comment: ['message-circle', 'Review comment'],
  } as const;
</script>

{#if corrections.length}
  <section aria-labelledby="corrections-head">
    <h3 class="sun-label timeline-head" id="corrections-head">Corrections<Badge tone="neutral" count>{corrections.length}</Badge></h3>
    <p class="hint">Where a gate, a reviewer or a revert corrected the agent, by cause.</p>
    <ul class="flags">
      {#each corrections as c (c.id)}
        {@const [icon, kind] = CORRECTION_KIND[c.kind]}
        {@const [tone, cause] = CAUSE_BADGE[c.cause_label]}
        {@const [head, ...rest] = c.body.split('\n')}
        <li class="flag correction">
          <div class="flag__line">
            <Icon name={icon} size={16} />
            <span class="correction__kind">{kind}</span>
            <Badge {tone}>{cause}</Badge>
            <time class="ml-mono dim correction__time" datetime={c.created_at}>{clock(c.created_at)}</time>
            <IconButton icon="star" label="Propose as memory" aria-pressed={!!starred[c.id]} variant="ghost" size="sm" onclick={() => star(c)} />
          </div>
          <p class="flag__message correction__body">{head}</p>
          {#if rest.length}
            <Button
              variant="ghost"
              size="sm"
              class="sun-disclosure"
              iconLeft={open[c.id] ? 'chevron-down' : 'chevron-right'}
              aria-expanded={!!open[c.id]}
              onclick={() => (open[c.id] = !open[c.id])}
            >
              {open[c.id] ? 'Hide' : 'Show'} details
            </Button>
            {#if open[c.id]}<pre class="ml-mono event__detail">{rest.join('\n')}</pre>{/if}
          {/if}
          {#if c.step_key || c.runtime}
            <p class="ml-mono dim correction__meta">{[c.step_key, c.runtime].filter(Boolean).join(' · ')}</p>
          {/if}
        </li>
      {/each}
    </ul>
  </section>
{/if}

<style>
  .correction .flag__line {
    flex-wrap: wrap;
    align-items: center;
  }
  .correction__kind {
    font-weight: 600;
    white-space: nowrap;
  }
  .correction__time {
    margin-left: auto;
    font-size: 11px;
  }
  .correction .flag__line :global(.ml-icon) {
    flex: none;
    margin-top: 2px;
    color: var(--fg-2);
  }
  .correction__body {
    display: block;
    margin: 6px 0 0;
    font-size: 13px;
    line-height: 1.45;
    color: var(--fg-1);
  }
  .correction__meta {
    margin: 4px 0 0;
    font-size: 11px;
  }
</style>
