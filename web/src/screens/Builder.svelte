<!-- Step-list workflow builder. The blocks and the YAML view edit the same file:
     `text` is what the kernel validates (live, with line:col) and what Save sends as a new version. -->
<script lang="ts">
  import { Badge, Button, Card, Icon, Input, Select, Tabs } from '@hryer/madlabs-design-system/svelte';
  import IconButton from '../lib/ui/IconButton.svelte';
  import { tick, untrack } from 'svelte';
  import PageHeader from '../lib/ui/PageHeader.svelte';
  import Chips from '../lib/ui/Chips.svelte';
  import { api } from '../lib/api/client';
  import type { Account, DryRun, WorkflowIssue } from '../lib/api/types';
  import { ON_LIMIT, money, points, points_for } from '../lib/format';
  import { can, current_workspace, error_text, toast } from '../lib/app_state.svelte';
  import { go, href } from '../lib/router.svelte';
  import {
    BLANK_WORKFLOW,
    BLOCKS,
    GATES,
    block_of,
    gate_name,
    new_step,
    step_at_line,
    step_summary,
    to_doc,
    to_yaml,
    type GateEntry,
    type StepDoc,
    type WorkflowDoc,
  } from '../lib/workflow';

  /** A workflow id, or `new`. */
  let { id }: { id: string } = $props();

  const EMPTY: WorkflowDoc = { name: '', steps: [] };
  let loaded = $state(false);
  let load_error = $state('');
  let version = $state(0);
  /** The file as last saved ('' for a new workflow, so it starts unsaved). */
  let saved = $state('');
  /** The file being edited. */
  let text = $state('');
  /** `text` parsed: what the blocks edit. */
  let doc = $state<WorkflowDoc>(EMPTY);
  /** `doc` as JSON when `text` last matched it; a block edit changes `doc` and rewrites `text`. */
  let synced = JSON.stringify(EMPTY);
  let yaml_broken = $state(false);
  let view = $state('steps');
  let settings_open = $state(false);
  let yaml_el = $state<HTMLTextAreaElement>();

  /** Parses `source` into the blocks. False when it isn't a workflow map yet. */
  function adopt(source: string): boolean {
    const next = to_doc(source);
    if (next) {
      synced = JSON.stringify(next);
      doc = next;
    }
    return !!next;
  }

  $effect(() => {
    const current = id;
    untrack(() => {
      loaded = false;
      load_error = '';
      const show = (source: string, as_saved: string, v: number) => {
        text = source;
        saved = as_saved;
        version = v;
        yaml_broken = !adopt(source);
        loaded = true;
      };
      if (current === 'new') return show(BLANK_WORKFLOW, '', 0);
      api
        .get_workflow(current)
        .then((w) => current === id && show(w.source_yaml, w.source_yaml, w.version))
        .catch((e) => (load_error = error_text(e)));
    });
  });

  // A block edit rewrites the file.
  $effect(() => {
    const now = JSON.stringify(doc);
    if (now !== synced) {
      synced = now;
      text = to_yaml(doc, untrack(() => text));
    }
  });

  function edit_yaml(value: string) {
    text = value;
    yaml_broken = !adopt(value);
  }

  // Live validation by the kernel (sun-workflow is the one validator), debounced.
  let issues = $state<WorkflowIssue[]>([]);
  /** The text `issues` belong to. */
  let checked = $state('');
  let check_error = $state('');
  $effect(() => {
    const source = text;
    if (!loaded) return;
    const timer = setTimeout(async () => {
      try {
        const result = await api.validate_workflow(source);
        if (source !== text) return;
        issues = result.errors;
        checked = source;
        check_error = '';
      } catch (e) {
        check_error = error_text(e);
      }
    }, 300);
    return () => clearTimeout(timer);
  });
  const checking = $derived(checked !== text);
  const placed = $derived(issues.map((issue) => ({ ...issue, step: step_at_line(checked, issue.line) })));

  const dirty = $derived(text !== saved);
  let saving = $state(false);
  const can_save = $derived(loaded && dirty && !checking && !issues.length && !saving);

  async function save() {
    saving = true;
    try {
      const w = await api.save_workflow(id === 'new' ? null : id, text, current_workspace());
      saved = text;
      version = w.version;
      toast('success', `Saved as version ${w.version}`, 'New tasks use it. Running tasks keep the version they started on.');
      if (id === 'new') go('workflows', w.id);
    } catch (e) {
      toast('danger', 'Not saved', error_text(e));
    }
    saving = false;
  }

  // Stable keys for step cards, so open/closed state follows a step when it moves.
  const keys = new WeakMap<object, number>();
  let next_key = 0;
  const key = (s: StepDoc) => keys.get(s) ?? (keys.set(s, ++next_key), next_key);
  let open = $state<Record<number, boolean>>({});

  async function focus(element_id: string) {
    await tick();
    const el = document.getElementById(element_id);
    el?.scrollIntoView({ block: 'nearest', behavior: 'smooth' });
    el?.focus({ preventScroll: true });
  }

  function move(i: number, by: number) {
    const steps = doc.steps;
    [steps[i], steps[i + by]] = [steps[i + by], steps[i]];
    const k = key(steps[i + by]);
    const edge = i + by === 0 || i + by === steps.length - 1;
    focus(edge ? `toggle-${k}` : `move-${by < 0 ? 'up' : 'down'}-${k}`);
  }

  function remove(i: number) {
    const [gone] = doc.steps.splice(i, 1);
    toast('info', `Removed ${gone.id}`, 'Nothing changes until you save.');
    const next = doc.steps[Math.min(i, doc.steps.length - 1)];
    focus(next ? `toggle-${key(next)}` : 'add-steps');
  }

  function add(type: string) {
    doc.steps.push(new_step(type, doc.steps.map((s) => s.id)));
    const k = key(doc.steps[doc.steps.length - 1]);
    open[k] = true;
    focus(`toggle-${k}`);
  }

  /** Jump to where an error is: its block (steps view) or its line (YAML view). */
  function reveal(p: (typeof placed)[number]) {
    if (view === 'yaml' && yaml_el) {
      const lines = text.split('\n');
      const start = lines.slice(0, p.line - 1).reduce((n, l) => n + l.length + 1, 0);
      yaml_el.focus();
      yaml_el.setSelectionRange(start, start + (lines[p.line - 1]?.length ?? 0));
      return;
    }
    const step = doc.steps[p.step];
    if (step) {
      open[key(step)] = true;
      focus(`toggle-${key(step)}`);
    } else {
      settings_open = true;
      focus('workflow-name');
    }
  }

  const set_chain = (chain: string[]) => {
    if (chain.length) doc.runtime = { ...doc.runtime, chain };
    else delete doc.runtime;
  };
  const set_budget = (field: 'wall' | 'turns' | 'cost_usd', value: unknown) => (doc.budget = { ...doc.budget, [field]: value });
  const set_timeout = (step: StepDoc, after: string, then: string) => {
    if (after) step.timeout = { after, then };
    else delete step.timeout;
  };
  const set_send_back = (step: StepDoc, to: string) => {
    if (to) step.on_fail = { send_back_to: to, max: step.on_fail?.max ?? 2 };
    else delete step.on_fail;
  };

  // Unset follows the ecosystem's policy (Settings), not `switch`.
  const LIMIT_OPTIONS = [{ value: '', label: 'Use the ecosystem default' }, ...ON_LIMIT];
  function set_on_limit(value: string) {
    if (!doc.runtime) return;
    if (value) doc.runtime.on_limit = value;
    else delete doc.runtime.on_limit;
  }
  const THEN = [
    { value: 'remind', label: 'Remind the approvers' },
    { value: 'fail', label: 'Fail the task' },
    { value: 'auto_reject', label: 'Reject it' },
  ];
  const plural = (n: number, word: string) => `${n} ${word}${n === 1 ? '' : 's'}`;

  // Dry run: what would run, in which order, what it needs and may cost. Saves nothing.
  let dry = $state<DryRun | null>(null);
  /** The text `dry` belongs to; a later edit marks it out of date. */
  let dry_for = $state('');
  let dry_running = $state(false);
  /** Sun Cloud's points rate, for the estimate in points; null off Sun Cloud. */
  let rate = $state<Account['points'] | null>(null);
  async function dry_run() {
    dry_running = true;
    const source = text;
    try {
      if (!rate) rate = await api.get_account().then((a) => (a.cloud ? a.points : null), () => null);
      dry = await api.dry_run(source);
      dry_for = source;
      focus('dry-title');
    } catch (e) {
      toast('danger', 'Dry run failed', error_text(e));
    }
    dry_running = false;
  }
  const waves = $derived(
    dry ? [...new Set(dry.steps.map((s) => s.wave))].map((w) => dry!.steps.filter((s) => s.wave === w)) : [],
  );
  /** Simulated step and run statuses: words and icons, never colour alone. */
  const SIM_STATUS: Record<string, { tone: 'success' | 'danger' | 'neutral' | 'warning'; label: string; icon: string }> = {
    succeeded: { tone: 'success', label: 'Passed', icon: 'circle-check' },
    failed: { tone: 'danger', label: 'Failed', icon: 'circle-x' },
    cancelled: { tone: 'warning', label: 'Stopped', icon: 'square-x' },
    not_reached: { tone: 'neutral', label: 'Not reached', icon: 'dot' },
  };
</script>

<PageHeader title={doc.name || 'Workflow'} subtitle={loaded ? `${version ? `Version ${version}` : 'Not saved yet'}${dirty && version ? ' · unsaved changes' : ''}` : undefined}>
  <span role="status" aria-live="polite">
    {#if !loaded}
      <Badge>Loading</Badge>
    {:else if check_error}
      <Badge tone="warning" dot>Can’t check</Badge>
    {:else if checking}
      <Badge dot>Checking…</Badge>
    {:else if issues.length}
      <Badge tone="danger" dot>{plural(issues.length, 'problem')}</Badge>
    {:else}
      <Badge tone="success" dot>Valid</Badge>
    {/if}
  </span>
  {#if can(id === 'new' ? 'create_workflow' : 'edit_workflow')}
    <Button variant="ice" iconLeft="save" disabled={!can_save} loading={saving} onclick={save}>Save</Button>
  {/if}
</PageHeader>

<div class="sun-page builder">
  <div class="builder__bar">
    <Button variant="ghost" size="sm" iconLeft="arrow-left" href={href('workflows')}>All workflows</Button>
    <span style="flex:1"></span>
    <Button variant="secondary" size="sm" iconLeft="play" disabled={!loaded || checking || !!issues.length} loading={dry_running} onclick={dry_run}>Dry run</Button>
    <Tabs bind:value={view} items={[{ id: 'steps', label: 'Steps', icon: 'list-ordered' }, { id: 'yaml', label: 'YAML', icon: 'file-code' }]} />
  </div>

  {#if load_error}
    <div class="sun-empty" role="alert">{load_error}</div>
  {:else if !loaded}
    <div class="sun-empty" role="status">Loading…</div>
  {:else}
    {#if issues.length && !checking}
      <section class="issues" aria-labelledby="issues-title">
        <h2 id="issues-title"><Icon name="triangle-alert" size={18} /> Fix {issues.length === 1 ? 'this' : 'these'} before saving</h2>
        <ul>
          {#each placed as p, n (n)}
            <li>
              <Button variant="ghost" size="sm" class="issue" onclick={() => reveal(p)}>
                <span class="ml-mono">Line {p.line}:{p.column}</span>
                {#if p.step >= 0 && doc.steps[p.step]}<span class="ml-mono">· {doc.steps[p.step].id}:</span>{/if}
                <span class="issue__message">{p.message}</span>
              </Button>
            </li>
          {/each}
        </ul>
      </section>
    {/if}

    {#if dry}
      {@const d = dry}
      {@const sim = dry.simulation}
      <Card as="section" padding="sm" class="dry" aria-labelledby="dry-title">
        <div class="dry__head">
          <h2 id="dry-title" tabindex="-1">Dry run</h2>
          {#if dry_for !== text}<Badge tone="warning">Out of date: run it again</Badge>{/if}
          <span style="flex:1"></span>
          <IconButton icon="x" label="Close the dry run" variant="ghost" size="sm" onclick={() => (dry = null)} />
        </div>
        <p class="dry__note">Nothing was saved, started or sent. Steps on the same line can run at the same time.</p>
        <ol class="dry__waves" aria-label="Run order">
          {#each waves as wave, n (n)}
            <li>
              {#each wave as s (s.id)}
                <span class="dry__step">
                  <Icon name={block_of(s.kind).icon} size={14} />
                  <span class="ml-mono">{s.id}</span>
                  {#if s.uses.length}<span class="dry__uses">{s.uses.join(', ')}</span>{/if}
                </span>
              {/each}
            </li>
          {/each}
        </ol>
        <section class="sim" aria-labelledby="sim-title">
          <div class="dry__head">
            <h3 id="sim-title">Simulated run</h3>
            <Badge tone={SIM_STATUS[sim.status]?.tone ?? 'warning'}>{SIM_STATUS[sim.status]?.label ?? sim.status}</Badge>
          </div>
          <p class="dry__note">
            Each step ran once in a sandbox with a scripted agent: no model, no commands, no messages.
            {#if sim.end_reason}<br />Stopped: {sim.end_reason}{/if}
          </p>
          <ol class="sim__steps">
            {#each sim.steps as s (s.id)}
              {@const look = SIM_STATUS[s.status] ?? { tone: 'warning', label: s.status, icon: 'dot' }}
              <li class="sim__step">
                <span class="sim__icon sim__icon--{look.tone}"><Icon name={look.icon} size={16} /></span>
                <span class="sim__body">
                  <span><span class="ml-mono">{s.id}</span> · {look.label}{s.attempts > 1 ? ` after ${s.attempts} tries` : ''}</span>
                  {#if s.note}<span class="dry__uses">{s.note}</span>{/if}
                </span>
              </li>
            {/each}
          </ol>
          {#if sim.actions.length}
            <p class="dry__note">Recorded, not sent: <span class="ml-mono">{sim.actions.join(', ')}</span></p>
          {/if}
        </section>
        <dl class="dry__facts">
          <dt>Plugins and extensions</dt>
          <dd>
            {#each d.mods as m (m.name)}
              <Badge tone={m.installed ? 'success' : 'warning'}>{m.name}{m.installed ? '' : ' · not installed'}</Badge>
            {:else}
              None
            {/each}
          </dd>
          <dt>Runtimes</dt>
          <dd>{d.runtimes.length ? d.runtimes.join(' → ') : 'The ecosystem default'}</dd>
          <dt>Estimated cost</dt>
          <dd>
            {#if d.estimated_cost_usd !== null}
              About {money(d.estimated_cost_usd)} for {plural(d.agent_steps, 'agent step')}, from past sessions
              {#if rate}<span class="dry__cap">On Sun credits: up to {points(points_for(d.estimated_cost_usd, rate))}</span>{/if}
            {:else if d.agent_steps}
              No estimate yet: no agent session has reported its cost
            {:else}
              No agent steps, so no model cost
            {/if}
            {#if d.budget_usd !== null}<span class="dry__cap">Capped at {money(d.budget_usd)}</span>{/if}
          </dd>
        </dl>
      </Card>
    {/if}

    {#if view === 'yaml'}
      <div class="ml-field">
        <label class="ml-field__label" for="workflow-yaml">Workflow file</label>
        <div class="ml-control ml-control--mono area" class:ml-control--error={yaml_broken}>
          <textarea id="workflow-yaml" bind:this={yaml_el} class="ml-control__input" spellcheck="false" rows="24" bind:value={() => text, edit_yaml}></textarea>
        </div>
        <div class="ml-field__hint">The same workflow as the steps: a change here shows there, and the other way round. Comments stay when you edit a block.</div>
      </div>
    {:else if yaml_broken}
      <div class="sun-empty" role="alert">The file isn’t a workflow the blocks can show yet. Fix it in the YAML view.</div>
    {:else}
      <details class="settings" bind:open={settings_open}>
        <summary>
          <span class="sun-tile"><Icon name="settings" size={18} /></span>
          <span class="settings__title">Workflow settings<span class="settings__meta">Name, start, runtimes and limits</span></span>
          <Icon name="chevron-down" size={18} class="settings__chevron" />
        </summary>
        <div class="fields">
          <Input id="workflow-name" label="Name" mono hint="Lowercase letters, digits, - and _." bind:value={doc.name} />
          <Chips label="Starts on" placeholder="manual" hint="manual, webhook, cron:<schedule> or a plugin event" bind:items={() => doc.on ?? [], (on) => (doc.on = on)} />
          <Chips label="Runtimes, in failover order" placeholder="claude-code" hint="Empty uses the ecosystem default." bind:items={() => doc.runtime?.chain ?? [], set_chain} />
          <Select label="When a runtime hits its limit" options={LIMIT_OPTIONS} disabled={!doc.runtime} bind:value={() => doc.runtime?.on_limit ?? '', set_on_limit} />
          <Input label="Time limit" placeholder="e.g. 2h" bind:value={() => doc.budget?.wall ?? '', (v) => set_budget('wall', v)} />
          <Input label="Max turns" type="number" inputmode="numeric" min="1" bind:value={() => String(doc.budget?.turns ?? ''), (v) => set_budget('turns', v)} />
          <Input label="Max cost (USD)" type="number" inputmode="decimal" min="0" step="0.5" bind:value={() => String(doc.budget?.cost_usd ?? ''), (v) => set_budget('cost_usd', v)} />
        </div>
        {#each placed.filter((p) => p.step < 0) as p, n (n)}
          <p class="error">Line {p.line}:{p.column} · {p.message}</p>
        {/each}
      </details>

      <section aria-labelledby="steps-title">
        <h2 class="sun-label" id="steps-title">Steps, in order</h2>
        <ol class="blocks">
          {#each doc.steps as step, i (key(step))}
            {@const k = key(step)}
            {@const block = block_of(step.type)}
            {@const errors = placed.filter((p) => p.step === i)}
            <Card as="li" padding="none" class="block {errors.length ? 'block--error' : ''}">
              <div class="block__head">
                <span class="sun-tile" class:sun-tile--ice={open[k]}><Icon name={block.icon} size={18} /></span>
                <Button variant="ghost" class="block__toggle" id="toggle-{k}" aria-expanded={!!open[k]} aria-controls="body-{k}" onclick={() => (open[k] = !open[k])}>
                  <span class="block__title"><span class="ml-mono">{step.id || 'no id'}</span><span class="block__type">{block.label}</span></span>
                  <span class="block__summary">{step_summary(step)}</span>
                </Button>
                <IconButton id="move-up-{k}" icon="arrow-up" label="Move {step.id} up" variant="ghost" disabled={i === 0} onclick={() => move(i, -1)} />
                <IconButton id="move-down-{k}" icon="arrow-down" label="Move {step.id} down" variant="ghost" disabled={i === doc.steps.length - 1} onclick={() => move(i, 1)} />
              </div>
              {#each errors as p, n (n)}
                <p class="error">Line {p.line}:{p.column} · {p.message}</p>
              {/each}
              {#if open[k]}
                <div class="fields" id="body-{k}">
                  <Input label="Step id" mono hint="Lowercase letters, digits, - and _." bind:value={step.id} />
                  {#if step.type === 'agent'}
                    <div class="ml-field wide">
                      <label class="ml-field__label" for="prompt-{k}">Instructions</label>
                      <div class="ml-control area"><textarea id="prompt-{k}" class="ml-control__input" rows="4" placeholder="What should the agent do? Or a prompt file, e.g. prompts/fix.md" bind:value={step.prompt}></textarea></div>
                    </div>
                    <Chips label="Skills" placeholder="tdd" bind:items={() => step.skills ?? [], (v) => (step.skills = v)} />
                  {:else if step.type === 'gate'}
                    <div class="wide">
                      <Chips label="Checks" placeholder="test" suggestions={GATES} hint="Checks with settings (like a file name) are edited in the YAML view." name={gate_name} bind:items={() => step.gates ?? [], (g: GateEntry[]) => (step.gates = g)} />
                    </div>
                    <Select label="If a check fails" bind:value={() => step.on_fail?.send_back_to ?? '', (v) => set_send_back(step, v)}
                      options={[{ value: '', label: 'Fail the step' }, ...[...new Set([...doc.steps.filter((s) => s !== step).map((s) => s.id), step.on_fail?.send_back_to ?? ''])].filter(Boolean).map((s) => ({ value: s, label: `Send back to ${s}` }))]} />
                    {#if step.on_fail}
                      <Input label="Send back at most" type="number" inputmode="numeric" min="0" hint="times" bind:value={() => String(step.on_fail?.max ?? ''), (v) => step.on_fail && (step.on_fail.max = Number(v))} />
                    {/if}
                  {:else if step.type === 'approval'}
                    <Chips label="Who can approve" placeholder="role:admin" hint="role:<name> or user:<name>" bind:items={() => step.who ?? [], (v) => (step.who = v)} />
                    <Input label="Time limit" placeholder="e.g. 24h, empty waits forever" bind:value={() => step.timeout?.after ?? '', (v) => set_timeout(step, v, step.timeout?.then ?? 'remind')} />
                    <Select label="When time runs out" options={THEN} disabled={!step.timeout} bind:value={() => step.timeout?.then ?? 'remind', (v) => set_timeout(step, step.timeout?.after ?? '', v)} />
                  {:else if step.type === 'wait'}
                    <Input label="Wait for" placeholder="e.g. 30m, 2h, 1d" bind:value={step.for} />
                  {:else if step.type === 'notify'}
                    <Chips label="Send to" placeholder="slack:#team" bind:items={() => step.to ?? [], (v) => (step.to = v)} />
                    <div class="ml-field wide">
                      <label class="ml-field__label" for="message-{k}">Message</label>
                      <div class="ml-control area"><textarea id="message-{k}" class="ml-control__input" rows="3" bind:value={step.message}></textarea></div>
                    </div>
                  {:else if step.type === 'action'}
                    <Input label="Plugin action" mono placeholder="e.g. github.open_pr" bind:value={step.use} />
                  {:else}
                    <Input label="Extension step" mono placeholder="e.g. browser.screenshot" bind:value={step.type} />
                  {/if}
                  {#if step.with !== undefined || step.context !== undefined || step.needs !== undefined}
                    <p class="ml-field__hint wide">This step also has
                      {[step.with !== undefined && 'inputs (with)', step.context !== undefined && 'context', step.needs !== undefined && 'needs'].filter(Boolean).join(', ')};
                      edit those in the YAML view.</p>
                  {/if}
                  <div class="wide">
                    <Button variant="ghost" size="sm" iconLeft="trash-2" onclick={() => remove(i)}>Remove step</Button>
                  </div>
                </div>
              {/if}
            </Card>
          {:else}
            <li class="sun-empty">No steps yet. Add the first one below.</li>
          {/each}
        </ol>
      </section>

      <section aria-labelledby="add-steps">
        <h2 class="sun-label" id="add-steps" tabindex="-1">Add a step</h2>
        <div class="palette">
          {#each BLOCKS as b (b.type)}
            <Card as="button" variant="outline" padding="sm" interactive class="palette__item" onclick={() => add(b.type)}>
              <span class="sun-tile"><Icon name={b.icon} size={18} /></span>
              <span><span class="palette__label">{b.label}</span><span class="palette__hint">{b.hint}</span></span>
            </Card>
          {/each}
        </div>
      </section>
    {/if}
  {/if}
</div>

<style>
  .builder__bar {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    flex-wrap: wrap;
  }
  h2 {
    margin: 0;
  }
  .sun-label {
    margin-bottom: 10px;
  }
  ol,
  ul {
    list-style: none;
    margin: 0;
    padding: 0;
  }
  :global(.ml-card.dry) {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
  }
  .dry__head {
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: var(--space-2);
  }
  .dry__head h2 {
    font-size: 15px;
    font-weight: 600;
    color: var(--fg-1);
  }
  .dry__note,
  .dry__facts dd {
    margin: 0;
    font-size: 13px;
    color: var(--fg-2);
  }
  .dry__waves {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    margin: 0;
    padding-left: var(--space-5);
  }
  .dry__waves li {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-2);
  }
  .dry__step {
    display: inline-flex;
    align-items: center;
    flex-wrap: wrap;
    gap: 6px;
    min-width: 0;
    padding: 4px 10px;
    border-radius: var(--radius-pill);
    background: var(--surface-sunken);
    font-size: 13px;
    color: var(--fg-1);
  }
  .dry__uses {
    color: var(--fg-2);
    overflow-wrap: anywhere;
  }
  .sim {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
  }
  .sim h3 {
    font-size: 14px;
    font-weight: 600;
    color: var(--fg-1);
    margin: 0;
  }
  .sim__steps {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    margin: 0;
    padding: 0;
    list-style: none;
  }
  .sim__step {
    display: flex;
    align-items: flex-start;
    gap: var(--space-2);
    font-size: 13px;
    color: var(--fg-1);
  }
  .sim__body {
    display: flex;
    flex-direction: column;
    min-width: 0;
  }
  .sim__icon {
    flex: none;
    display: inline-flex;
    margin-top: 1px;
    color: var(--fg-3);
  }
  .sim__icon--success {
    color: var(--success-fg);
  }
  .sim__icon--danger {
    color: var(--danger-fg);
  }
  .sim__icon--warning {
    color: var(--warning-fg);
  }
  .dry__facts {
    display: grid;
    gap: var(--space-1);
    margin: 0;
  }
  .dry__facts dt {
    font-size: var(--size-xs);
    font-weight: 600;
    color: var(--fg-1);
    margin-top: var(--space-1);
  }
  .dry__facts dd {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 6px;
  }
  .dry__cap {
    color: var(--fg-1);
  }
  .issues {
    padding: var(--space-3) var(--space-4);
    border-radius: var(--radius-lg);
    border: 1.5px solid var(--danger);
    background: var(--danger-soft);
  }
  .issues h2 {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    font-size: 15px;
    font-weight: 600;
    color: var(--fg-1);
    margin-bottom: var(--space-2);
  }
  /* DS ghost Buttons that wrap a whole line of text instead of one label. */
  .issues :global(.issue) {
    flex-wrap: wrap;
    justify-content: flex-start;
    gap: 6px;
    width: 100%;
    height: auto;
    min-height: 44px;
    padding: var(--space-1) var(--space-2);
    white-space: normal;
    text-align: left;
    font-weight: 500;
  }
  .issue__message {
    flex: 1 1 12em;
  }
  .error {
    margin: 0 var(--space-4) var(--space-3);
    font-size: var(--size-xs);
    font-weight: 500;
    color: var(--danger-fg);
  }
  .settings {
    border-radius: var(--radius-lg);
    background: var(--surface-card);
    border: 1.5px solid var(--border-subtle);
  }
  .blocks > :global(.block) {
    overflow: visible;
  }
  .blocks > :global(.block--error) {
    border-color: var(--danger);
  }
  .settings summary {
    display: flex;
    align-items: center;
    gap: var(--space-3);
    min-height: 44px;
    padding: var(--space-3) var(--space-4);
    cursor: pointer;
    list-style: none;
  }
  .settings summary::-webkit-details-marker {
    display: none;
  }
  .settings__title {
    flex: 1;
    display: flex;
    flex-direction: column;
    font-size: 15px;
    font-weight: 600;
  }
  .settings__meta,
  .block__summary {
    font-size: var(--size-xs);
    font-weight: 400;
    color: var(--fg-2);
  }
  .settings[open] :global(.settings__chevron) {
    transform: rotate(180deg);
  }
  .blocks {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
  }
  .block__head {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    padding: var(--space-2) var(--space-2) var(--space-2) var(--space-3);
  }
  .block__head :global(.block__toggle) {
    flex: 1;
    min-width: 0;
    height: auto;
    min-height: 44px;
    flex-direction: column;
    align-items: stretch;
    gap: 2px;
    padding: var(--space-1) var(--space-2);
    white-space: normal;
    text-align: left;
    font-weight: 400;
  }
  .block__title {
    display: flex;
    flex-wrap: wrap;
    align-items: baseline;
    column-gap: var(--space-2);
    font-size: 15px;
    font-weight: 600;
  }
  .block__type {
    white-space: nowrap;
    font-size: var(--size-xs);
    font-weight: 500;
    color: var(--fg-3);
  }
  .block__summary {
    overflow: hidden;
    white-space: nowrap;
    text-overflow: ellipsis;
  }
  .fields {
    display: grid;
    grid-template-columns: minmax(0, 1fr);
    gap: var(--space-4);
    padding: var(--space-2) var(--space-4) var(--space-4);
  }
  .area {
    height: auto;
    padding: 10px 14px;
    align-items: stretch;
  }
  .area textarea {
    resize: vertical;
    min-height: 4.5em;
    line-height: 1.5;
  }
  #workflow-yaml {
    min-height: 60vh;
    font-family: var(--font-mono);
    font-size: 13px;
    white-space: pre;
  }
  .palette {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(150px, 1fr));
    gap: var(--space-2);
  }
  /* Icon above the text so the label and hint get the tile's full width. */
  .palette :global(.palette__item) {
    display: flex;
    flex-direction: column;
    align-items: flex-start;
    gap: var(--space-2);
    min-height: 44px;
    font: inherit;
    text-align: left;
  }
  .palette__label {
    display: block;
    font-size: 14px;
    font-weight: 600;
  }
  .palette__hint {
    display: block;
    font-size: var(--size-xs);
    color: var(--fg-2);
    margin-top: 2px;
  }
  .palette :global(.palette__item:focus-visible),
  .settings summary:focus-visible {
    outline: none;
    box-shadow: var(--ring-focus);
  }
  @media (min-width: 760px) {
    .fields {
      grid-template-columns: repeat(2, minmax(0, 1fr));
    }
    .fields > .wide,
    .fields > :global(.chips) {
      grid-column: 1 / -1;
    }
  }
</style>
