<!-- One memory item as an approval card: a proposal to approve or reject, or an active
     item to retire. Nothing reaches an agent's context until someone approves it here. -->
<script lang="ts">
  import { Button, Card, Icon, Tag } from '@hryer/madlabs-design-system/svelte';
  import { api } from '../api/client';
  import type { MemoryItem } from '../api/types';
  import { error_text, toast } from '../app_state.svelte';
  import { day, safe_url } from '../format';

  let { item, on_change }: { item: MemoryItem; on_change: () => void } = $props();

  const SCOPE = { ecosystem: 'Ecosystem', repo: 'Repo', workflow: 'Workflow' };
  const PR_STATE = { pending: 'Opening a PR to', unknown: 'No answer from the code host yet on the PR to', failed: 'The PR failed to', delivered: 'PR opened to' };
  const who = (name: string | null) => (name === 'sun' ? 'Sun' : (name ?? '').replace(/^user:/, ''));

  const proposed = $derived(item.status === 'proposed');
  const scope = $derived(item.scope_name ? `${SCOPE[item.scope]} · ${item.scope_name}` : SCOPE[item.scope]);
  const kicker = $derived(
    proposed ? (item.author === 'sun' ? 'Sun suggests' : `Starred by ${who(item.author)}`) : `Approved by ${who(item.reviewed_by)} · since ${day(item.created_at)}`,
  );
  const pr_url = $derived(safe_url(item.pr?.url));
  /** The file the PR edits: AGENTS.md, CLAUDE.md, a skill or a workflow prompt. */
  const file = $derived(item.target ?? 'AGENTS.md');
  let busy = $state<'' | 'active' | 'retired'>('');

  const DONE = {
    active: ['Approved', 'Agents get it in their context from their next step.'],
    rejected: ['Rejected', 'Sun won’t propose it again.'],
    retired: ['Retired', 'Agents no longer get it.'],
  } as const;

  async function review(to: 'active' | 'retired') {
    busy = to;
    try {
      await api.review_memory(item.id, to);
      const [title, description] = DONE[to === 'retired' && proposed ? 'rejected' : to];
      toast('success', title, description);
      on_change();
    } catch (e) {
      toast('danger', 'Couldn’t save the review', error_text(e));
    }
    busy = '';
  }
</script>

<Card as="article" padding="sm" class="memory {proposed ? 'memory--proposed' : ''}" aria-label={item.body}>
  <div class="memory__head">
    <Icon name={proposed ? 'lightbulb' : 'check'} size={16} />
    <span class="memory__kicker">{kicker}</span>
    <Tag size="sm">{scope}</Tag>
  </div>
  <p class="memory__body">{item.body}</p>
  <p class="memory__reason">{item.reason}</p>
  {#if item.pr}
    <p class="memory__pr">
      <Icon name="git-pull-request" size={14} />
      {#if pr_url}<a href={pr_url} target="_blank" rel="noopener noreferrer">Review the PR to <code class="ml-mono">{file}</code></a>{:else}<span>{PR_STATE[item.pr.status]} <code class="ml-mono">{file}</code></span>{/if}
    </p>
  {/if}
  <div class="memory__actions">
    {#if proposed}
      <Button size="sm" variant="secondary" iconLeft="x" loading={busy === 'retired'} disabled={!!busy} onclick={() => review('retired')}>Reject</Button>
      <Button size="sm" variant="mint" iconLeft="check" loading={busy === 'active'} disabled={!!busy} onclick={() => review('active')}>Approve</Button>
    {:else if item.status === 'active'}
      <Button size="sm" variant="secondary" iconLeft="archive" loading={busy === 'retired'} disabled={!!busy} onclick={() => review('retired')}>Retire</Button>
    {/if}
  </div>
</Card>

<style>
  :global(.ml-card.memory) {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    min-width: 0;
  }
  :global(.ml-card.memory--proposed) {
    background: color-mix(in srgb, var(--amber-400) 10%, var(--surface-card));
    border-color: color-mix(in srgb, var(--amber-400) 40%, transparent);
  }
  .memory__head {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    flex-wrap: wrap;
    font-size: var(--size-xs);
    color: var(--fg-2);
  }
  :global(.memory--proposed) .memory__head > :global(.ml-icon) {
    color: var(--amber-400);
  }
  .memory__kicker {
    font-weight: 600;
    color: var(--fg-1);
  }
  .memory__body {
    margin: 0;
    font-size: 15px;
    line-height: 1.45;
    color: var(--fg-1);
    overflow-wrap: anywhere;
  }
  .memory__reason,
  .memory__pr {
    margin: 0;
    font-size: var(--size-xs);
    line-height: 1.45;
    color: var(--fg-2);
    overflow-wrap: anywhere;
  }
  .memory__pr {
    display: flex;
    align-items: center;
    gap: 6px;
  }
  .memory__pr a {
    color: var(--fg-1);
    min-height: 44px;
    display: inline-flex;
    align-items: center;
    gap: 0.35em;
    flex-wrap: wrap;
  }
  .memory__pr code {
    font-size: inherit;
    overflow-wrap: anywhere;
  }
  .memory__actions {
    display: flex;
    gap: var(--space-2);
    margin-top: var(--space-1);
  }
  .memory__actions > :global(*) {
    flex: 1;
  }
  @media (min-width: 640px) {
    .memory__actions {
      justify-content: flex-end;
    }
    .memory__actions > :global(*) {
      flex: none;
    }
  }
</style>
