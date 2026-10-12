<!-- A task card on the board and the swimlanes: draggable; the card's content is the link that opens the
     task, and the "Move to…" menu sits in its corner for keyboard and touch. -->
<script lang="ts">
  import { Badge, Card, Icon } from '@hryer/madlabs-design-system/svelte';
  import TaskMoveMenu from './TaskMoveMenu.svelte';
  import { app, plain_view } from '../lib/app_state.svelte';
  import { assignee_of, current_step, is_paused, money, priority_of, runtime_name, stuck_label } from '../lib/format';
  import { run_repos, type Project, type TaskPatch, type TaskSummary } from '../lib/api/types';

  let {
    t,
    link,
    queue_position,
    projects,
    dragging,
    on_move,
    up,
    down,
    ondragstart,
    ondragend,
  }: {
    t: TaskSummary;
    link: string;
    queue_position: number;
    projects: Project[];
    dragging: boolean;
    on_move: (t: TaskSummary, patch: TaskPatch, from_menu?: boolean) => void;
    up?: () => void;
    down?: () => void;
    ondragstart: (e: DragEvent) => void;
    ondragend: () => void;
  } = $props();

  const stuck = $derived(stuck_label(t));
  const step = $derived(current_step(t));
  const repos = $derived(run_repos(t));
  const plain = $derived(plain_view(repos.length));
  const priority = $derived(priority_of(t.priority));
  const who = $derived(assignee_of(t, app.agents));
  const project = $derived(projects.find((p) => p.id === t.project_id));

  function progress_line() {
    if (t.status === 'done') return t.run?.end_reason === 'unverified' ? 'Done without a gate' : 'All steps done';
    if (t.status === 'failed') return 'Run failed';
    if (!t.workflow && !t.assignee) return 'Nothing runs';
    if (!t.run || !step) return `#${queue_position} in queue · ${who.label}`;
    const i = t.run.steps.indexOf(step);
    const progress = `${step.name} · ${i + 1}/${t.run.steps.length}`;
    // The role view keeps runtimes out of a repo-less task.
    return plain ? progress : `${progress} · ${runtime_name(app.runtimes, t.run.runtime_id)}`;
  }
</script>

<Card padding="none" interactive class="task-card {dragging ? 'is-dragging' : ''}" draggable="true" data-task-id={t.id} {ondragstart} {ondragend}>
  <a class="task-card__link" href={link} draggable="false" aria-label="{t.key} {t.title}">
    <div class="task-card__top">
      {#if t.priority !== 'none'}
        <span class="task-card__priority" class:urgent={t.priority === 'urgent'} title="Priority: {priority.label}">
          <Icon name={priority.icon} size={16} /><span class="task-card__sr">Priority {priority.label}</span>
        </span>
      {/if}
      <span class="ml-mono task-card__key">{t.key}</span>
      <span style="flex:1"></span>
      {#if stuck}<Badge tone="warning" dot>{stuck}</Badge>{:else if is_paused(t)}<Badge tone="warning">Paused</Badge>{/if}
      {#if t.run && t.run.runtime_switches > 0 && t.status !== 'done' && !plain}
        <Badge tone="info">Failover → {runtime_name(app.runtimes, t.run.runtime_id)}</Badge>
      {/if}
      {#if t.run?.end_reason === 'unverified'}<Badge>Unverified</Badge>{/if}
      {#if t.status === 'failed'}<Badge tone="danger">Failed</Badge>{/if}
    </div>
    <div class="task-card__title">{t.title}</div>
    {#if t.run}
      <div class="task-card__bar" aria-hidden="true">
        {#each t.run.steps as s (s.step_key)}
          <span
            class="seg"
            class:done={s.status === 'succeeded'}
            class:now={s === step && !stuck}
            class:stuck={s === step && !!stuck}
            class:sun-pulse={s === step && !stuck && (t.status === 'in_progress' || t.status === 'verifying')}
          ></span>
        {/each}
      </div>
    {/if}
    <div class="task-card__line">{progress_line()}</div>
    <div class="task-card__chips">
      <span class="task-card__chip" title={who.kind === 'agent' ? t.assignee?.id : undefined}><Icon name={who.icon} size={14} />{who.label}</span>
      {#if project}<span class="task-card__chip"><Icon name="folder-kanban" size={14} />{project.name}</span>{/if}
    </div>
    <div class="task-card__foot">
      {#if repos.length}
        <span class="task-card__meta"><Icon name="git-branch" size={14} />{repos.length} repo{repos.length > 1 ? 's' : ''}</span>
      {:else if !plain}
        <span class="task-card__meta"><Icon name="folder" size={14} />No repo</span>
      {/if}
      <span style="flex:1"></span>
      <span class="ml-mono" style="font-size:11px">{money(t.run?.cost_usd ?? 0)}</span>
    </div>
  </a>
  <!-- Outside the link (no button inside a link); sits in the card's top-right corner. -->
  <span class="task-card__menu"><TaskMoveMenu {t} {on_move} {up} {down} /></span>
</Card>

<style>
  /* DS Card (soft, interactive, no padding): the link inside fills it. */
  :global(.task-card) {
    position: relative;
    flex: none;
    cursor: grab;
    animation: ml-pop-in var(--dur-slow) var(--ease-spring);
  }
  .task-card__link {
    display: flex;
    flex-direction: column;
    gap: 10px;
    padding: var(--space-4);
    color: inherit;
    text-decoration: none;
    border-radius: inherit;
    cursor: inherit;
  }
  .task-card__link:focus-visible {
    outline: none;
  }
  :global(.task-card.is-dragging) {
    opacity: 0.45;
  }
  :global(.task-card:has(.task-card__link:focus-visible)) {
    box-shadow: var(--ring-focus);
  }
  .task-card__top {
    display: flex;
    align-items: center;
    gap: 6px;
    flex-wrap: wrap;
    min-height: 32px;
    padding-right: 36px;
  }
  .task-card__priority {
    display: inline-flex;
    color: var(--fg-2);
  }
  .task-card__priority.urgent {
    color: var(--danger-fg);
  }
  .task-card__sr {
    position: absolute;
    width: 1px;
    height: 1px;
    overflow: hidden;
    clip-path: inset(50%);
    white-space: nowrap;
  }
  .task-card__key {
    font-size: 11px;
    color: var(--fg-2);
  }
  .task-card__menu {
    position: absolute;
    top: 12px;
    right: 12px;
  }
  .task-card__title {
    font-size: 15px;
    font-weight: 600;
    line-height: 1.3;
    color: var(--fg-1);
    text-wrap: pretty;
  }
  .task-card__bar {
    display: flex;
    gap: 3px;
  }
  .seg {
    flex: 1;
    height: 5px;
    border-radius: 3px;
    background: var(--surface-raised);
  }
  .seg.done {
    background: var(--mint-400);
  }
  .seg.now {
    background: var(--ice-400);
  }
  .seg.stuck {
    background: var(--amber-400);
  }
  .task-card__line {
    font-size: 12px;
    color: var(--fg-2);
    margin-top: -4px;
  }
  .task-card__chips {
    display: flex;
    flex-wrap: wrap;
    gap: 6px;
  }
  .task-card__chip {
    display: inline-flex;
    align-items: center;
    gap: 4px;
    max-width: 100%;
    padding: 2px 8px;
    border-radius: var(--radius-pill);
    background: var(--surface-raised);
    font-size: 12px;
    color: var(--fg-2);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .task-card__foot {
    display: flex;
    align-items: center;
    gap: 10px;
    font-size: 12px;
    color: var(--fg-2);
    border-top: 1.5px solid var(--border-subtle);
    padding-top: 10px;
  }
  .task-card__meta {
    display: flex;
    align-items: center;
    gap: 4px;
  }
  /* Touch: the menu button grows to 44 px (app.css). */
  @media (max-width: 1023px) {
    .task-card__top {
      padding-right: 44px;
    }
    .task-card__menu {
      top: 8px;
      right: 8px;
    }
  }
</style>
