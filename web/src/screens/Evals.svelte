<!-- Eval replay: try a changed instruction, memory line or workflow on past tasks
     before rolling it out, side by side with the original runs. Replays run in a sandbox. -->
<script lang="ts">
  import { Badge, Button, Card, Input, Select } from '@hryer/madlabs-design-system/svelte';
  import PageHeader from '../lib/ui/PageHeader.svelte';
  import { api } from '../lib/api/client';
  import type { EvalOutcome, EvalPick, EvalRun, EvalSet, Runtime, Workflow } from '../lib/api/types';
  import { current_workspace, error_text, toast } from '../lib/app_state.svelte';
  import { CAUSE_BADGE, chain_names, day, runtime_name } from '../lib/format';
  import { go, href, route } from '../lib/router.svelte';

  let evals = $state<EvalRun[]>([]);
  let sets = $state<EvalSet[]>([]);
  let workflows = $state<Workflow[]>([]);
  let runtimes = $state<Runtime[]>([]);
  let error = $state('');
  let loaded = $state(false);

  async function load() {
    try {
      [evals, sets] = await Promise.all([api.evals(), api.eval_sets()]);
      error = '';
    } catch (e) {
      error = error_text(e);
    }
    loaded = true;
  }
  load();
  api.list_workflows().then((w) => (workflows = w), () => {});
  api.list_runtimes(current_workspace()).then((r) => (runtimes = r), () => {});

  // ponytail: polls while a replay runs; an eval SSE stream would replace this.
  const running = $derived(evals.some((e) => e.status === 'running'));
  $effect(() => {
    if (!running) return;
    const timer = setInterval(load, 2000);
    return () => clearInterval(timer);
  });

  const selected = $derived(route.id ? (evals.find((e) => e.id === route.id) ?? null) : null);

  // ---- new eval
  type PickBy = EvalPick['by'];
  let name = $state('');
  let pick_by = $state<PickBy>('cause');
  let cause = $state<string>('test_failure');
  let workflow_id = $state('');
  let set_id = $state('');
  let memory_text = $state('');
  let workflow_yaml = $state('');
  let runtime = $state('');
  let busy = $state(false);
  let form_error = $state('');

  const PICKS = [
    { value: 'cause', label: 'Tasks corrected for a cause' },
    { value: 'workflow', label: 'Tasks of a workflow' },
    { value: 'set', label: 'A saved eval set' },
  ];
  const CAUSES = Object.entries(CAUSE_BADGE).map(([value, [, label]]) => ({ value, label }));

  function pick(): EvalPick | null {
    if (pick_by === 'cause') return { by: 'cause', cause: cause as keyof typeof CAUSE_BADGE };
    if (pick_by === 'workflow') return workflow_id ? { by: 'workflow', workflow_id } : null;
    return set_id ? { by: 'set', set_id } : null;
  }

  async function start(e: SubmitEvent) {
    e.preventDefault();
    const chosen = pick();
    if (!chosen) {
      form_error = pick_by === 'set' ? 'Pick a saved set.' : 'Pick a workflow.';
      return;
    }
    const memory = memory_text.split('\n').map((l) => l.trim()).filter(Boolean);
    busy = true;
    form_error = '';
    try {
      const run = await api.start_eval({
        name: name.trim(),
        pick: chosen,
        candidate: { memory, workflow: workflow_yaml.trim() || null },
        runtime: runtime || null,
      });
      evals = [run, ...evals];
      toast('success', 'Eval started', 'Replaying in a sandbox. Nothing leaves it.');
      name = '';
      memory_text = '';
      go('evals', run.id);
    } catch (err) {
      form_error = error_text(err);
    }
    busy = false;
  }

  async function save_set(run: EvalRun) {
    const ids = run.report?.tasks.map((t) => t.task_id) ?? [];
    try {
      await api.save_eval_set(run.name, ids);
      sets = await api.eval_sets();
      toast('success', 'Saved as an eval set', `Replay these ${ids.length} tasks against your next change.`);
    } catch (err) {
      toast('danger', 'Couldn’t save the set', error_text(err));
    }
  }

  // ---- display
  const STATUS = {
    running: ['info', 'Replaying'],
    done: ['success', 'Done'],
    failed: ['danger', 'Failed'],
  } as const;
  const pct = (rate: number) => `${Math.round(rate * 100)}%`;
  const signed = (n: number, unit = '') => (n === 0 ? `±0${unit}` : `${n > 0 ? '+' : '−'}${Math.abs(n)}${unit}`);
  const pts = (delta: number) => signed(Math.round(delta * 100), ' pts');
  /** Better is up for passes, down for repeats; text says which, colour only repeats it. */
  const verdict = (delta: number, up_is_good: boolean) =>
    delta === 0 ? (['neutral', 'no change'] as const) : (delta > 0) === up_is_good ? (['success', 'better'] as const) : (['danger', 'worse'] as const);
  const outcome_badge = (o: EvalOutcome) => (o.passed ? (['success', 'Passed'] as const) : (['danger', o.status.replace('_', ' ')] as const));
  const runtime_text = (r: string) => (r === 'replay' ? 'recorded transcripts, no model ran' : `real runs on ${r}`);
  const pick_text = (p: EvalPick) =>
    p.by === 'cause'
      ? `Corrected for ${CAUSE_BADGE[p.cause]?.[1] ?? p.cause}`
      : p.by === 'workflow'
        ? `Workflow ${workflows.find((w) => w.id === p.workflow_id)?.name ?? p.workflow_id}`
        : p.by === 'set'
          ? `Set ${sets.find((s) => s.id === p.set_id)?.name ?? p.set_id}`
          : `${p.task_ids.length} picked tasks`;
</script>

<PageHeader title="Evals" subtitle="Replay past tasks against a changed instruction or workflow before you roll it out. Nothing leaves the sandbox." />

<div class="sun-page">
  {#if error}
    <div class="sun-empty" role="alert">{error}</div>
  {:else if !loaded}
    <div class="sun-empty" role="status">Loading…</div>
  {/if}

  {#if route.id && loaded && !error}
    <div><Button variant="ghost" size="sm" iconLeft="arrow-left" href={href('evals')}>All evals</Button></div>
    {#if !selected}
      <div class="sun-empty">This eval doesn’t exist.</div>
    {:else}
      {@const [tone, label] = STATUS[selected.status]}
      <Card as="section" padding="sm" class="stack" aria-labelledby="eval-head">
        <div class="head">
          <h2 id="eval-head">{selected.name}</h2>
          <Badge {tone} live={selected.status === 'running'}>{label}</Badge>
        </div>
        <p class="meta">
          {pick_text(selected.spec.pick)}
          {#if selected.report}· {selected.report.replayed} replayed · {runtime_text(selected.report.runtime)}{/if}
        </p>
        {#if selected.spec.candidate?.memory?.length || selected.spec.candidate?.workflow}
          <div class="candidate">
            <h3 class="sun-label">Candidate</h3>
            <ul>
              {#each selected.spec.candidate?.memory ?? [] as line (line)}<li>{line}</li>{/each}
              {#if selected.spec.candidate?.workflow}<li>A changed workflow version</li>{/if}
            </ul>
          </div>
        {/if}
        {#if selected.status === 'running'}
          <p class="sun-empty" role="status">Replaying each task in a sandbox. This page updates on its own.</p>
        {:else if selected.status === 'failed'}
          <p class="sun-empty" role="alert">{selected.error}</p>
        {/if}
      </Card>

      {#if selected.report}
        {@const r = selected.report}
        {@const pass = verdict(r.pass_delta, true)}
        {@const rep = verdict(r.repeat_delta, false)}
        <section class="tiles" aria-label="Result">
          <Card variant="tint" padding="sm" class="tile">
            <h3 class="sun-label">Pass rate</h3>
            <p class="tile__value ml-mono">{pct(r.original.pass_rate)} → {pct(r.replay.pass_rate)}</p>
            <Badge tone={pass[0]}>{pts(r.pass_delta)} · {pass[1]}</Badge>
          </Card>
          <Card variant="tint" padding="sm" class="tile">
            <h3 class="sun-label">Repeat mistakes</h3>
            <p class="tile__value ml-mono">{r.original.repeats} → {r.replay.repeats}</p>
            <Badge tone={rep[0]}>{signed(r.repeat_delta)} · {rep[1]}</Badge>
          </Card>
        </section>

        <section class="list" aria-labelledby="tasks-head">
          <div class="head">
            <h2 class="sun-label" id="tasks-head">Tasks</h2>
            <Button size="sm" variant="secondary" iconLeft="save" onclick={() => save_set(selected)}>Save as eval set</Button>
          </div>
          {#each r.tasks as t (t.task_id)}
            <article class="task" aria-label={t.title}>
              <a class="task__title" href={href('tasks', t.task_id)}>{t.title}</a>
              <div class="sides">
                {#each [['Original', t.original], ['Replay', t.replay]] as const as [side, o] (side)}
                  <div class="side">
                    <span class="side__name">{side}</span>
                    {#if o}
                      {@const [otone, olabel] = outcome_badge(o)}
                      <Badge tone={otone} dot>{olabel}</Badge>
                      <span class="side__n">{o.causes.length} corrections · {o.repeats} repeated</span>
                      {#if o.causes.length}
                        <span class="causes">
                          {#each [...new Set(o.causes)] as c (c)}<Badge tone={CAUSE_BADGE[c][0]}>{CAUSE_BADGE[c][1]}</Badge>{/each}
                        </span>
                      {/if}
                      {#if o.actions.length}<span class="side__n">Recorded, not sent: <span class="ml-mono">{o.actions.join(', ')}</span></span>{/if}
                    {:else}
                      <span class="side__n" role="alert">Couldn’t replay: {t.error}</span>
                    {/if}
                  </div>
                {/each}
              </div>
            </article>
          {/each}
        </section>
      {/if}
    {/if}
  {:else if loaded && !error}
    <Card as="form" padding="sm" class="stack" aria-labelledby="new-head" onsubmit={start}>
      <h2 class="sun-label" id="new-head">New eval</h2>
      <Input label="Name" bind:value={name} required maxlength={200} placeholder="e.g. Tests before done" />
      <div class="row">
        <Select label="Tasks to replay" options={PICKS} bind:value={pick_by} />
        {#if pick_by === 'cause'}
          <Select label="Cause" options={CAUSES} bind:value={cause} />
        {:else if pick_by === 'workflow'}
          <Select label="Workflow" placeholder="Pick a workflow" options={workflows.map((w) => ({ value: w.id, label: w.name }))} bind:value={workflow_id} />
        {:else}
          <Select label="Eval set" placeholder={sets.length ? 'Pick a set' : 'No saved sets yet'} options={sets.map((s) => ({ value: s.id, label: `${s.name} (${s.task_ids.length})` }))} bind:value={set_id} />
        {/if}
      </div>
      <label class="ml-field">
        <span class="ml-field__label">Candidate instructions</span>
        <span class="ml-control area"><textarea rows="3" bind:value={memory_text} placeholder="One per line, e.g. Run the tests and make them pass before you finish a step."></textarea></span>
        <span class="ml-field__hint">Added to every replayed agent’s context, as approved memory would be.</span>
      </label>
      <details>
        <summary>Candidate workflow (optional)</summary>
        <label class="ml-field">
          <span class="ml-field__label">Workflow YAML</span>
          <span class="ml-control area"><textarea rows="6" class="ml-mono" bind:value={workflow_yaml} placeholder="Replayed instead of each task’s latest workflow version"></textarea></span>
        </label>
      </details>
      <Select
        label="Run on"
        options={[{ value: '', label: 'Recorded transcripts (free, no model runs)' }, ...chain_names(runtimes).map((name) => ({ value: name, label: `${runtime_name(runtimes, name)} (real runs, uses quota)` }))]}
        bind:value={runtime}
        hint="Pull requests, messages and other side effects are recorded, never sent."
      />
      {#if form_error}<p class="error" role="alert">{form_error}</p>{/if}
      <div><Button variant="ice" iconLeft="play" type="submit" loading={busy}>Start eval</Button></div>
    </Card>

    <section class="list" aria-labelledby="evals-head">
      <h2 class="sun-label" id="evals-head">Evals <Badge tone="neutral" count>{evals.length}</Badge></h2>
      {#each evals as e (e.id)}
        {@const [tone, label] = STATUS[e.status]}
        <a class="sun-row" href={href('evals', e.id)} aria-label={e.name}>
          <span class="sun-row__body">
            <span class="sun-row__title">{e.name}</span>
            <span class="sun-row__meta">{pick_text(e.spec.pick)} · {day(e.created_at)}</span>
          </span>
          <span class="sun-row__aside">
            {#if e.report}<span class="ml-mono">{pts(e.report.pass_delta)}</span>{/if}
            <Badge {tone} live={e.status === 'running'}>{label}</Badge>
          </span>
        </a>
      {:else}
        <div class="sun-empty">No evals yet. Start one above to see how a change does on work Sun already did.</div>
      {/each}
    </section>
  {/if}
</div>

<style>
  .sun-page > :global(.stack),
  .list {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
  }
  .head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--space-2);
    flex-wrap: wrap;
  }
  h2,
  h3 {
    margin: 0;
  }
  .head h2:not(.sun-label) {
    font-size: 18px;
    color: var(--fg-1);
    overflow-wrap: anywhere;
  }
  .meta,
  .side__n {
    margin: 0;
    font-size: 13px;
    color: var(--fg-2);
  }
  .candidate ul {
    margin: 6px 0 0;
    padding-left: 18px;
    font-size: 14px;
    color: var(--fg-1);
  }
  .tiles {
    display: grid;
    grid-template-columns: 1fr;
    gap: var(--space-3);
  }
  @media (min-width: 640px) {
    .tiles {
      grid-template-columns: 1fr 1fr;
    }
  }
  .tiles > :global(.tile) {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    align-items: flex-start;
  }
  .tile__value {
    margin: 0;
    font-size: 28px;
    font-weight: 700;
    color: var(--fg-1);
  }
  .task {
    padding: var(--space-3);
    border-radius: var(--radius-lg);
    border: 1.5px solid var(--border-subtle);
    background: var(--surface-card);
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
  }
  .task__title {
    display: flex;
    align-items: center;
    min-height: 44px;
    font-weight: 600;
    color: var(--fg-1);
    overflow-wrap: anywhere;
  }
  .sides {
    display: grid;
    grid-template-columns: 1fr;
    gap: var(--space-2);
  }
  @media (min-width: 640px) {
    .sides {
      grid-template-columns: 1fr 1fr;
    }
  }
  .side {
    display: flex;
    flex-direction: column;
    align-items: flex-start;
    gap: 4px;
    min-width: 0;
  }
  .side__name {
    font-size: var(--size-xs);
    font-weight: 600;
    color: var(--fg-3);
    text-transform: uppercase;
    letter-spacing: 0.04em;
  }
  .side__n {
    overflow-wrap: anywhere;
  }
  .causes {
    display: flex;
    flex-wrap: wrap;
    gap: 4px;
  }
  .row {
    display: grid;
    grid-template-columns: 1fr;
    gap: var(--space-3);
  }
  @media (min-width: 640px) {
    .row {
      grid-template-columns: 1fr 1fr;
    }
  }
  .area {
    height: auto;
    padding: var(--space-2) var(--space-3);
  }
  .area textarea {
    width: 100%;
    border: 0;
    outline: 0;
    resize: vertical;
    background: transparent;
    color: inherit;
    font: inherit;
    font-size: var(--size-body);
  }
  details summary {
    cursor: pointer;
    min-height: 44px;
    display: flex;
    align-items: center;
    font-size: 14px;
    color: var(--fg-2);
  }
  .error {
    margin: 0;
    color: var(--danger-fg);
    font-size: 13px;
  }
</style>
