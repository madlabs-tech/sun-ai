<!-- The Tasks page, and a project's page (`project_id`: only its tasks). The View menu picks Board, List,
     Table or Swimlane; Display sets grouping, swimlane rows, ordering and whether Cancelled shows. Both
     are kept per workspace in this browser. Moves (drag, "Move to…") are optimistic and roll back with
     the kernel's sentence when it refuses one. -->
<script lang="ts">
  import { tick } from 'svelte';
  import { Icon, Input, Select, Switch } from '@hryer/madlabs-design-system/svelte';
  import IconButton from '../lib/ui/IconButton.svelte';
  import Menu from '../lib/ui/Menu.svelte';
  import PageHeader from '../lib/ui/PageHeader.svelte';
  import Onboarding from './Onboarding.svelte';
  import TaskBoard, { type Lane } from './TaskBoard.svelte';
  import TaskRows, { type TableColumn } from './TaskRows.svelte';
  import { app, current_workspace, error_text, local_get, local_set, toast } from '../lib/app_state.svelte';
  import { api } from '../lib/api/client';
  import { href } from '../lib/router.svelte';
  import { COLUMNS, PRIORITIES, PRIORITY_RANK, STATUS_BADGE, STATUS_ICON, assignee_of } from '../lib/format';
  import type { Project, QualityMetrics, TaskPatch, TaskSummary } from '../lib/api/types';

  let { on_new, project_id }: { on_new: () => void; project_id?: string } = $props();
  let query = $state('');

  const VIEWS = [
    { value: 'board', label: 'Board', icon: 'kanban' },
    { value: 'list', label: 'List', icon: 'list' },
    { value: 'table', label: 'Table', icon: 'table-2' },
    { value: 'swimlane', label: 'Swimlane', icon: 'rows-3' },
  ] as const;
  const GROUPINGS = [
    { value: 'status', label: 'Status' },
    { value: 'project', label: 'Project' },
    { value: 'priority', label: 'Priority' },
    { value: 'none', label: 'None' },
  ] as const;
  const LANES = [
    { value: 'assignee', label: 'Assignee' },
    { value: 'project', label: 'Project' },
    { value: 'priority', label: 'Priority' },
  ] as const;
  const ORDERINGS = [
    { value: 'manual', label: 'Manual' },
    { value: 'priority', label: 'Priority' },
    { value: 'updated', label: 'Updated' },
    { value: 'created', label: 'Created' },
    { value: 'title', label: 'Title' },
  ] as const;
  type Values<T extends readonly { value: string }[]> = T[number]['value'];
  type Ordering = Values<typeof ORDERINGS>;

  interface Display {
    view: Values<typeof VIEWS>;
    grouping: Values<typeof GROUPINGS>;
    lanes: Values<typeof LANES>;
    ordering: Ordering;
    show_cancelled: boolean;
    /** The table's header sort; `null`: the Display ordering. */
    sort: { column: TableColumn; desc: boolean } | null;
  }

  const KEY = 'sun_task_view:';
  const COLUMN_IDS: string[] = ['key', 'title', 'status', 'priority', 'assignee', 'project', 'created', 'updated'];

  /** The saved view of `workspace`; anything missing or unknown is the default. */
  function load(workspace: string): Display {
    let saved: Partial<Record<keyof Display, unknown>> = {};
    try {
      saved = JSON.parse(local_get(KEY + workspace) || '{}') ?? {};
    } catch {
      // A broken value: the defaults.
    }
    const one = <T extends string>(value: unknown, list: readonly { value: T }[]): T => list.find((o) => o.value === value)?.value ?? list[0].value;
    const sort = saved.sort as Display['sort'];
    return {
      view: one(saved.view, VIEWS),
      grouping: one(saved.grouping, GROUPINGS),
      lanes: one(saved.lanes, LANES),
      ordering: one(saved.ordering, ORDERINGS),
      show_cancelled: saved.show_cancelled === true,
      sort: sort && COLUMN_IDS.includes(sort.column) ? { column: sort.column, desc: sort.desc === true } : null,
    };
  }

  let display = $derived(load(current_workspace()));

  function set(change: Partial<Display>) {
    display = { ...display, ...change };
    local_set(KEY + current_workspace(), JSON.stringify(display));
  }

  let projects = $state<Project[]>([]);
  $effect(() => {
    const workspace = current_workspace();
    if (workspace) api.list_projects(workspace).then((p) => (projects = p), () => (projects = []));
  });
  const project = $derived(project_id ? projects.find((p) => p.id === project_id) : undefined);

  /** Manual order is per status column: the board and swimlanes, or rows grouped by status. */
  const manual_ok = $derived(display.view === 'board' || display.view === 'swimlane' || display.grouping === 'status');
  const ordering = $derived<Ordering>(display.ordering === 'manual' && !manual_ok ? 'priority' : display.ordering);
  const statuses = $derived(COLUMNS.filter((c) => display.show_cancelled || !c.hidden).map((c) => c.status));

  const project_name = (t: TaskSummary) => projects.find((p) => p.id === t.project_id)?.name ?? '';
  const ORDER: Record<Ordering, (a: TaskSummary, b: TaskSummary) => number> = {
    manual: (a, b) => a.position - b.position,
    priority: (a, b) => PRIORITY_RANK[b.priority] - PRIORITY_RANK[a.priority] || a.position - b.position,
    updated: (a, b) => b.updated_at.localeCompare(a.updated_at),
    created: (a, b) => b.created_at.localeCompare(a.created_at),
    title: (a, b) => a.title.localeCompare(b.title),
  };
  const BY_COLUMN: Record<TableColumn, (a: TaskSummary, b: TaskSummary) => number> = {
    key: (a, b) => a.number - b.number,
    title: ORDER.title,
    status: (a, b) => statuses.indexOf(a.status) - statuses.indexOf(b.status),
    priority: ORDER.priority,
    assignee: (a, b) => assignee_of(a, app.agents).label.localeCompare(assignee_of(b, app.agents).label),
    project: (a, b) => project_name(a).localeCompare(project_name(b)),
    created: (a, b) => a.created_at.localeCompare(b.created_at),
    updated: (a, b) => a.updated_at.localeCompare(b.updated_at),
  };

  const shown = $derived.by(() => {
    const q = query.toLowerCase();
    const sort = display.view === 'table' ? display.sort : null;
    const compare = sort ? (a: TaskSummary, b: TaskSummary) => BY_COLUMN[sort.column](a, b) * (sort.desc ? -1 : 1) : ORDER[ordering];
    return app.tasks
      .filter((t) => (!project_id || t.project_id === project_id) && statuses.includes(t.status) && (!q || `${t.key} ${t.title}`.toLowerCase().includes(q)))
      .sort(compare);
  });

  /** `tasks` split by `field`, in that field's order. Fixed sets keep their empty groups when `keep_empty` (drop targets). */
  function group_by(field: Display['grouping'] | Display['lanes'], tasks: TaskSummary[], keep_empty: boolean): Lane[] {
    let groups: Omit<Lane, 'tasks'>[];
    let key_of: (t: TaskSummary) => string;
    if (field === 'status') {
      groups = statuses.map((s) => ({ key: s, label: STATUS_BADGE[s][1], icon: STATUS_ICON[s] }));
      key_of = (t) => t.status;
    } else if (field === 'priority') {
      groups = PRIORITIES.map((p) => ({ key: p.value, label: p.label, icon: p.icon }));
      key_of = (t) => t.priority;
    } else if (field === 'project') {
      groups = [...projects.map((p) => ({ key: p.id, label: p.name, icon: 'folder-kanban' })), { key: '', label: 'No project', icon: 'minus' }];
      key_of = (t) => t.project_id ?? '';
    } else if (field === 'assignee') {
      const KIND = { agent: 0, workflow: 1, none: 2 };
      // Every agent has a row (a drop there assigns it); workflows and None only once a task has them.
      const agent_rows = app.agents.map((a) => ({ kind: 'agent' as const, key: `agent:${a.id}`, label: a.name, icon: 'bot' }));
      groups = [...new Map([...agent_rows, ...tasks.map((t) => assignee_of(t, app.agents))].map((a) => [a.key, a])).values()]
        .sort((a, b) => KIND[a.kind] - KIND[b.kind] || a.label.localeCompare(b.label))
        .map((a) => ({ key: a.key, label: a.label, icon: a.icon }));
      key_of = (t) => assignee_of(t, app.agents).key;
    } else {
      return [{ key: 'all', label: 'All tasks', tasks }];
    }
    return groups.map((g) => ({ ...g, tasks: tasks.filter((t) => key_of(t) === g.key) })).filter((g) => keep_empty || g.tasks.length);
  }

  const lanes = $derived(
    display.view === 'board'
      ? [{ key: '', label: '', tasks: shown }]
      : display.view === 'swimlane'
        ? group_by(display.lanes, shown, true)
        : group_by(display.grouping, shown, false),
  );

  /** Applies `patch` at once, then the kernel's answer; a refusal puts the task back and says why. */
  async function move(t: TaskSummary, patch: TaskPatch, from_menu = false) {
    const live = app.tasks.find((x) => x.id === t.id);
    if (!live) return;
    const before = Object.fromEntries(Object.keys(patch).map((k) => [k, live[k as keyof TaskPatch]]));
    Object.assign(live, patch);
    if (from_menu) {
      // The card was redrawn (another column): keep focus on it, not on the page.
      await tick();
      if (document.activeElement === document.body) document.querySelector<HTMLElement>(`[data-task-id="${t.id}"] [aria-haspopup="menu"]`)?.focus();
    }
    try {
      Object.assign(live, await api.update_task(t.id, patch));
      if (from_menu && patch.status) toast('success', `${t.key} moved to ${STATUS_BADGE[patch.status][1]}`);
    } catch (e) {
      Object.assign(live, before);
      toast('danger', `${t.key} didn’t move`, error_text(e));
    }
  }

  const link = (t: TaskSummary) => (project_id ? href('projects', project_id, t.key) : href('tasks', t.key));
  const view = $derived(VIEWS.find((v) => v.value === display.view) ?? VIEWS[0]);

  function sort_by(column: TableColumn) {
    const s = display.sort;
    set({ sort: s?.column !== column ? { column, desc: false } : s.desc ? null : { column, desc: true } });
  }

  const waiting = $derived(app.tasks.filter((t) => t.status === 'needs_you').length);

  // Post-merge quality: advisory, so an older kernel just shows nothing.
  let metrics = $state<QualityMetrics | null>(null);
  $effect(() => {
    void app.tasks;
    api.quality_metrics().then((m) => (metrics = m), () => (metrics = null));
  });
  const held_line = $derived(
    metrics?.held_up_rate != null
      ? ` · ${Math.round(metrics.held_up_rate * 100)}% of merged work held up (${metrics.held_up} of ${metrics.decided})`
      : '',
  );

  // Plan cap on agents at once: say why queued tasks wait instead of leaving people guessing.
  let agents_cap = $state<number | null>(null);
  api.get_entitlement().then((e) => (agents_cap = e.limits?.agents ?? null), () => {});
  const plan_notice = $derived.by(() => {
    if (agents_cap === null) return '';
    const active = app.tasks.filter((t) => ['in_progress', 'verifying', 'needs_you'].includes(t.status) && t.run).length;
    const queued = app.tasks.filter((t) => t.status === 'todo' && t.run?.status === 'queued').length;
    if (active < agents_cap || !queued) return '';
    return `Your plan runs ${agents_cap} agents at once. ${queued} queued ${queued === 1 ? 'task starts' : 'tasks start'} when one finishes.`;
  });
</script>

<PageHeader
  title={project_id ? (project?.name ?? 'Project') : 'Tasks'}
  subtitle={project_id ? project?.description || 'Project' : `${waiting} waiting on you${held_line}`}
>
  <div class="tasks-search">
    <Input size="sm" iconLeft="search" placeholder="Search tasks" aria-label="Search tasks" bind:value={query} />
  </div>
  <div class="tasks-tools">
    <Menu
      label="View"
      icon={view.icon}
      text={view.label}
      variant="secondary"
      items={VIEWS.map((v) => ({ label: v.label, icon: v.icon, checked: v.value === display.view, onselect: () => set({ view: v.value }) }))}
    />
    <Menu label="Display" icon="sliders-horizontal" text="Display" variant="secondary">
      {#if display.view === 'list' || display.view === 'table'}
        <Select
          label="Grouping"
          size="sm"
          value={display.grouping}
          options={[...GROUPINGS]}
          onchange={(e: Event) => set({ grouping: (e.currentTarget as HTMLSelectElement).value as Display['grouping'] })}
        />
      {/if}
      {#if display.view === 'swimlane'}
        <Select
          label="Swimlane rows"
          size="sm"
          value={display.lanes}
          options={[...LANES]}
          onchange={(e: Event) => set({ lanes: (e.currentTarget as HTMLSelectElement).value as Display['lanes'] })}
        />
      {/if}
      <Select
        label="Ordering"
        size="sm"
        value={ordering}
        options={ORDERINGS.filter((o) => o.value !== 'manual' || manual_ok)}
        hint={manual_ok ? undefined : 'Manual order needs grouping by status.'}
        onchange={(e: Event) => set({ ordering: (e.currentTarget as HTMLSelectElement).value as Ordering, sort: null })}
      />
      <Switch label="Show cancelled" checked={display.show_cancelled} onchange={(e: Event) => set({ show_cancelled: (e.currentTarget as HTMLInputElement).checked })} />
    </Menu>
    <IconButton icon="plus" label="New task" variant="ice" size="sm" onclick={on_new} />
  </div>
</PageHeader>

{#if !project_id}<Onboarding />{/if}

{#if plan_notice}
  <p class="plan-notice" role="status"><Icon name="info" size={16} />{plan_notice} <a href={href('settings', 'billing')}>See the plan</a></p>
{/if}

{#if project_id && !app.error && !app.tasks.some((t) => t.project_id === project_id)}
  <p class="sun-empty project-empty">No tasks in this project yet. Move tasks in from Tasks: Swimlane, rows by Project.</p>
{/if}

{#if app.error}
  <div class="sun-page"><div class="sun-empty" role="alert">{app.error}</div></div>
{:else if display.view === 'board' || display.view === 'swimlane'}
  <TaskBoard
    {lanes}
    {statuses}
    lane_field={display.view === 'board' ? '' : display.lanes}
    manual={ordering === 'manual'}
    {projects}
    {link}
    on_move={move}
  />
{:else}
  <TaskRows
    mode={display.view}
    groups={lanes}
    grouped={display.grouping !== 'none'}
    {projects}
    {link}
    on_move={move}
    sort={display.sort}
    on_sort={sort_by}
  />
{/if}

<style>
  .plan-notice {
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: var(--space-2);
    margin: 0 var(--space-4);
    padding: var(--space-3);
    border-radius: var(--radius-md);
    border: 1.5px solid var(--border-subtle);
    background: var(--surface-tint);
    font-size: 13px;
    color: var(--fg-1);
  }
  .plan-notice a {
    color: var(--fg-link);
    font-weight: 600;
  }
  .tasks-search {
    flex: 1 1 200px;
  }
  .project-empty {
    margin: var(--space-4) var(--space-4) 0;
  }
  .tasks-tools {
    display: flex;
    align-items: center;
    gap: var(--space-2);
  }
  @media (min-width: 1024px) {
    .tasks-search {
      flex: 0 1 240px;
    }
  }
</style>
