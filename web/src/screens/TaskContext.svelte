<!-- Execution log, what agents get: pinned constraints (in every session's envelope and pack, so they
     survive context rotation and runtime hand-offs) and the knowledge sources with the pack preview.
     Loaded once per task, not on every task list refresh, which would reset an unsaved edit. Styles:
     TaskRunLog's shared section classes. -->
<script lang="ts">
  import { Badge, Button, Checkbox, Icon, Input, Radio } from '@hryer/madlabs-design-system/svelte';
  import IconButton from '../lib/ui/IconButton.svelte';
  import { api } from '../lib/api/client';
  import type { ContextPreview, KnowledgeSource, TaskContext } from '../lib/api/types';
  import { error_text, toast } from '../lib/app_state.svelte';
  import { money, tokens, whole } from '../lib/format';

  let { task_id, workspace_id, plain }: { task_id: string; workspace_id: string; plain: boolean } = $props();

  let sources = $state<KnowledgeSource[]>([]);
  let picked = $state<string[]>([]);
  let budget = $state('');
  /** Over budget: `block` = the step doesn't start, `trim` = docs that don't fit become pointers. */
  let over_budget = $state<'trim' | 'block'>('trim');
  let pack = $state<ContextPreview | null>(null);
  let pack_error = $state('');
  let all_items = $state(false);
  let saving_context = $state(false);
  const ITEMS_SHOWN = 5;
  /** Pinned constraints; `null` until the task's context loaded. */
  let pinned = $state<string[] | null>(null);
  let saved_context: TaskContext | null = null;

  $effect(() => {
    const [task, workspace] = [task_id, workspace_id];
    sources = [];
    pack = null;
    // Advisory, like flags: a kernel without knowledge shows no section.
    pinned = null;
    // Only the task's workspace's sources can be picked.
    Promise.all([api.list_sources(workspace).catch(() => []), api.get_context(task)]).then(([all, saved]) => {
      if (task !== task_id) return;
      // A source taken off the workspace since stays out (the kernel refuses it).
      picked = saved.sources?.filter((id) => all.some((s) => s.id === id)) ?? all.map((s) => s.id);
      budget = saved.budget_tokens?.toString() ?? '';
      over_budget = saved.block_over_budget ? 'block' : 'trim';
      sources = all;
      saved_context = saved;
      pinned = saved.pinned ?? [];
    }, () => {});
  });

  const budget_tokens = $derived(whole(budget));
  const budget_error = $derived(Number.isNaN(budget_tokens) ? 'Use a whole number of tokens.' : '');
  /** All picked = `null` (every source, including ones added later). */
  const selection = $derived<TaskContext>({
    sources: sources.every((s) => picked.includes(s.id)) ? null : picked.filter((id) => sources.some((s) => s.id === id)),
    budget_tokens,
    pinned: pinned ?? [],
    block_over_budget: over_budget === 'block',
  });

  let preview_seq = 0;
  $effect(() => {
    const [task, body] = [task_id, selection];
    if (!sources.length || budget_error) return;
    const seq = ++preview_seq;
    const timer = setTimeout(async () => {
      try {
        const next = await api.preview_context(task, body);
        if (seq === preview_seq) [pack, pack_error] = [next, ''];
      } catch (e) {
        if (seq === preview_seq) pack_error = error_text(e);
      }
    }, 300);
    return () => clearTimeout(timer);
  });

  const toggle = (source_id: string, on: boolean) => (picked = on ? [...picked, source_id] : picked.filter((id) => id !== source_id));

  async function save_context() {
    saving_context = true;
    try {
      saved_context = await api.set_context(task_id, selection);
      toast('success', 'Context saved', 'Every agent step of this task uses it from now on.');
    } catch (e) {
      toast('danger', 'Context not saved', error_text(e));
    }
    saving_context = false;
  }

  // Saved at once, on top of the last saved selection.
  let new_pin = $state('');
  let pinning = $state(false);

  async function save_pins(next: string[]) {
    if (!saved_context) return;
    pinning = true;
    try {
      saved_context = await api.set_context(task_id, { ...saved_context, pinned: next });
      pinned = saved_context.pinned ?? [];
      new_pin = '';
    } catch (e) {
      toast('danger', 'Constraint not saved', error_text(e));
    }
    pinning = false;
  }

  function pin(e: SubmitEvent) {
    e.preventDefault();
    const text = new_pin.trim();
    if (!text || !pinned) return;
    if (pinned.includes(text)) return void (new_pin = '');
    save_pins([...pinned, text]);
  }
</script>

{#if pinned}
  <section aria-labelledby="pinned-head">
    <h3 class="sun-label timeline-head" id="pinned-head">Pinned constraints{#if pinned.length}<Badge count>{pinned.length}</Badge>{/if}</h3>
    <p class="hint">Every agent session gets these{plain ? '' : ', even after a fresh session or a runtime switch'}.</p>
    {#if pinned.length}
      <ul class="pins" aria-label="Pinned constraints">
        {#each pinned as text (text)}
          <li>
            <Icon name="pin" size={14} />
            <span class="pins__text">{text}</span>
            <IconButton icon="x" label="Unpin: {text}" variant="ghost" size="sm" disabled={pinning} onclick={() => pinned && save_pins(pinned.filter((p) => p !== text))} />
          </li>
        {/each}
      </ul>
    {/if}
    <form class="pins__add" onsubmit={pin}>
      <Input label="Pin a constraint" size="sm" maxlength={500} placeholder="e.g. Never change the billing schema" bind:value={new_pin} style="flex:1" />
      <Button type="submit" size="sm" variant="secondary" iconLeft="pin" loading={pinning} disabled={!new_pin.trim() || pinned.length >= 20}>Pin</Button>
    </form>
  </section>
{/if}

{#if sources.length}
  {@const items = all_items ? (pack?.items ?? []) : (pack?.items ?? []).slice(0, ITEMS_SHOWN)}
  <section aria-labelledby="context-head">
    <h3 class="sun-label" id="context-head">Context</h3>
    <p class="hint">Knowledge sources agents may cite on this task.</p>
    <div class="context__sources" role="group" aria-label="Knowledge sources">
      {#each sources as s (s.id)}
        <Checkbox label={s.name} checked={picked.includes(s.id)} onchange={(e: Event) => toggle(s.id, (e.currentTarget as HTMLInputElement).checked)} />
      {/each}
    </div>
    {#if pack?.suggested_sources.length}
      <p class="sun-label context__subhead" id="suggested-head">Suggested sources</p>
      <ul class="context__items context__suggested" aria-labelledby="suggested-head">
        {#each pack.suggested_sources as s (s.source_id)}
          <li>
            <span class="context__label">{s.name}</span>
            <span class="ml-mono dim">{s.docs} {s.docs === 1 ? 'doc' : 'docs'}</span>
            {#if picked.includes(s.source_id)}
              <Badge tone="success">Selected</Badge>
            {:else}
              <Button size="sm" variant="secondary" iconLeft="plus" aria-label="Add {s.name}" onclick={() => toggle(s.source_id, true)}>Add</Button>
            {/if}
          </li>
        {/each}
      </ul>
    {/if}
    <div class="context__budget">
      <Input label="Token budget" type="number" inputmode="numeric" min="1" size="sm" placeholder="Step default" bind:value={budget} error={budget_error || undefined} hint={budget_error ? undefined : 'Optional cap for the context pack.'} />
    </div>
    <fieldset class="answer context__mode">
      <legend class="sun-label">Over budget</legend>
      <Radio bind:group={over_budget} value="trim" label="Trim" description="Docs that don't fit become pointers." name="over_budget" />
      <Radio bind:group={over_budget} value="block" label="Block" description="The step doesn't start." name="over_budget" />
    </fieldset>
    {#if pack_error}
      <p class="hint" role="alert">{pack_error}</p>
    {:else if pack}
      <p class="context__summary ml-mono" aria-live="polite">{pack.summary}</p>
      <p class="hint context__cost">
        {pack.cost_estimate_usd === null ? 'No cost data yet.' : `≈ ${money(pack.cost_estimate_usd)} per agent step`}
      </p>
      {#if pack.blocked}
        <div class="flag context__blocked" role="status">
          <div class="flag__line">
            <Badge tone="warning">Blocked</Badge>
            <span class="flag__message">{pack.blocked}</span>
          </div>
        </div>
      {/if}
      <ul class="context__items" aria-label="Context pack">
        {#each items as item, i (i)}
          <li>
            <span class="ml-mono dim">P{item.priority}</span>
            <span class="context__label">{item.label}</span>
            <span class="ml-mono dim">{tokens(item.tokens)}</span>
            <Badge tone={item.included ? 'success' : 'neutral'}>{item.included ? 'Included' : 'Pointer'}</Badge>
          </li>
        {/each}
      </ul>
      {#if pack.items.length > ITEMS_SHOWN}
        <Button variant="ghost" size="sm" class="sun-disclosure" iconLeft={all_items ? 'chevron-down' : 'chevron-right'} aria-expanded={all_items} onclick={() => (all_items = !all_items)}>
          {all_items ? 'Collapse the list' : `Show all ${pack.items.length}`}
        </Button>
      {/if}
    {/if}
    <div class="row" style="margin-top:10px">
      <Button size="sm" variant="secondary" iconLeft="save" loading={saving_context} disabled={!!budget_error} onclick={save_context}>Save context</Button>
    </div>
  </section>
{/if}

<style>
  .pins {
    margin: 0 0 10px;
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  .pins li {
    display: flex;
    align-items: center;
    gap: 8px;
    color: var(--fg-1, inherit);
  }
  .pins__text {
    flex: 1;
    min-width: 0;
    overflow-wrap: anywhere;
    font-size: 13px;
  }
  .pins__add {
    display: flex;
    align-items: flex-end;
    gap: 8px;
  }
  .context__sources {
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  .context__sources :global(.ml-check) {
    min-height: 40px;
  }
  .context__budget,
  .context__mode {
    margin-top: var(--space-2);
  }
  .context__subhead {
    margin: var(--space-3) 0 6px;
  }
  .context__suggested li {
    min-height: 44px;
  }
  .context__cost {
    margin: 0 0 6px;
  }
  .context__blocked {
    margin-bottom: 6px;
  }
  .context__summary {
    margin: var(--space-3) 0 6px;
    font-size: 12px;
    color: var(--fg-1);
  }
  .context__items {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  .context__items li {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    font-size: 12px;
  }
  .context__label {
    flex: 1;
    min-width: 0;
    color: var(--fg-1);
    overflow-wrap: anywhere;
  }
</style>
