<!-- Execution log (task page sidebar): the task's runs (trigger, status, cost, duration). Opening a run shows
     what the task panel used to: for the latest run its runtime, limit policy, cost and context meter, side
     effects waiting on a person, pinned constraints, sources to check, data touched, after-merge quality,
     context, MCP tools, steps, device, cost budget, gates, corrections, artifacts and the live timeline;
     for an older run, its steps, gates and timeline. Before any run, the settings that shape it (pins, context, budget, MCP).
     Holds the shared section styles (`.task-log …`) its sections use. -->
<script lang="ts">
  import { Badge, Button, Card, Dialog, Icon, Input, Select } from '@hryer/madlabs-design-system/svelte';
  import IconButton from '../lib/ui/IconButton.svelte';
  import { api, artifact_url } from '../lib/api/client';
  import type { Correction, GateResult, KnowledgeFlag, OnLimit, OutboxItem, RunDetail, RunEvent, RunStatus, Step, StepType, TaskDetail, TaskQualityView, TaskRun, Usage } from '../lib/api/types';
  import { act, app, can, error_text, toast } from '../lib/app_state.svelte';
  import { ON_LIMIT, bytes, clock, context_fill, current_step, money, runtime_name, stuck_label, tokens, uptime, usd } from '../lib/format';
  import DeviceResults from '../lib/ui/DeviceResults.svelte';
  import DeviceView from '../lib/ui/DeviceView.svelte';
  import TaskMcp from '../lib/ui/TaskMcp.svelte';
  import TaskContext from './TaskContext.svelte';
  import TaskCorrections from './TaskCorrections.svelte';
  import TaskQuality from './TaskQuality.svelte';

  let { task, runs, events, live, plain }: { task: TaskDetail; runs: TaskRun[]; events: RunEvent[]; live: boolean; plain: boolean } = $props();

  const RUN_BADGE: Record<RunStatus, ['neutral' | 'info' | 'primary' | 'warning' | 'success' | 'danger', string]> = {
    queued: ['neutral', 'Queued'],
    running: ['info', 'Running'],
    verifying: ['primary', 'Verifying'],
    waiting_approval: ['warning', 'Waiting'],
    done: ['success', 'Done'],
    failed: ['danger', 'Failed'],
    cancelled: ['neutral', 'Cancelled'],
  };
  const TRIGGER: Record<string, string> = { created: 'On create', trigger: 'Trigger', moved: 'Moved', retry: 'Retry', manual: 'Manual', comment: 'Comment' };
  const duration = (r: TaskRun) =>
    r.started_at ? uptime(Math.max(0, Math.round((new Date(r.ended_at ?? Date.now()).getTime() - new Date(r.started_at).getTime()) / 1000))) : '—';

  /** The run shown: `undefined` follows the latest; `null` none (closed). */
  let picked = $state<string | null | undefined>(undefined);
  const latest = $derived(task.run?.id ?? null);
  const open_id = $derived(picked === undefined ? latest : picked);
  /** The latest run is open (or there is no run yet): its sections show. */
  const on_latest = $derived(open_id === latest);
  const toggle_run = (id: string) => (picked = open_id === id ? null : id);

  /** An older run's timeline, steps and gates, read once (it has ended). */
  let older = $state<RunEvent[]>([]);
  let older_run = $state<RunDetail | null>(null);
  $effect(() => {
    const id = open_id;
    [older, older_run] = [[], null];
    if (!id || id === latest) return;
    api.list_events(id).then((list) => id === open_id && (older = list), () => {});
    api.get_run(id).then((detail) => id === open_id && (older_run = detail), () => {});
  });

  /** Timeline kinds about sessions, runtimes and code, which the role view leaves out. */
  const TECHNICAL = new Set(['dispatched', 'tool_call', 'command', 'file_change', 'usage', 'runtime_switch', 'handoff', 'rotation', 'session_assigned', 'watchdog']);
  const timeline = $derived((on_latest ? events : older).filter((e) => !plain || !TECHNICAL.has(e.kind)));

  /** This task's own limit policy; '' follows the workflow, then the ecosystem. */
  let task_limit = $state('');
  const LIMIT_OPTIONS = [{ value: '', label: 'Workflow or ecosystem default' }, ...ON_LIMIT];
  async function set_task_limit(value: string) {
    const was = task_limit;
    task_limit = value;
    try {
      await api.set_task_on_limit(task.id, (value || null) as OnLimit | null);
      toast('success', 'Limit policy saved', value ? undefined : 'This task follows its workflow, then the ecosystem.');
    } catch (e) {
      task_limit = was;
      toast('danger', 'Not saved', error_text(e));
    }
  }
  /** This task's own cost budget per run; the smaller of it and the workflow's wins. */
  let task_usd = $state('');
  let saved_usd = $state<number | null>(null);
  let saving_usd = $state(false);
  const parsed_usd = $derived(usd(task_usd));
  const usd_error = $derived(Number.isNaN(parsed_usd) ? 'Use an amount in dollars, 0 or more.' : '');
  async function save_task_budget(e: SubmitEvent) {
    e.preventDefault();
    if (usd_error) return;
    saving_usd = true;
    try {
      saved_usd = (await api.set_task_budget(task.id, parsed_usd)).cost_usd;
      toast('success', 'Budget saved', saved_usd === null ? 'The workflow’s budget applies.' : 'It applies from the next agent step.');
    } catch (err) {
      toast('danger', 'Not saved', error_text(err));
    }
    saving_usd = false;
  }
  /** The cap on this run's cost: the smaller of the workflow's and the task's. */
  const cost_cap = (workflow: number | null | undefined) => (workflow != null && saved_usd !== null ? Math.min(workflow, saved_usd) : (workflow ?? saved_usd));

  let usage = $state<Usage | null>(null);
  /** The task folder's disk use and the host it is on; null once removed. */
  let disk = $state<{ host: string; bytes: number } | null>(null);
  let flags = $state<KnowledgeFlag[]>([]);
  let corrections = $state<Correction[]>([]);
  const NO_QUALITY: TaskQualityView = { data_touched: [], quality: null, held_up: null };
  let quality = $state<TaskQualityView>(NO_QUALITY);
  /** Side effects of this run whose delivery timed out; they wait for a person. */
  let unknown = $state<OutboxItem[]>([]);
  let confirm_resend = $state<OutboxItem | null>(null);
  let open_detail = $state<Record<number, boolean>>({});

  // Reload with the task (an action elsewhere refreshes it).
  $effect(() => {
    load(task);
  });

  async function load(t: TaskDetail) {
    // Flags, quality and corrections are advisory: an older kernel without the route just shows none.
    const [found, held, fixed, hosts, limit, own_budget, waiting, spent] = await Promise.all([
      api.knowledge_flags(t.id).catch(() => []),
      api.task_quality(t.id).catch(() => NO_QUALITY),
      api.corrections(t.id).catch(() => []),
      api.list_hosts(t.workspace_id).catch(() => []),
      api.task_on_limit(t.id).catch(() => ({ on_limit: null })),
      api.task_budget(t.id).catch(() => ({ cost_usd: null })),
      t.run ? api.list_outbox('unknown').catch(() => []) : [],
      t.run ? api.usage(t.run.id).catch(() => null) : null,
    ]);
    if (t.id !== task.id) return;
    [usage, flags, quality, corrections] = [spent, found, held, fixed];
    task_limit = limit.on_limit ?? '';
    saved_usd = own_budget.cost_usd;
    task_usd = own_budget.cost_usd?.toString() ?? '';
    unknown = waiting.filter((o) => o.run_id === t.run?.id);
    const on = hosts.find((h) => h.disk?.some((d) => d.task_id === t.id));
    const used = on?.disk?.find((d) => d.task_id === t.id);
    disk = on && used ? { host: on.name, bytes: used.bytes } : null;
  }

  function resolve(item: OutboxItem, resolution: 'delivered' | 'resend') {
    confirm_resend = null;
    act(() => api.resolve_outbox(item.id, resolution), resolution === 'resend' ? 'Sending it again' : 'Marked as delivered');
  }

  /** Corrections starred this visit: pressed, and starring again does nothing new. */
  let starred = $state<Record<string, boolean>>({});

  /** Stars a correction, or a `learning` note from the timeline. */
  async function star(c: { id: string }, note = false) {
    try {
      const { created } = await (note ? api.star_note(c.id) : api.star_correction(c.id));
      starred[c.id] = true;
      toast('success', created ? 'Proposed as memory' : 'Already proposed', 'Approve it on the Memory screen to give it to agents.');
    } catch (e) {
      toast('danger', 'Couldn’t propose it', error_text(e));
    }
  }

  const STEP_ICON: Record<StepType, string> = {
    agent: 'bot',
    gate: 'shield-check',
    action: 'plug',
    approval: 'user-check',
    wait: 'hourglass',
    notify: 'bell',
    decide: 'sparkles',
    extension: 'puzzle',
  };
  const EVENT_ICON: Record<string, string> = {
    created: 'circle-plus',
    dispatched: 'cpu',
    plan: 'list-checks',
    tool_call: 'terminal',
    command: 'terminal',
    file_change: 'file-diff',
    data_touched: 'database',
    pr_merged: 'git-merge',
    pr_state: 'git-pull-request',
    quality_checked: 'activity',
    runtime_switch: 'arrow-left-right',
    handoff: 'arrow-left-right',
    failover_review: 'user-check',
    rotation: 'refresh-cw',
    gate_result: 'shield-check',
    approval: 'user-check',
    decision: 'sparkles',
    triaged: 'list-ordered',
    intervention: 'message-square',
    watchdog: 'alarm-clock',
    artifact: 'file-text',
    verify: 'scan-search',
    done: 'circle-check',
  };

  // Timeline filter: over the events already streamed, so a filtered timeline stays live.
  let kind_filter = $state('');
  let text_filter = $state('');
  $effect(() => {
    void open_id;
    [kind_filter, text_filter] = ['', ''];
  });
  const kind_options = $derived.by(() => {
    const counts = new Map<string, number>();
    for (const e of timeline) counts.set(e.kind, (counts.get(e.kind) ?? 0) + 1);
    const label = (kind: string) => kind[0].toUpperCase() + kind.slice(1).replaceAll('_', ' ');
    return [
      { value: '', label: `All events (${timeline.length})` },
      ...[...counts].sort(([a], [b]) => a.localeCompare(b)).map(([kind, n]) => ({ value: kind, label: `${label(kind)} (${n})` })),
    ];
  });
  const needle = $derived(text_filter.trim().toLowerCase());
  const shown = $derived(timeline.filter((e) => (!kind_filter || e.kind === kind_filter) && (!needle || JSON.stringify(e.payload).toLowerCase().includes(needle))));

  // Context meter: the latest agent session's fill, live from the timeline.
  const fill = $derived(context_fill(events));
  /** The latest session's model and effort, where its chain entry set them. */
  const session_settings = $derived.by(() => {
    const p = [...events].reverse().find((e) => e.kind === 'session_assigned')?.payload;
    return [p?.model, p?.effort].filter((v) => typeof v === 'string' && v).join(' · ');
  });
  /** Calm below `rotate_at − 20`, then amber: Sun starts a fresh session at `rotate_at`. */
  const fill_level = $derived(!fill || fill.percent === null ? 'ok' : fill.percent >= fill.rotate_at - 20 ? 'high' : 'ok');

  const step = $derived(current_step(task));
  const stuck = $derived(stuck_label(task));
  /** `current`: the latest run's step; an older run ended, so a step it stopped on is `stuck`. */
  function step_state(status: string, current: boolean) {
    if (status === 'succeeded') return 'done';
    if (status === 'queued') return 'todo';
    return !current || (stuck && step?.status === status) ? 'stuck' : 'now';
  }
</script>

{#snippet steps_list(steps: Step[], current: boolean)}
  <section>
    <h3 class="sun-label">Steps</h3>
    <ol class="steps">
      {#each steps as s (s.step_key)}
        {@const st = step_state(s.status, current)}
        <li class="step step--{st}">
          <span class="step__icon" class:sun-pulse={current && st === 'now' && task.status === 'in_progress'}>
            <Icon name={st === 'done' ? 'check' : STEP_ICON[s.type]} size={14} />
          </span>
          <span class="step__name">{s.name}</span>
          {#if s.attempt > 1}<Badge tone="neutral">Attempt {s.attempt}</Badge>{/if}
          <span class="ml-mono dim" style="font-size:11px">{s.type}</span>
        </li>
      {/each}
    </ol>
  </section>
{/snippet}

{#snippet gates_list(gates: GateResult[])}
  <section>
    <h3 class="sun-label">Gates</h3>
    <ul class="gates">
      {#each gates as g (g.name)}
        <li>
          <span class="gate-icon gate-icon--{g.status}" class:sun-pulse={g.status === 'running'}>
            <Icon name={g.status === 'passed' ? 'circle-check' : g.status === 'failed' ? 'circle-x' : 'loader'} size={16} />
          </span>
          {g.name}
          <span style="flex:1"></span>
          <span class="dim" style="font-size:12px">{g.status}</span>
        </li>
      {/each}
    </ul>
  </section>
{/snippet}

<section class="task-log" aria-labelledby="log-head">
  <h2 class="sun-label timeline-head"><span id="log-head">Execution log</span>{#if live}<Badge tone="info" dot live>Live</Badge>{/if}{#if quality.data_touched.length}<Badge tone="warning">Data touched</Badge>{/if}</h2>
  {#if runs.length}
    <ul class="runs" aria-label="Runs">
      {#each [...runs].reverse() as r (r.id)}
        {@const [tone, label] = RUN_BADGE[r.status]}
        <li>
          <button type="button" class="run" aria-expanded={open_id === r.id} onclick={() => toggle_run(r.id)}>
            <Icon name={open_id === r.id ? 'chevron-down' : 'chevron-right'} size={16} />
            <span class="run__name">Run {r.attempt} <span class="dim">· {TRIGGER[r.trigger] ?? r.trigger}</span></span>
            <Badge {tone} dot live={r.status === 'running'}>{label}</Badge>
            <span class="ml-mono run__meta">{money(r.cost_usd)} · {duration(r)}</span>
          </button>
        </li>
      {/each}
    </ul>
  {:else}
    <p class="hint">{task.assignee || task.workflow ? 'No runs yet. One starts when the task leaves Backlog and a runtime is free.' : 'A plain task: nothing runs.'}</p>
  {/if}

  {#if on_latest}
    {@const run = task.run}
    <div class="task-log__body">
      {#if run || disk}
        <dl class="meta">
          {#if disk && !plain}
            <dt>Disk</dt>
            <dd><span class="ml-mono" style="font-size:12px">{bytes(disk.bytes)}</span> <span class="dim">on {disk.host}</span></dd>
          {/if}
          {#if run && !plain}
            <dt>Runtime</dt>
            <dd class="chain">
              {#each run.runtime_chain as r, i (r)}
                {#if i > 0}<Icon name="chevron-right" size={12} />{/if}
                <span class:on={r === run.runtime_id}>{runtime_name(app.runtimes, r)}</span>
              {/each}
              {#if session_settings}<span class="ml-mono">· {session_settings}</span>{/if}
            </dd>
            <dt id="on-limit-label">On limit</dt>
            <dd>
              <Select size="sm" aria-labelledby="on-limit-label" options={LIMIT_OPTIONS} bind:value={() => task_limit, set_task_limit} />
            </dd>
          {/if}
          {#if run}
            {@const cap = cost_cap(run.budget.cost_usd)}
            <dt>Cost</dt>
            <dd class="ml-mono" style="font-size:12px">
              {money(run.cost_usd)}{#if cap != null}<span class="dim">&nbsp;/ {money(cap)} budget</span>{/if}
            </dd>
            {#if usage && usage.input_tokens + usage.output_tokens > 0 && !plain}
              <dt>Tokens</dt>
              <dd class="ml-mono" style="font-size:12px">{tokens(usage.input_tokens)} in · {tokens(usage.output_tokens)} out</dd>
            {/if}
            {#if fill && !plain}
              <dt>Context</dt>
              <dd class="meter-row" data-testid="context-meter">
                {#if fill.window !== null && fill.percent !== null}
                  <span
                    class="meter"
                    role="meter"
                    aria-label="Context window"
                    aria-valuemin={0}
                    aria-valuemax={100}
                    aria-valuenow={Math.min(fill.percent, 100)}
                    aria-valuetext="{fill.percent}% of {tokens(fill.window)} tokens; a fresh session starts at {fill.rotate_at}%"
                  >
                    <span class="meter__fill meter__fill--{fill_level}" style="width:{Math.min(fill.percent, 100)}%"></span>
                    <span class="meter__mark" style="left:{fill.rotate_at}%"></span>
                  </span>
                  <span class="ml-mono meter__text">{tokens(fill.used)} / {tokens(fill.window)} · {fill.percent}%</span>
                {:else}
                  <span class="ml-mono meter__text">{tokens(fill.used)} tokens</span>
                {/if}
                {#if fill.percent !== null && fill.percent >= fill.rotate_at}
                  <Badge tone="warning"><Icon name="refresh-cw" size={12} />Fresh session after this tool call</Badge>
                {:else if fill.rotations}
                  <Badge tone="info"><Icon name="refresh-cw" size={12} />{fill.rotations === 1 ? 'Rotated once' : `Rotated ${fill.rotations}×`}</Badge>
                {/if}
              </dd>
            {/if}
          {/if}
        </dl>
      {/if}

      {#if unknown.length}
        <section aria-labelledby="unknown-head">
          <h3 class="sun-label timeline-head" id="unknown-head">Needs a decision<Badge tone="warning" count>{unknown.length}</Badge></h3>
          {#each unknown as item (item.id)}
            <Card variant="outline" padding="sm" class="unknown" data-testid="unknown-effect">
              <div class="unknown__head">
                <Icon name="circle-alert" size={16} />
                <span class="ml-mono unknown__target">{item.target}</span>
                <Badge tone="warning">No answer</Badge>
              </div>
              {#if item.summary}<p class="unknown__summary">{item.summary}</p>{/if}
              <p class="hint">Sun didn’t hear back{item.step_key ? ` during “${item.step_key}”` : ''}, so it won’t send this again on its own. Check whether it happened, then tell Sun.</p>
              <div class="row" style="flex-wrap:wrap">
                <Button size="sm" variant="secondary" iconLeft="check" onclick={() => resolve(item, 'delivered')}>It went through</Button>
                <Button size="sm" variant="ice" iconLeft="send" onclick={() => (confirm_resend = item)}>Send again</Button>
              </div>
            </Card>
          {/each}
        </section>
      {/if}

      <TaskContext task_id={task.id} workspace_id={task.workspace_id} {plain} />
      <TaskQuality task_id={task.id} {flags} bind:quality />

      {#if !plain}<TaskMcp task_id={task.id} workspace={task.workspace_id} />{/if}

      {#if run}
        {@render steps_list(run.steps, true)}
      {:else if task.assignee || task.workflow}
        <div class="sun-empty">Waiting for a free runtime. Steps show here once it dispatches.</div>
      {/if}

      {#if run?.steps.some((s) => s.type === 'extension' && s.status === 'running')}
        <section aria-labelledby="device-head">
          <h3 class="sun-label" id="device-head">Device</h3>
          <DeviceView task_id={task.id} />
        </section>
      {/if}

      {#if run && events.some((e) => e.kind === 'device_result')}
        <section class="device-results" aria-labelledby="device-results-head">
          <h3 class="sun-label" id="device-results-head">Device checks</h3>
          <DeviceResults run_id={run.id} {events} />
        </section>
      {/if}

      {#if !plain}
        <section aria-labelledby="budget-head">
          <h3 class="sun-label timeline-head" id="budget-head">Cost budget</h3>
          <p class="hint">USD per run. The smaller of this and the workflow’s budget stops the run.</p>
          <form class="budget" onsubmit={save_task_budget} novalidate>
            <Input size="sm" type="number" inputmode="decimal" min="0" step="0.01" mono aria-labelledby="budget-head" placeholder="Workflow’s" bind:value={task_usd} error={usd_error || undefined} />
            <Button size="sm" variant="secondary" type="submit" loading={saving_usd}>Save</Button>
          </form>
        </section>
      {/if}

      {#if task.gates.length}{@render gates_list(task.gates)}{/if}

      <TaskCorrections {corrections} {starred} {star} />

      {#if task.artifacts.length}
        <section>
          <h3 class="sun-label">Artifacts</h3>
          {#each task.artifacts as a (a.id)}
            <div class="artifact">
              <span class="sun-tile" style="width:36px;height:36px;background:var(--frost-400);color:var(--on-frost)"><Icon name="file-text" size={18} /></span>
              <span class="ml-mono" style="flex:1;min-width:0;font-size:13px;overflow-wrap:anywhere">{a.name} · v{a.version}</span>
              <Button size="sm" variant="secondary" iconLeft="download" href={artifact_url(a.id)} aria-label="Download {a.name} version {a.version}">Download</Button>
            </div>
          {/each}
        </section>
      {/if}
    </div>
  {:else if older_run}
    <div class="task-log__body">
      {@render steps_list(older_run.run.steps, false)}
      {#if older_run.gates.length}{@render gates_list(older_run.gates)}{/if}
    </div>
  {/if}

  {#if open_id && timeline.length}
    <section class="task-log__body" aria-labelledby="timeline-head">
      <h3 class="sun-label timeline-head" id="timeline-head">Timeline</h3>
      <div class="timeline-filter" role="search" aria-label="Filter the timeline">
        <Select class="timeline-filter__kind" size="sm" aria-label="Event kind" bind:value={kind_filter} options={kind_options} />
        <Input class="timeline-filter__text" size="sm" type="search" iconLeft="search" aria-label="Search the timeline" placeholder="Search events" autocomplete="off" bind:value={text_filter} />
      </div>
      {#if !shown.length}
        <p class="hint" role="status">
          No events match.
          <Button variant="ghost" size="sm" onclick={() => ([kind_filter, text_filter] = ['', ''])}>Clear the filter</Button>
        </p>
      {:else if shown.length < timeline.length}
        <p class="hint" role="status">Showing {shown.length} of {timeline.length} events</p>
      {/if}
      <ol class="timeline">
        {#each shown as e (e.seq)}
          <li class="event" class:event--switch={e.kind === 'runtime_switch' || e.kind === 'handoff'}>
            <span class="event__icon event__icon--{e.kind}"><Icon name={EVENT_ICON[e.kind] ?? 'dot'} size={14} /></span>
            <div class="event__body">
              <div class="event__line">
                <span class="event__summary" class:ml-mono={e.kind === 'tool_call' || e.kind === 'command'}>{e.payload.summary}</span>
                <time class="ml-mono dim" datetime={e.at}>{clock(e.at)}</time>
                {#if e.kind === 'note' && e.payload.kind === 'learning' && e.payload.note_id}
                  {@const note_id = String(e.payload.note_id)}
                  {#if can('manage_settings')}<IconButton icon="star" label="Propose this learning as memory" aria-pressed={!!starred[note_id]} variant="ghost" size="sm" onclick={() => star({ id: note_id }, true)} />{/if}
                {/if}
              </div>
              {#if e.payload.detail}
                <Button
                  variant="ghost"
                  size="sm"
                  class="sun-disclosure"
                  iconLeft={open_detail[e.seq] ? 'chevron-down' : 'chevron-right'}
                  aria-expanded={!!open_detail[e.seq]}
                  onclick={() => (open_detail[e.seq] = !open_detail[e.seq])}
                >
                  {open_detail[e.seq] ? 'Hide' : 'Show'} {e.kind === 'tool_call' || e.kind === 'command' ? 'output' : 'details'}
                </Button>
                {#if open_detail[e.seq]}<pre class="ml-mono event__detail">{e.payload.detail}</pre>{/if}
              {/if}
            </div>
          </li>
        {/each}
      </ol>
    </section>
  {/if}
</section>

{#if confirm_resend}
  {@const item = confirm_resend}
  <Dialog open onclose={() => (confirm_resend = null)} title="Send {item.target} again?">
    <p style="margin:0;color:var(--fg-2)">Only do this if it didn’t happen. It goes out once more with the same key, so a plugin that checks keys won’t double it; others might.</p>
    {#snippet footer()}
      <Button variant="secondary" onclick={() => (confirm_resend = null)}>Not yet</Button>
      <Button variant="ice" iconLeft="send" onclick={() => resolve(item, 'resend')}>Send again</Button>
    {/snippet}
  </Dialog>
{/if}

<style>
  .task-log {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
    min-width: 0;
  }
  .task-log__body {
    display: flex;
    flex-direction: column;
    gap: 22px;
    min-width: 0;
  }
  .runs {
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  .run {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 6px var(--space-2);
    width: 100%;
    min-height: 44px;
    padding: var(--space-1) var(--space-2);
    border: 0;
    border-radius: var(--radius-md);
    background: transparent;
    color: var(--fg-1);
    font: inherit;
    font-size: 13px;
    text-align: left;
    cursor: pointer;
  }
  .run:hover,
  .run[aria-expanded='true'] {
    background: var(--accent-soft);
  }
  .run:focus-visible {
    outline: none;
    box-shadow: var(--ring-focus);
  }
  .run__name {
    flex: 1;
    min-width: 0;
    font-weight: 600;
  }
  .run__meta {
    font-size: 11px;
    color: var(--fg-2);
  }
  /* Shared by the execution log's sections (TaskContext, TaskQuality, TaskCorrections). */
  .task-log :global(ol),
  .task-log :global(ul) {
    list-style: none;
    margin: 0;
    padding: 0;
  }
  .task-log :global(.dim) {
    color: var(--fg-3);
  }
  .task-log :global(.hint) {
    margin: 0 0 10px;
    font-size: 13px;
    color: var(--fg-2);
  }
  .task-log :global(.row) {
    display: flex;
    align-items: center;
    gap: var(--space-2);
  }
  .task-log :global(.timeline-head) {
    display: flex;
    align-items: center;
    gap: var(--space-2);
  }
  .task-log :global(.meta) {
    display: grid;
    grid-template-columns: auto 1fr;
    gap: 6px 14px;
    margin: 0;
    font-size: 13px;
  }
  .task-log :global(.meta dt) {
    color: var(--fg-3);
  }
  .task-log :global(.meta dd) {
    margin: 0;
    color: var(--fg-1);
    min-width: 0;
  }
  .task-log :global(.answer) {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    margin: 0;
    padding: 0;
    border: 0;
    min-width: 0;
  }
  .task-log :global(.answer legend) {
    margin-bottom: var(--space-2);
  }
  .task-log :global(.touched) {
    display: flex;
    flex-direction: column;
    gap: 6px;
    margin-bottom: var(--space-2);
    padding: var(--space-3);
    border-radius: var(--radius-lg);
    background: var(--surface-card);
    font-size: 13px;
  }
  .task-log :global(.touched li) {
    display: flex;
    align-items: baseline;
    gap: var(--space-2);
    min-width: 0;
  }
  .task-log :global(.touched__rule) {
    display: inline-flex;
    flex: none;
    align-items: center;
    gap: 4px;
    min-width: 96px;
    color: var(--fg-2);
    font-size: 12px;
  }
  .task-log :global(.touched__path) {
    min-width: 0;
    overflow-wrap: anywhere;
    color: var(--fg-1);
  }
  .task-log :global(.flags) {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
  }
  .task-log :global(.flag) {
    padding: var(--space-3);
    border-radius: var(--radius-lg);
    background: var(--surface-card);
  }
  .task-log :global(.flag__line) {
    display: flex;
    align-items: flex-start;
    gap: var(--space-2);
    font-size: 13px;
    line-height: 1.45;
    color: var(--fg-1);
  }
  .task-log :global(.flag__line .ml-badge) {
    flex: none;
  }
  .task-log :global(.flag__message) {
    min-width: 0;
    overflow-wrap: anywhere;
  }
  .task-log :global(.flag__cites) {
    display: flex;
    flex-direction: column;
    gap: 4px;
    margin-top: var(--space-2);
    font-size: 12px;
    color: var(--fg-2);
  }
  .task-log :global(.flag__cites li) {
    display: flex;
    align-items: baseline;
    gap: 6px;
    min-width: 0;
    overflow-wrap: anywhere;
  }
  .task-log :global(.flag__cites svg) {
    flex: none;
    color: var(--fg-3);
  }
  .task-log :global(.flag__cites a) {
    color: inherit;
    text-decoration: underline;
    text-underline-offset: 2px;
  }
  .task-log :global(.flag__cites a:hover) {
    color: var(--fg-1);
  }
  :global(.unknown) {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
  }
  .unknown__head {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    color: var(--warning-fg);
  }
  .unknown__target {
    flex: 1;
    min-width: 0;
    overflow-wrap: anywhere;
    font-size: 12px;
    color: var(--fg-1);
  }
  .unknown__summary {
    margin: 0;
    color: var(--fg-1);
  }
  .chain {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 4px;
    font-size: 12px;
    color: var(--fg-2);
  }
  .chain .on {
    font-weight: 700;
    color: var(--ice-300);
  }
  .steps {
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  .step {
    display: flex;
    align-items: center;
    gap: var(--space-3);
    padding: var(--space-2) 10px;
    border-radius: var(--radius-md);
  }
  .step__icon {
    width: 26px;
    height: 26px;
    flex: none;
    border-radius: 9px;
    display: grid;
    place-items: center;
    background: var(--surface-raised);
    color: var(--fg-2);
  }
  .step__name {
    flex: 1;
    font-size: 14px;
    font-weight: 500;
    color: var(--fg-2);
  }
  .step--done .step__icon {
    background: var(--mint-400);
    color: var(--on-mint);
  }
  .step--done .step__name {
    color: var(--fg-1);
  }
  .step--now {
    background: var(--info-soft);
  }
  .step--now .step__icon {
    background: var(--ice-400);
    color: var(--on-ice);
  }
  .step--stuck {
    background: var(--warning-soft);
  }
  .step--stuck .step__icon {
    background: var(--amber-400);
    color: var(--on-amber);
  }
  .step--now .step__name,
  .step--stuck .step__name {
    font-weight: 600;
    color: var(--fg-1);
  }
  .gates {
    display: flex;
    flex-direction: column;
    gap: 6px;
  }
  .gates li {
    display: flex;
    align-items: center;
    gap: 10px;
    font-size: 14px;
  }
  .gate-icon {
    display: flex;
    color: var(--fg-3);
  }
  .gate-icon--passed {
    color: var(--mint-300);
  }
  .gate-icon--failed {
    color: var(--danger-fg);
  }
  .budget {
    display: flex;
    align-items: flex-start;
    gap: var(--space-2);
    max-width: 320px;
  }
  .budget :global(.ml-field) {
    flex: 1;
    min-width: 0;
  }
  /* A number input is ~20 characters wide by default; let it shrink at 360 px. */
  .budget :global(.ml-control__input) {
    width: 100%;
    min-width: 0;
  }
  .meter-row {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 6px 10px;
  }
  .meter {
    position: relative;
    flex: 1 1 120px;
    max-width: 220px;
    height: 5px;
    border-radius: 3px;
    background: var(--slate-700);
  }
  .meter__fill {
    position: absolute;
    inset: 0 auto 0 0;
    border-radius: 3px;
    background: var(--ice-400);
    transition: width var(--dur-base, 200ms) ease-out;
  }
  .meter__fill--high {
    background: var(--amber-400);
  }
  /* The rotation threshold. */
  .meter__mark {
    position: absolute;
    top: -3px;
    bottom: -3px;
    width: 2px;
    margin-left: -1px;
    border-radius: 1px;
    background: var(--slate-300);
  }
  .meter__text {
    font-size: 12px;
    white-space: nowrap;
  }
  @media (prefers-reduced-motion: reduce) {
    .meter__fill {
      transition: none;
    }
  }
  .artifact {
    display: flex;
    align-items: center;
    gap: var(--space-3);
    padding: var(--space-3);
    border-radius: var(--radius-lg);
    background: var(--surface-card);
  }
  .timeline-filter {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-2);
    margin-bottom: 10px;
  }
  .timeline-filter :global(.timeline-filter__kind) {
    flex: 1 1 150px;
    min-width: 0;
  }
  .timeline-filter :global(.timeline-filter__text) {
    flex: 2 1 180px;
    min-width: 0;
  }
  .timeline {
    display: flex;
    flex-direction: column;
    gap: 14px;
  }
  .event {
    display: flex;
    gap: var(--space-3);
    animation: ml-fade-up var(--dur-base) var(--ease-out) both;
  }
  .event__icon {
    width: 28px;
    height: 28px;
    flex: none;
    border-radius: 9px;
    display: grid;
    place-items: center;
    background: var(--surface-card);
    color: var(--fg-2);
  }
  .event__icon--watchdog {
    background: var(--warning-soft);
    color: var(--amber-300);
  }
  .event__icon--intervention {
    background: var(--accent-soft-2);
    color: var(--fg-1);
  }
  .event__icon--runtime_switch {
    background: var(--ice-400);
    color: var(--on-ice);
  }
  .event__body {
    flex: 1;
    min-width: 0;
    padding-top: 4px;
  }
  .event--switch .event__body {
    padding: var(--space-3);
    border-radius: var(--radius-lg);
    background: var(--info-soft);
    border: 1.5px solid var(--border-default);
  }
  .event__line {
    display: flex;
    gap: var(--space-2);
    align-items: baseline;
  }
  .event__summary {
    flex: 1;
    font-size: 14px;
    line-height: 1.4;
    color: var(--fg-1);
    overflow-wrap: anywhere;
  }
  .event__summary.ml-mono {
    font-size: 12px;
  }
  .event__line time {
    font-size: 11px;
    flex: none;
  }
</style>
