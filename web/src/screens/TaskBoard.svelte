<!-- Board and Swimlane views: a column per status, once (Board) or per lane (Swimlane, lanes by
     assignee, project or priority). Native HTML5 drag and drop: across columns is a status move, across
     lanes sets the lane's project or priority, inside a cell sets the manual order (`position` between
     the neighbours). Across assignee lanes, an agent's lane assigns that agent; workflow and None lanes
     take only their own cards (a workflow is picked before the first run, on the task page). -->
<script lang="ts" module>
  import type { TaskSummary } from '../lib/api/types';

  export interface Lane {
    key: string;
    label: string;
    icon?: string;
    tasks: TaskSummary[];
  }

  /** The position that puts a task at index `k` of `rest` (its column without it): between its new neighbours. */
  export function position_at(rest: TaskSummary[], k: number, fallback: number): number {
    const [prev, next] = [rest[k - 1], rest[k]];
    // ponytail: midpoints halve the gap each time; ~50 drops into one gap reach float precision, then the server should renumber.
    if (prev && next) return (prev.position + next.position) / 2;
    if (prev) return prev.position + 1;
    return next ? next.position - 1 : fallback;
  }
</script>

<script lang="ts">
  import { Icon } from '@hryer/madlabs-design-system/svelte';
  import { legal_moves, type Priority, type Project, type TaskPatch, type TaskStatus } from '../lib/api/types';
  import { COLUMNS, assignee_of } from '../lib/format';
  import { app } from '../lib/app_state.svelte';
  import TaskCard from './TaskCard.svelte';

  let {
    lanes,
    statuses,
    lane_field,
    manual,
    projects,
    link,
    on_move,
  }: {
    lanes: Lane[];
    statuses: TaskStatus[];
    /** '' for the plain board (one lane, no lane header). */
    lane_field: '' | 'assignee' | 'project' | 'priority';
    /** Manual ordering: drops and Move up/down set `position`. */
    manual: boolean;
    projects: Project[];
    link: (t: TaskSummary) => string;
    on_move: (t: TaskSummary, patch: TaskPatch, from_menu?: boolean) => void;
  } = $props();

  const columns = $derived(statuses.map((s) => COLUMNS.find((c) => c.status === s)!));
  let drag = $state<TaskSummary | null>(null);
  let over = $state<{ cell: string; index: number } | null>(null);

  const lane_key = (t: TaskSummary) =>
    lane_field === 'priority' ? t.priority : lane_field === 'project' ? (t.project_id ?? '') : lane_field === 'assignee' ? assignee_of(t, app.agents).key : '';
  const accepts = (lane: Lane) => !!drag && (lane_field !== 'assignee' || lane.key === lane_key(drag) || lane.key.startsWith('agent:'));
  const illegal = (status: TaskStatus) => !!drag && drag.status !== status && !legal_moves(drag).includes(status);

  function start(t: TaskSummary, e: DragEvent) {
    drag = t;
    if (!e.dataTransfer) return;
    e.dataTransfer.effectAllowed = 'move';
    e.dataTransfer.setData('text/plain', t.key);
  }

  function end() {
    drag = null;
    over = null;
  }

  /** Accepts the drag (an illegal status too: the kernel's 409 says why) and tracks the insertion index. */
  function over_cell(e: DragEvent, cell: string, lane: Lane) {
    if (!accepts(lane)) return;
    e.preventDefault();
    if (e.dataTransfer) e.dataTransfer.dropEffect = 'move';
    const cards = [...(e.currentTarget as HTMLElement).querySelectorAll('[data-task-id]')];
    const index = cards.findIndex((c) => {
      const r = c.getBoundingClientRect();
      return e.clientY < r.top + r.height / 2;
    });
    over = { cell, index: index < 0 ? cards.length : index };
  }

  function leave(e: DragEvent, cell: string) {
    if (over?.cell === cell && !(e.currentTarget as HTMLElement).contains(e.relatedTarget as Node)) over = null;
  }

  function drop(e: DragEvent, lane: Lane, status: TaskStatus, list: TaskSummary[]) {
    e.preventDefault();
    const t = drag;
    const at = over?.index ?? list.length;
    end();
    if (!t) return;
    const patch: TaskPatch = {};
    if (t.status !== status) patch.status = status;
    if (lane_field === 'priority' && lane.key !== t.priority) patch.priority = lane.key as Priority;
    if (lane_field === 'project' && lane.key !== (t.project_id ?? '')) patch.project_id = lane.key || null;
    if (lane_field === 'assignee' && lane.key !== lane_key(t) && lane.key.startsWith('agent:')) patch.assignee = { kind: 'agent', id: lane.key.slice(6) };
    if (manual) {
      const rest = list.filter((x) => x.id !== t.id);
      const k = list.slice(0, at).filter((x) => x.id !== t.id).length;
      const moved = Object.keys(patch).length > 0 || list.findIndex((x) => x.id === t.id) !== k;
      if (moved) patch.position = position_at(rest, k, t.position);
    }
    if (Object.keys(patch).length) on_move(t, patch);
  }

  /** Move up / Move down: one place along the cell's manual order. */
  function step(list: TaskSummary[], i: number, by: -1 | 1) {
    const t = list[i];
    if (!manual || !list[i + by]) return undefined;
    return () => on_move(t, { position: position_at(list.filter((x) => x.id !== t.id), i + by, t.position) }, true);
  }
</script>

{#snippet cell(lane: Lane, col: (typeof columns)[number], titled: boolean)}
  {@const key = `${lane.key}|${col.status}`}
  {@const list = lane.tasks.filter((t) => t.status === col.status)}
  <section
    class="task-col"
    class:is-over={over?.cell === key}
    class:is-illegal={illegal(col.status)}
    aria-labelledby={titled ? `col-${col.status}` : undefined}
    aria-label={titled ? undefined : `${lane.label} · ${col.label}`}
    ondragover={(e) => over_cell(e, key, lane)}
    ondragleave={(e) => leave(e, key)}
    ondrop={(e) => drop(e, lane, col.status, list)}
  >
    {#if titled}
      <header class="task-col__head">
        <h2 id="col-{col.status}" class="task-col__title">{col.label}</h2>
        <span class="ml-mono task-col__count" class:hot={col.status === 'needs_you' && list.length > 0}>{list.length}</span>
        <span style="flex:1"></span>
        <span class="task-col__hint">{col.hint}</span>
      </header>
    {/if}
    <div class="task-col__list">
      {#each list as t, i (t.id)}
        {#if manual && over?.cell === key && over.index === i}<div class="drop-line" aria-hidden="true"></div>{/if}
        <TaskCard
          {t}
          link={link(t)}
          queue_position={i + 1}
          {projects}
          dragging={drag?.id === t.id}
          {on_move}
          up={step(list, i, -1)}
          down={step(list, i, 1)}
          ondragstart={(e) => start(t, e)}
          ondragend={end}
        />
      {:else}
        {#if titled}<div class="sun-empty">Nothing here</div>{/if}
      {/each}
      {#if manual && over?.cell === key && over.index === list.length && list.length}<div class="drop-line" aria-hidden="true"></div>{/if}
    </div>
  </section>
{/snippet}

{#if !lane_field}
  <div class="task-columns">
    {#each columns as col (col.status)}
      {@render cell(lanes[0] ?? { key: '', label: '', tasks: [] }, col, true)}
    {/each}
  </div>
{:else}
  <div class="lanes" style="--cols:{columns.length}">
    <div class="lanes__head" aria-hidden="true">
      {#each columns as col (col.status)}<span class="task-col__title">{col.label}</span>{/each}
    </div>
    {#each lanes as lane (lane.key)}
      <section class="lane" aria-labelledby="lane-{lane.key}">
        <h2 class="lane__title" id="lane-{lane.key}">
          {#if lane.icon}<Icon name={lane.icon} size={16} />{/if}{lane.label}<span class="ml-mono task-col__count">{lane.tasks.length}</span>
        </h2>
        <div class="lane__cells">
          {#each columns as col (col.status)}{@render cell(lane, col, false)}{/each}
        </div>
      </section>
    {/each}
  </div>
{/if}

<style>
  .task-columns {
    flex: 1;
    min-height: 0;
    display: flex;
    gap: var(--space-4);
    padding: var(--space-4);
    overflow-x: auto;
    overflow-y: hidden;
    scroll-snap-type: x mandatory;
    scroll-padding: var(--space-4);
    overscroll-behavior-x: contain;
  }
  .task-col {
    width: min(284px, calc(100vw - 56px));
    flex: none;
    display: flex;
    flex-direction: column;
    min-height: 0;
    scroll-snap-align: start;
    border-radius: var(--radius-lg);
    outline: 2px dashed transparent;
    outline-offset: 2px;
    transition: outline-color var(--dur-fast) var(--ease-out), opacity var(--dur-fast) var(--ease-out);
  }
  .task-col.is-over {
    outline-color: var(--ice-400);
    background: var(--accent-soft);
  }
  .task-col.is-illegal {
    opacity: 0.6;
  }
  .task-col.is-illegal.is-over {
    outline-color: var(--danger-fg);
    background: transparent;
  }
  .task-col__head {
    display: flex;
    align-items: baseline;
    gap: var(--space-2);
    padding: 0 4px var(--space-3);
  }
  .task-col__title {
    margin: 0;
    font-family: var(--font-condensed);
    font-weight: 400;
    font-size: 21px;
    line-height: 1;
    color: var(--fg-1);
  }
  .task-col__count {
    font-size: 12px;
    color: var(--fg-2);
    background: var(--border-subtle);
    padding: 1px 8px;
    border-radius: 8px;
  }
  .task-col__count.hot {
    background: var(--amber-400);
    color: var(--on-amber);
  }
  .task-col__hint {
    font-size: 12px;
    color: var(--fg-3);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .task-col__list {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
    display: flex;
    flex-direction: column;
    gap: 10px;
    padding: 4px;
    margin: -4px;
  }
  .drop-line {
    flex: none;
    height: 3px;
    margin: -6px 0;
    border-radius: 2px;
    background: var(--ice-400);
  }

  /* Swimlanes: one scroller; lane titles stick to the left, column titles to the top. */
  .lanes {
    flex: 1;
    min-height: 0;
    overflow: auto;
    padding: 0 var(--space-4) var(--space-4);
    --col: min(260px, calc(100vw - 56px));
  }
  .lanes__head,
  .lane__cells {
    display: grid;
    grid-template-columns: repeat(var(--cols), var(--col));
    gap: var(--space-4);
    width: max-content;
  }
  .lanes__head {
    position: sticky;
    top: 0;
    z-index: 2;
    padding: var(--space-4) 0 var(--space-2);
    background: var(--bg-page);
  }
  .lanes__head .task-col__title {
    font-size: 18px;
  }
  .lane {
    padding-bottom: var(--space-4);
    border-bottom: 1.5px solid var(--border-subtle);
    margin-bottom: var(--space-3);
  }
  .lane__title {
    position: sticky;
    left: 0;
    display: inline-flex;
    align-items: center;
    gap: var(--space-2);
    margin: 0 0 var(--space-2);
    font-size: 14px;
    font-weight: 600;
    color: var(--fg-1);
  }
  /* A swimlane cell: a quiet dashed drop area, no "Nothing here" in every empty one. */
  .lane .task-col {
    width: auto;
    min-height: 72px;
    padding: 4px;
    border: 1.5px dashed var(--border-subtle);
  }
  .lane .task-col__list {
    overflow: visible;
  }
  @media (min-width: 1024px) {
    .task-columns {
      padding: 18px var(--space-5) var(--space-5);
      scroll-snap-type: none;
    }
    .lanes {
      padding: 0 var(--space-5) var(--space-5);
    }
  }
</style>
