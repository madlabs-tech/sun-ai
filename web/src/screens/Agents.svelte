<!-- Agents: the open workspace's. A task assigned to one runs its instructions on its runtime chain
     (failover order) with its skills; each runtime of the chain may pin a host and set a model and an
     effort level, from what that host's CLI reports. Ready = some runtime of the chain is logged in on an online host;
     not ready says why per runtime. Creating, editing and archiving need manage_runtimes; an edit gives
     running tasks nothing new (they keep their version). -->
<script lang="ts">
  import { Badge, Button, Dialog, Icon, Input, Tag } from '@hryer/madlabs-design-system/svelte';
  import PageHeader from '../lib/ui/PageHeader.svelte';
  import Loaded from '../lib/ui/Loaded.svelte';
  import Chips from '../lib/ui/Chips.svelte';
  import ChainSettings from '../lib/ui/ChainSettings.svelte';
  import IconButton from '../lib/ui/IconButton.svelte';
  import { api } from '../lib/api/client';
  import type { Agent, AgentFields, ChainEntry, Runtime } from '../lib/api/types';
  import { app, can, current_workspace, error_text, toast } from '../lib/app_state.svelte';
  import { chain_names, entry_name } from '../lib/format';

  const workspace = current_workspace();
  const manage = $derived(can('manage_runtimes'));
  /** The list, also kept for the assignee pickers and cards. */
  const fetch_agents = () => api.list_agents(workspace).then((list) => (app.agents = list));
  let load = $state(fetch_agents());
  /** Every (host, tool) and key-backed runtime: chain name suggestions, and each entry's hosts, models
   *  and effort levels. Plus the catalog's installed skills. */
  let runtimes = $state<Runtime[]>([]);
  let skills = $state<string[]>([]);
  api.list_runtimes(workspace).then(
    (rows) => (runtimes = rows),
    () => {},
  );
  api.list_catalog('', '', workspace).then(
    (c) => (skills = c.entries.filter((e) => e.kind === 'skill' && e.installed).map((e) => e.id)),
    () => {},
  );

  const empty_form = () => ({ name: '', description: '', instructions: '', runtime_chain: [] as ChainEntry[], skill_ids: [] as string[] });
  /** The agent being edited; `null` with `editing` open: a new one. */
  let editing = $state<Agent | null>(null);
  let open = $state(false);
  let form = $state(empty_form());
  let saving = $state(false);
  let error = $state('');
  let archiving = $state<Agent | null>(null);

  function start(a: Agent | null) {
    editing = a;
    form = a
      ? { name: a.name, description: a.description, instructions: a.instructions, runtime_chain: [...a.runtime_chain], skill_ids: [...a.skill_ids] }
      : empty_form();
    error = '';
    open = true;
  }

  async function save(e: SubmitEvent) {
    e.preventDefault();
    if (!form.name.trim()) return void (error = 'Give the agent a name.');
    if (!form.runtime_chain.length) return void (error = 'Add at least one runtime.');
    saving = true;
    error = '';
    const fields: AgentFields = { ...$state.snapshot(form), name: form.name.trim() };
    try {
      if (editing) {
        await api.update_agent(editing.id, fields);
        toast('success', `${fields.name} saved`, 'New runs use it; running tasks keep what they started with.');
      } else {
        await api.create_agent(fields, workspace);
        toast('success', `${fields.name} created`, 'Assign it to a task to start it.');
      }
      open = false;
      load = fetch_agents();
    } catch (err) {
      error = error_text(err);
    }
    saving = false;
  }

  async function archive(a: Agent) {
    saving = true;
    try {
      await api.archive_agent(a.id);
      toast('success', `${a.name} archived`, 'Its tasks keep it; new tasks can’t pick it.');
      load = fetch_agents();
    } catch (err) {
      toast('danger', `${a.name} not archived`, error_text(err));
    }
    saving = false;
    archiving = null;
  }

  const tasks = (n: number) => (n === 1 ? '1 open task' : `${n} open tasks`);
</script>

<PageHeader title="Agents" subtitle="The agents of this workspace. Assign one to a task and it works with these instructions, runtimes and skills.">
  {#if manage}<Button variant="ice" iconLeft="plus" onclick={() => start(null)}>New agent</Button>{/if}
</PageHeader>

<div class="sun-page">
  <Loaded {load} empty={manage ? 'No agents yet. Create one, then assign it to a task.' : 'No agents yet. Someone who manages runtimes can create one.'}>
    {#snippet row(a: Agent)}
      <article class="sun-row agent" aria-label={a.name}>
        <span class="sun-tile sun-tile--ice"><Icon name="bot" size={18} /></span>
        <div class="sun-row__body">
          <div class="sun-row__title">
            {a.name}
            <Badge tone={a.ready ? 'success' : 'warning'} dot>{a.ready ? 'Ready' : 'Not ready'}</Badge>
          </div>
          <div class="sun-row__meta agent__meta">
            <span class="agent__chain" aria-label="Runtimes, in failover order">
              {#each a.runtime_chain as r, i (entry_name(r))}
                {#if i > 0}<Icon name="chevron-right" size={12} />{/if}<span class="ml-mono">{entry_name(r)}</span>{#if typeof r !== 'string'}<span class="agent__settings">{[r.model, r.effort].filter(Boolean).join(' · ')}</span>{/if}
              {/each}
            </span>
            · {tasks(a.task_count)}
          </div>
          {#if a.description}<p class="agent__text">{a.description}</p>{/if}
          {#if a.skill_ids.length}
            <ul class="sun-chips agent__skills" aria-label="Skills">
              {#each a.skill_ids as s (s)}<li><Tag size="sm" icon="sparkles"><span class="ml-mono">{s}</span></Tag></li>{/each}
            </ul>
          {/if}
          {#if !a.ready && a.ready_reason}
            <p class="agent__why"><Icon name="circle-alert" size={14} /><span>{a.ready_reason}</span></p>
          {/if}
        </div>
        {#if manage}
          <div class="agent__actions">
            <IconButton icon="pencil" label="Edit {a.name}" variant="ghost" size="sm" onclick={() => start(a)} />
            <IconButton icon="archive" label="Archive {a.name}" variant="ghost" size="sm" onclick={() => (archiving = a)} />
          </div>
        {/if}
      </article>
    {/snippet}
  </Loaded>
</div>

<Dialog bind:open onclose={() => (open = false)} title={editing ? `Edit ${editing.name}` : 'New agent'}>
  <form id="agent-form" class="agent-form" onsubmit={save} novalidate>
    <Input label="Name" placeholder="e.g. Bug Fixer" maxlength={64} autocomplete="off" bind:value={form.name} />
    <Input label="Description" placeholder="What it is for" maxlength={1000} autocomplete="off" bind:value={form.description} />
    <div class="ml-field">
      <label class="ml-field__label" for="agent-instructions">Instructions</label>
      <div class="ml-control agent-form__area">
        <textarea
          id="agent-instructions"
          class="ml-control__input"
          rows="6"
          maxlength="20000"
          placeholder="What it does, what it must never do, and how it knows it is done."
          bind:value={form.instructions}
        ></textarea>
      </div>
      <div class="ml-field__hint">Optional. Without it, the agent works from the task alone.</div>
    </div>
    <Chips
      label="Runtimes, in failover order"
      placeholder="claude-code"
      suggestions={chain_names(runtimes)}
      name={entry_name}
      hint="The first ready one runs; the next takes over at a limit."
      bind:items={form.runtime_chain}
    />
    <ChainSettings {runtimes} bind:chain={form.runtime_chain} />
    <Chips label="Skills" placeholder="tdd" suggestions={skills} hint="Installed skills from the Catalog." bind:items={form.skill_ids} />
    {#if error}<p class="agent-form__error" role="alert">{error}</p>{/if}
  </form>
  {#snippet footer()}
    <Button variant="secondary" onclick={() => (open = false)}>Cancel</Button>
    <Button variant="ice" type="submit" form="agent-form" loading={saving}>{editing ? 'Save' : 'Create agent'}</Button>
  {/snippet}
</Dialog>

{#if archiving}
  {@const a = archiving}
  <Dialog open onclose={() => (archiving = null)} title="Archive {a.name}?">
    <p class="agent__confirm">
      {a.task_count ? `Its ${tasks(a.task_count)} keep it and go on.` : 'It has no open tasks.'} New tasks can’t pick it, and its name is free again.
    </p>
    {#snippet footer()}
      <Button variant="secondary" onclick={() => (archiving = null)}>Cancel</Button>
      <Button variant="danger" loading={saving} onclick={() => archive(a)}>Archive</Button>
    {/snippet}
  </Dialog>
{/if}

<style>
  .agent__meta {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: var(--space-1);
    overflow-wrap: anywhere;
  }
  .agent__chain {
    display: inline-flex;
    flex-wrap: wrap;
    align-items: center;
    gap: var(--space-1);
  }
  .agent__settings {
    font-size: var(--size-xs);
    color: var(--fg-2);
  }
  .agent__text,
  .agent__why,
  .agent__confirm {
    margin: var(--space-2) 0 0;
    font-size: var(--size-xs);
    line-height: 1.5;
    color: var(--fg-2);
    overflow-wrap: anywhere;
  }
  .agent__confirm {
    margin: 0;
    font-size: 14px;
    color: var(--fg-1);
  }
  .agent__why {
    display: flex;
    gap: var(--space-2);
    align-items: flex-start;
  }
  .agent__why :global(.ml-icon) {
    flex: none;
    margin-top: 2px;
    color: var(--warning-fg);
  }
  .agent__skills {
    list-style: none;
    margin: var(--space-2) 0 0;
    padding: 0;
  }
  .agent__actions {
    display: flex;
    gap: var(--space-1);
    align-self: flex-start;
  }
  .agent-form {
    display: grid;
    gap: var(--space-4);
    min-width: 0;
  }
  .agent-form__area {
    height: auto;
    padding: 10px 14px;
    align-items: stretch;
  }
  .agent-form__area textarea {
    resize: vertical;
    min-height: 6em;
    line-height: 1.5;
  }
  .agent-form__error {
    margin: 0;
    font-size: 14px;
    line-height: 1.5;
    color: var(--danger-fg);
  }
</style>
