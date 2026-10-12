<!-- List and Table views. List: rows under collapsible group headers. Table: a sortable table (the
     header buttons sort; again reverses), grouped by a header row per group. Each row has the
     "Move to…" menu; its key and title open the task. -->
<script lang="ts" module>
  export const TABLE_COLUMNS = [
    ['key', 'Key'],
    ['title', 'Title'],
    ['status', 'Status'],
    ['priority', 'Priority'],
    ['assignee', 'Assignee'],
    ['project', 'Project'],
    ['created', 'Created'],
    ['updated', 'Updated'],
  ] as const;
  export type TableColumn = (typeof TABLE_COLUMNS)[number][0];
</script>

<script lang="ts">
  import { Badge, Icon } from '@hryer/madlabs-design-system/svelte';
  import TaskMoveMenu from './TaskMoveMenu.svelte';
  import type { Lane } from './TaskBoard.svelte';
  import type { Project, TaskPatch, TaskSummary } from '../lib/api/types';
  import { STATUS_BADGE, STATUS_ICON, assignee_of, day, priority_of } from '../lib/format';
  import { app } from '../lib/app_state.svelte';

  let {
    mode,
    groups,
    grouped,
    projects,
    link,
    on_move,
    sort = null,
    on_sort,
  }: {
    mode: 'list' | 'table';
    groups: Lane[];
    grouped: boolean;
    projects: Project[];
    link: (t: TaskSummary) => string;
    on_move: (t: TaskSummary, patch: TaskPatch, from_menu?: boolean) => void;
    sort?: { column: TableColumn; desc: boolean } | null;
    on_sort?: (column: TableColumn) => void;
  } = $props();

  const uid = $props.id();
  let collapsed = $state<Record<string, boolean>>({});
  const project_name = (t: TaskSummary) => projects.find((p) => p.id === t.project_id)?.name ?? '';
</script>

{#snippet priority(t: TaskSummary)}
  {@const p = priority_of(t.priority)}
  <span class="rows__priority" class:urgent={t.priority === 'urgent'}><Icon name={p.icon} size={16} /><span class="rows__label">{p.label}</span></span>
{/snippet}

{#snippet who(t: TaskSummary)}
  {@const a = assignee_of(t, app.agents)}
  <span class="rows__chip"><Icon name={a.icon} size={14} />{a.label}</span>
{/snippet}

{#if mode === 'list'}
  <div class="rows">
    {#each groups as g, gi (g.key)}
      <section class="rows__group" aria-labelledby={grouped ? `${uid}-g${gi}` : undefined} aria-label={grouped ? undefined : 'Tasks'}>
        {#if grouped}
          <h2 class="rows__head">
            <button type="button" id="{uid}-g{gi}" aria-expanded={!collapsed[g.key]} aria-controls="{uid}-l{gi}" onclick={() => (collapsed[g.key] = !collapsed[g.key])}>
              <Icon name={collapsed[g.key] ? 'chevron-right' : 'chevron-down'} size={16} />
              {#if g.icon}<Icon name={g.icon} size={16} />{/if}
              <span>{g.label}</span>
              <span class="ml-mono rows__count">{g.tasks.length}</span>
            </button>
          </h2>
        {/if}
        <ul class="rows__list" id="{uid}-l{gi}" hidden={collapsed[g.key]}>
          {#each g.tasks as t (t.id)}
            <li class="row" data-task-id={t.id}>
              <span class="row__status" title={STATUS_BADGE[t.status][1]}><Icon name={STATUS_ICON[t.status]} size={16} /><span class="rows__label">{STATUS_BADGE[t.status][1]}</span></span>
              <span class="ml-mono row__key">{t.key}</span>
              <a class="row__title" href={link(t)} aria-label="{t.key} {t.title}">{t.title}</a>
              <span class="row__meta">
                {@render priority(t)}
                {@render who(t)}
                {#if project_name(t)}<span class="rows__chip"><Icon name="folder-kanban" size={14} />{project_name(t)}</span>{/if}
                <span class="ml-mono row__day">{day(t.updated_at)}</span>
              </span>
              <span class="row__menu"><TaskMoveMenu {t} {on_move} /></span>
            </li>
          {/each}
        </ul>
      </section>
    {:else}
      <div class="sun-empty">No tasks</div>
    {/each}
  </div>
{:else}
  <div class="table-wrap">
    <table class="table">
      <thead>
        <tr>
          {#each TABLE_COLUMNS as [column, label] (column)}
            <th scope="col" aria-sort={sort?.column === column ? (sort.desc ? 'descending' : 'ascending') : undefined}>
              <button type="button" onclick={() => on_sort?.(column)}>
                {label}
                {#if sort?.column === column}<Icon name={sort.desc ? 'arrow-down' : 'arrow-up'} size={14} />{/if}
              </button>
            </th>
          {/each}
        </tr>
      </thead>
      {#each groups as g (g.key)}
        <tbody>
          {#if grouped}
            <tr class="table__group"><th scope="colgroup" colspan={TABLE_COLUMNS.length}>{g.label} <span class="ml-mono rows__count">{g.tasks.length}</span></th></tr>
          {/if}
          {#each g.tasks as t (t.id)}
            {@const [tone, status] = STATUS_BADGE[t.status]}
            <tr data-task-id={t.id}>
              <td><a class="ml-mono" href={link(t)} tabindex="-1">{t.key}</a></td>
              <td class="table__title"><a href={link(t)} aria-label="{t.key} {t.title}">{t.title}</a></td>
              <td><span class="table__status"><Badge {tone}>{status}</Badge><TaskMoveMenu {t} {on_move} /></span></td>
              <td>{@render priority(t)}</td>
              <td>{@render who(t)}</td>
              <td>{project_name(t)}</td>
              <td class="ml-mono">{day(t.created_at)}</td>
              <td class="ml-mono">{day(t.updated_at)}</td>
            </tr>
          {/each}
        </tbody>
      {:else}
        <tbody><tr><td colspan={TABLE_COLUMNS.length} class="sun-empty">No tasks</td></tr></tbody>
      {/each}
    </table>
  </div>
{/if}

<style>
  .rows {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
    padding: var(--space-4);
    display: flex;
    flex-direction: column;
    gap: var(--space-4);
  }
  .rows__head {
    margin: 0 0 var(--space-2);
  }
  .rows__head button {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    min-height: 36px;
    padding: 0 var(--space-2);
    border: 0;
    border-radius: var(--radius-sm);
    background: transparent;
    color: var(--fg-1);
    font: inherit;
    font-size: 14px;
    font-weight: 600;
    cursor: pointer;
  }
  .rows__head button:hover,
  .rows__head button:focus-visible {
    outline: none;
    background: var(--accent-soft);
  }
  .rows__count {
    font-size: 12px;
    font-weight: 400;
    color: var(--fg-2);
  }
  .rows__list {
    list-style: none;
    margin: 0;
    padding: 0;
    border: 1.5px solid var(--border-subtle);
    border-radius: var(--radius-lg);
    background: var(--surface-card);
    overflow: hidden;
  }
  .row {
    position: relative;
    display: flex;
    align-items: center;
    gap: var(--space-3);
    min-height: 44px;
    padding: 0 var(--space-3);
  }
  .row + .row {
    border-top: 1.5px solid var(--border-subtle);
  }
  .row:hover {
    background: var(--accent-soft);
  }
  .row:has(.row__title:focus-visible) {
    box-shadow: inset var(--ring-focus);
  }
  .row__status {
    display: inline-flex;
    color: var(--fg-2);
  }
  .row__key {
    flex: none;
    font-size: 12px;
    color: var(--fg-2);
  }
  .row__title {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    font-size: 14px;
    font-weight: 600;
    color: var(--fg-1);
    text-decoration: none;
  }
  .row__title:focus-visible {
    outline: none;
  }
  .row__title::after {
    content: '';
    position: absolute;
    inset: 0;
  }
  .row__meta {
    display: none;
    align-items: center;
    gap: var(--space-3);
    flex: none;
  }
  .row__day {
    font-size: 12px;
    color: var(--fg-2);
  }
  .row__menu {
    position: relative;
    z-index: 1;
  }
  .rows__priority {
    display: inline-flex;
    align-items: center;
    gap: 4px;
    color: var(--fg-2);
    font-size: 13px;
  }
  .rows__priority.urgent {
    color: var(--danger-fg);
  }
  .row .rows__label {
    position: absolute;
    width: 1px;
    height: 1px;
    overflow: hidden;
    clip-path: inset(50%);
    white-space: nowrap;
  }
  .rows__chip {
    display: inline-flex;
    align-items: center;
    gap: 4px;
    max-width: 180px;
    padding: 2px 8px;
    border-radius: var(--radius-pill);
    background: var(--surface-raised);
    font-size: 12px;
    color: var(--fg-2);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .table-wrap {
    flex: 1;
    min-height: 0;
    overflow: auto;
    margin: var(--space-4);
    border: 1.5px solid var(--border-subtle);
    border-radius: var(--radius-lg);
    background: var(--surface-card);
  }
  .table {
    width: 100%;
    min-width: 900px;
    border-collapse: collapse;
    font-size: 13px;
  }
  .table th,
  .table td {
    padding: var(--space-2) var(--space-3);
    text-align: left;
    border-bottom: 1.5px solid var(--border-subtle);
    white-space: nowrap;
    color: var(--fg-1);
  }
  .table thead th {
    position: sticky;
    top: 0;
    z-index: 1;
    padding: 0;
    background: var(--surface-card);
  }
  .table thead button {
    display: inline-flex;
    align-items: center;
    gap: 4px;
    width: 100%;
    min-height: 40px;
    padding: 0 var(--space-3);
    border: 0;
    background: transparent;
    color: var(--fg-2);
    font: inherit;
    font-weight: 600;
    cursor: pointer;
  }
  .table thead button:hover,
  .table thead button:focus-visible {
    outline: none;
    color: var(--fg-1);
    background: var(--accent-soft);
  }
  .table__group th {
    background: var(--surface-raised);
    font-weight: 600;
  }
  .table a {
    color: var(--fg-1);
    text-decoration: none;
  }
  .table a:hover {
    text-decoration: underline;
  }
  .table__title {
    max-width: 360px;
    overflow: hidden;
    text-overflow: ellipsis;
    font-weight: 600;
  }
  .table__status {
    display: inline-flex;
    align-items: center;
    gap: 4px;
  }
  @media (min-width: 720px) {
    .row__meta {
      display: flex;
    }
  }
  @media (min-width: 1024px) {
    .rows {
      padding: var(--space-5);
    }
    .table-wrap {
      margin: var(--space-5);
    }
  }
</style>
