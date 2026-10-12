<!-- A task's page, #/tasks/<KEY> (or #/projects/<project>/<KEY> inside a project; an old ULID link is
     rewritten to the key). Header: breadcrumb, a live chip while its agent works or waits, Stop (interrupt →
     Blocked). Main column: title and description edited in place (saved on blur, Esc cancels), the actions
     that fit its status, then the Activity, live, with the comment box. Sidebar: Properties (status by its
     legal moves, priority, assignee, project, workflow), Pull requests, Execution log and Details. One column
     on phones. -->
<script lang="ts">
  import { Badge, Button, Dialog } from '@hryer/madlabs-design-system/svelte';
  import { api, watch_activity, watch_events } from '../lib/api/client';
  import { legal_moves, run_repos, type Project, type RunEvent, type TaskActivity as Activity, type TaskDetail, type TaskPatch, type TaskRun, type Workflow } from '../lib/api/types';
  import { act, app, can, error_text, feature_on, load_workspace, plain_view, workspace_of } from '../lib/app_state.svelte';
  import { assignee_items, assignee_name } from '../lib/assignee';
  import { PRIORITIES, STATUS_BADGE, STATUS_ICON, actor_name, priority_of, stamp } from '../lib/format';
  import { go, href, replace } from '../lib/router.svelte';
  import Menu from '../lib/ui/Menu.svelte';
  import PageHeader from '../lib/ui/PageHeader.svelte';
  import PullRequests from '../lib/ui/PullRequests.svelte';
  import TaskActions from './TaskActions.svelte';
  import TaskActivity from './TaskActivity.svelte';
  import TaskRunLog from './TaskRunLog.svelte';

  let { id, project_id }: { id: string; project_id?: string } = $props();

  let task = $state<TaskDetail | null>(null);
  let runs = $state<TaskRun[]>([]);
  let error = $state('');
  let workflows = $state<Workflow[]>([]);
  let projects = $state<Project[]>([]);

  const workspace = $derived(task ? workspace_of(task.workspace_id) : undefined);
  /** The role view: a repo-less task seen by a non-technical role shows no repos, branches or runtimes. */
  const plain = $derived(!!task && plain_view(run_repos(task).length));
  const back = () => (project_id ? go('projects', project_id) : go('tasks'));

  // Reload whenever the task or the workspace's list changes (an action elsewhere refreshes app.tasks).
  $effect(() => {
    const ref = id;
    void app.tasks;
    load(ref);
  });

  async function load(ref: string) {
    try {
      const detail = await api.get_task(ref);
      const all = await api.task_runs(detail.id).catch(() => []);
      if (ref !== id) return;
      [task, runs, error] = [detail, all, ''];
      // An old link names the task by its ULID: show its key instead.
      if (ref.toUpperCase() !== detail.key.toUpperCase()) project_id ? replace('projects', project_id, detail.key) : replace('tasks', detail.key);
    } catch (e) {
      error = error_text(e);
    }
  }

  // The workspace's workflows and projects, for the Assignee, Project and Activity names.
  const workspace_id = $derived(task?.workspace_id);
  $effect(() => {
    const ws = workspace_id;
    if (!ws) return;
    api.list_workflows(ws).then((w) => (workflows = w), () => {});
    api.list_projects(ws).then((p) => (projects = p), () => {});
  });

  // Live timeline for the latest run (SSE, resumes by seq). A new event can move the task,
  // so it also refreshes the task list, which reloads this page.
  let events = $state<RunEvent[]>([]);
  let live = $state(false);
  const run_id = $derived(task?.run?.id);
  let refresh: ReturnType<typeof setTimeout> | undefined;
  const refresh_soon = () => {
    clearTimeout(refresh);
    refresh = setTimeout(load_workspace, 300);
  };
  $effect(() => {
    const rid = run_id;
    events = [];
    if (!rid) return;
    live = true;
    const stop = watch_events(
      rid,
      0,
      (e) => {
        if (e.seq <= (events.at(-1)?.seq ?? 0)) return;
        events.push(e);
        refresh_soon();
      },
      () => {
        live = false;
        load_workspace();
      },
    );
    return () => {
      stop();
      live = false;
    };
  });

  // Live activity: comments, moves, runs and pull requests. A run or a move changes the task too;
  // a PR event (a webhook) or a move refreshes the Pull requests section.
  let activity = $state<Activity[]>([]);
  let pr_version = $state(0);
  const task_id = $derived(task?.id);
  function take(a: Activity) {
    if (a.seq <= (activity.at(-1)?.seq ?? 0)) return;
    activity.push(a);
    if (a.kind !== 'comment') refresh_soon();
    if (a.kind === 'pull_request' || a.kind === 'status') pr_version++;
  }
  $effect(() => {
    const tid = task_id;
    activity = [];
    if (!tid) return;
    return watch_activity(tid, 0, take, () => {});
  });

  /** Who runs it: its agent's or workflow's name. */
  const runner = $derived(task ? (task.assignee ? assignee_name(task.assignee, workflows, app.agents) : (task.workflow?.name ?? '')) : '');
  const working = $derived(task?.run?.status === 'running');
  const queued = $derived(!!task && !!(task.assignee || task.workflow) && (task.run?.status === 'queued' || (!task.run && task.status === 'todo')));

  /** Applies `patch`; the kernel's refusal (an illegal move, say) comes back as a toast. */
  const patch = (change: TaskPatch, done?: string) => task && act(() => api.update_task(task!.id, change), done);

  let confirm_stop = $state(false);
  function stop() {
    confirm_stop = false;
    if (task) act(() => api.interrupt_task(task!.id), `${task.key} stopped`);
  }

  // Title and description, edited in place: saved on blur, Esc puts the saved text back.
  let title = $state('');
  let editing_title = $state(false);
  let body = $state('');
  let editing_body = $state(false);
  $effect(() => {
    if (task && !editing_title) title = task.title;
  });
  $effect(() => {
    if (task && !editing_body) body = task.body;
  });

  function title_key(e: KeyboardEvent) {
    const el = e.currentTarget as HTMLElement;
    if (e.key === 'Enter') {
      e.preventDefault();
      el.blur();
    } else if (e.key === 'Escape') {
      e.stopPropagation();
      title = task?.title ?? '';
      editing_title = false;
      el.blur();
    }
  }

  /** Keeps the edit on screen until the saved task comes back (no flash of the old text). */
  async function save_title() {
    if (!editing_title || !task) return;
    const next = title.replace(/\s+/g, ' ').trim();
    if (next && next !== task.title) await patch({ title: next });
    editing_title = false;
  }

  function body_key(e: KeyboardEvent) {
    if (e.key !== 'Escape') return;
    e.stopPropagation();
    body = task?.body ?? '';
    editing_body = false;
    (e.currentTarget as HTMLElement).blur();
  }

  async function save_body() {
    if (!editing_body || !task) return;
    if (body !== task.body) await patch({ body });
    editing_body = false;
  }

  /** Who created it, from the log's first entry (the engine when no person did). */
  const created_by = $derived(actor_name(activity.find((a) => a.kind === 'created')?.body.by, app.me?.user));
</script>

{#if error}
  <PageHeader title="Task" />
  <div class="sun-page"><div class="sun-empty" role="alert">{error} <Button size="sm" variant="ghost" onclick={back}>Back to tasks</Button></div></div>
{:else if task}
  {@const t = task}
  {@const [tone, label] = STATUS_BADGE[t.status]}
  {@const project = projects.find((p) => p.id === t.project_id)}
  <PageHeader>
    {#snippet lead()}
      <nav class="crumbs" aria-label="Breadcrumb">
        <span class="crumbs__dim">{workspace?.name ?? 'Workspace'}</span>
        <span class="crumbs__sep" aria-hidden="true">›</span>
        {#if project_id}
          <a href={href('projects', project_id)}>{projects.find((p) => p.id === project_id)?.name ?? 'Project'}</a>
        {:else}
          <a href={href('tasks')}>Tasks</a>
        {/if}
        <span class="crumbs__sep" aria-hidden="true">›</span>
        <span class="ml-mono" aria-current="page">{t.key}</span>
      </nav>
    {/snippet}
    {#if working}
      <Badge tone="info" dot live>{runner} is working</Badge>
    {:else if queued}
      <Badge tone="neutral" dot>{runner} · queued</Badge>
    {/if}
    {#if run_repos(t).length && feature_on('canvas')}
      <Button size="sm" variant="secondary" iconLeft="monitor-smartphone" href={href('canvas', t.id)}>Open canvas</Button>
    {/if}
    {#if working && can('run')}
      <Button size="sm" variant="secondary" iconLeft="square" onclick={() => (confirm_stop = true)}>Stop</Button>
    {/if}
  </PageHeader>

  <div class="task-page">
    <article class="task-page__main" aria-labelledby="task-title">
      <div class="task-page__status">
        <Badge {tone} dot>{label}</Badge>
      </div>
      {#if can('run')}
        <h1
          id="task-title"
          class="task-page__title"
          contenteditable="plaintext-only"
          spellcheck="true"
          aria-describedby="title-hint"
          bind:textContent={title}
          onfocus={() => (editing_title = true)}
          onblur={save_title}
          onkeydown={title_key}
        ></h1>
        <span id="title-hint" class="sun-sr-only">Edit the title; Enter saves, Escape cancels.</span>
        <textarea
          class="task-page__body"
          aria-label="Description"
          placeholder="Add description…"
          maxlength={65536}
          bind:value={body}
          onfocus={() => (editing_body = true)}
          onblur={save_body}
          onkeydown={body_key}
        ></textarea>
      {:else}
        <h1 id="task-title" class="task-page__title">{t.title}</h1>
        {#if t.body}<p class="task-page__body task-page__body--read">{t.body}</p>{/if}
      {/if}

      <TaskActions task={t} {events} {plain} />

      <TaskActivity task={t} {activity} {runs} {runner} {workflows} {projects} on_comment={take} />
    </article>

    <aside class="task-page__side" aria-label="Task details">
      <section aria-labelledby="props-head">
        <h2 class="sun-label" id="props-head">Properties</h2>
        <dl class="props">
          <dt>Status</dt>
          <dd>
            <Menu
              readonly={!can('run')}
              label="Status"
              icon={STATUS_ICON[t.status]}
              text={label}
              items={[t.status, ...legal_moves(t)].map((s) => ({
                label: STATUS_BADGE[s][1],
                icon: STATUS_ICON[s],
                checked: s === t.status,
                onselect: () => s !== t.status && patch({ status: s }, `${t.key} moved to ${STATUS_BADGE[s][1]}`),
              }))}
            />
          </dd>
          <dt>Priority</dt>
          <dd>
            <Menu
              readonly={!can('run')}
              label="Priority"
              icon={priority_of(t.priority).icon}
              text={priority_of(t.priority).label}
              items={PRIORITIES.map((p) => ({ label: p.label, icon: p.icon, checked: p.value === t.priority, onselect: () => p.value !== t.priority && patch({ priority: p.value }) }))}
            />
          </dd>
          <dt>Assignee</dt>
          <dd>
            <Menu
              readonly={!can('run')}
              label="Assignee"
              icon={!t.assignee ? 'minus' : t.assignee.kind === 'agent' ? 'bot' : 'workflow'}
              text={t.assignee ? runner : 'None'}
              items={assignee_items(workflows, app.agents, workspace?.default_workflow_id ?? null, t.assignee, (a) => patch({ assignee: a }), can('edit_workflow') ? (wf) => go('workflows', wf) : undefined)}
            />
          </dd>
          <dt>Project</dt>
          <dd>
            <Menu
              readonly={!can('run')}
              label="Project"
              icon="folder-kanban"
              text={project?.name ?? 'No project'}
              items={[
                ...projects.map((p) => ({ label: p.name, icon: 'folder-kanban', checked: p.id === t.project_id, onselect: () => p.id !== t.project_id && patch({ project_id: p.id }) })),
                null,
                { label: 'No project', icon: 'minus', checked: !t.project_id, onselect: () => t.project_id && patch({ project_id: null }) },
              ]}
            />
          </dd>
          <!-- An agent's task runs a hidden system workflow: the Assignee row already says who. -->
          {#if t.assignee?.kind !== 'agent'}
            <dt>Workflow</dt>
            <dd class="props__text">
              {#if t.workflow}{t.workflow.name} <span class="ml-mono crumbs__dim">v{t.run?.workflow_version ?? t.workflow.version}</span>{:else}None{/if}
            </dd>
          {/if}
        </dl>
      </section>

      {#if !plain && (workspace?.git_settings.pr_sidebar ?? true)}
        <PullRequests task={t} refresh={pr_version} auto_link={workspace?.git_settings.auto_link ?? true} />
      {/if}

      <TaskRunLog task={t} {runs} {events} {live} {plain} />

      <section aria-labelledby="details-head">
        <h2 class="sun-label" id="details-head">Details</h2>
        <dl class="props">
          <dt>Created by</dt>
          <dd class="props__text">{created_by}</dd>
          <dt>Created</dt>
          <dd class="props__text"><time datetime={t.created_at}>{stamp(t.created_at)}</time></dd>
          <dt>Updated</dt>
          <dd class="props__text"><time datetime={t.updated_at}>{stamp(t.updated_at)}</time></dd>
        </dl>
      </section>
    </aside>
  </div>

  <Dialog bind:open={confirm_stop} onclose={() => (confirm_stop = false)} title="Stop {runner}?">
    <p style="margin:0;color:var(--fg-2)">The agent stops now and {t.key} moves to Blocked. Comment or move it to Todo to start again.</p>
    {#snippet footer()}
      <Button variant="secondary" onclick={() => (confirm_stop = false)}>Keep working</Button>
      <Button variant="danger" iconLeft="square" onclick={stop}>Stop</Button>
    {/snippet}
  </Dialog>
{:else}
  <PageHeader title="Task" />
  <div class="sun-page"><div class="sun-empty" role="status">Loading…</div></div>
{/if}

<style>
  .crumbs {
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: var(--space-2);
    min-width: 0;
    font-size: 14px;
    color: var(--fg-1);
  }
  .crumbs a {
    color: var(--fg-link);
    font-weight: 600;
  }
  .crumbs__dim {
    color: var(--fg-2);
    font-size: 13px;
  }
  .crumbs__sep {
    color: var(--fg-3);
  }
  .task-page {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
    display: flex;
    flex-direction: column;
  }
  .task-page__main,
  .task-page__side {
    display: flex;
    flex-direction: column;
    gap: var(--space-5);
    padding: var(--space-4);
    min-width: 0;
    box-sizing: border-box;
  }
  .task-page__side {
    border-top: 1.5px solid var(--border-subtle);
  }
  .task-page__status {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    margin-bottom: calc(-1 * var(--space-3));
  }
  .task-page__title {
    margin: 0;
    padding: 2px 6px;
    margin-inline: -6px;
    border-radius: var(--radius-sm);
    font-family: var(--font-heading);
    font-weight: 400;
    font-size: 30px;
    line-height: 1.1;
    color: var(--fg-1);
    text-wrap: balance;
    overflow-wrap: anywhere;
  }
  .task-page__title[contenteditable]:hover,
  .task-page__body:hover {
    background: var(--bg-subtle);
  }
  .task-page__title[contenteditable]:focus,
  .task-page__body:focus {
    outline: none;
    box-shadow: var(--ring-focus);
    background: transparent;
  }
  .task-page__body {
    margin: calc(-1 * var(--space-2)) -6px 0;
    padding: 6px;
    min-height: 60px;
    border: 0;
    border-radius: var(--radius-sm);
    background: transparent;
    color: var(--fg-1);
    font: inherit;
    font-size: 15px;
    line-height: 1.55;
    resize: none;
    field-sizing: content;
  }
  .task-page__body::placeholder {
    color: var(--fg-3);
  }
  .task-page__body--read {
    white-space: pre-wrap;
    min-height: 0;
  }
  .props {
    display: grid;
    grid-template-columns: 88px minmax(0, 1fr);
    align-items: center;
    gap: var(--space-1) var(--space-3);
    margin: 0;
    font-size: 13px;
  }
  .props dt {
    color: var(--fg-2);
  }
  .props dd {
    margin: 0;
    min-width: 0;
  }
  /* A long workflow name wraps inside its menu button instead of running out of the sidebar. */
  .props dd :global(.ml-btn) {
    max-width: 100%;
    height: auto;
    min-height: var(--control-sm);
    white-space: normal;
    text-align: left;
  }
  .props__text {
    padding: var(--space-1) 0;
    color: var(--fg-1);
    overflow-wrap: anywhere;
  }
  /* Touch: 44 px targets (the breadcrumb links, the property menus). */
  @media (max-width: 1023px) {
    .crumbs a {
      display: inline-flex;
      align-items: center;
      justify-content: center;
      min-width: 44px;
      min-height: 44px;
    }
    .props dd :global(.ml-btn) {
      min-height: var(--control-md);
    }
  }
  /* Wide: the main column and a sidebar, each scrolling on its own; the comment box stays in view. */
  @media (min-width: 900px) {
    .task-page {
      overflow: hidden;
      display: grid;
      grid-template-columns: minmax(0, 1fr) 360px;
    }
    .task-page__main,
    .task-page__side {
      overflow-y: auto;
      padding: var(--space-5);
    }
    .task-page__main {
      padding-bottom: 0;
    }
    .task-page__side {
      border-top: 0;
      border-left: 1.5px solid var(--border-subtle);
    }
  }
</style>
