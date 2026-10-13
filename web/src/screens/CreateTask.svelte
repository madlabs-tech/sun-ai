<!-- New task (the create dialog): title and description, then pills for Status (Backlog or Todo),
     Priority, Assignee (workflows, the default first; agents with their readiness; None) and Project. The
     exec env sits under More once the assignee runs something. A line says what happens on create. "Create
     another" keeps the dialog open; ⌘↵ / Ctrl+↵ creates. From a project's page the task goes into that
     project and you stay there. Repositories are the workspace's (Settings → Code): the run resolves them
     from the task text, so there is nothing to pick here. -->
<script lang="ts">
  import { tick, untrack } from 'svelte';
  import { Button, Dialog, Input, Switch } from '@hryer/madlabs-design-system/svelte';
  import { api } from '../lib/api/client';
  import type { Assignee, ExecEnvSpec, Priority, Project, Workflow } from '../lib/api/types';
  import ExecEnvPicker from '../lib/ui/ExecEnvPicker.svelte';
  import Menu from '../lib/ui/Menu.svelte';
  import TextArea from '../lib/ui/TextArea.svelte';
  import { SIDEBAR_SHORTCUT, app, can, current_workspace, error_text, load_workspace, toast, workspace_of } from '../lib/app_state.svelte';
  import { assignee_items, assignee_name } from '../lib/assignee';
  import { PRIORITIES, STATUS_BADGE, STATUS_ICON, priority_of } from '../lib/format';
  import { go } from '../lib/router.svelte';

  let { open = $bindable(false), project_id }: { open?: boolean; project_id?: string } = $props();

  const workspace = $derived(workspace_of(current_workspace()));
  let workflows = $state<Workflow[]>([]);
  let projects = $state<Project[]>([]);

  let title = $state('');
  let body = $state('');
  let status = $state<'todo' | 'backlog'>('todo');
  let priority = $state<Priority>('none');
  let assignee = $state<Assignee | null>(null);
  let project = $state<string | null>(null);
  let exec_env = $state<ExecEnvSpec | null>(null);
  let another = $state(false);
  let error = $state('');
  let creating = $state(false);

  // Each opening starts from the workspace's defaults (and the project page it was opened on).
  $effect(() => {
    if (open) untrack(reset);
  });

  async function reset() {
    [title, body, status, priority, project, exec_env, error] = ['', '', 'todo', 'none', project_id ?? null, null, ''];
    const id = current_workspace();
    assignee = workspace?.default_workflow_id ? { kind: 'workflow', id: workspace.default_workflow_id } : null;
    // The dialog doesn't move focus itself: start in the title.
    tick().then(() => document.getElementById('new-task-title')?.focus());
    try {
      [workflows, projects] = await Promise.all([api.list_workflows(id || undefined), id ? api.list_projects(id) : []]);
    } catch (e) {
      error = error_text(e);
    }
  }

  const who = $derived(assignee_name(assignee, workflows, app.agents));
  const hint = $derived(
    !assignee ? 'A plain task: nothing runs.' : status === 'backlog' ? 'Won’t start while in Backlog.' : `${who} will start right after creation.`,
  );
  const assignee_icon = $derived(!assignee ? 'minus' : assignee.kind === 'agent' ? 'bot' : 'workflow');
  const project_name = $derived(projects.find((p) => p.id === project)?.name ?? 'No project');

  async function create(e?: Event) {
    e?.preventDefault();
    if (creating) return;
    if (!title.trim()) {
      error = 'Say what needs doing.';
      return;
    }
    creating = true;
    try {
      // Snapshots: the mock clones what it stores, and a state proxy can't be cloned.
      const task = await api.create_task($state.snapshot({
        title: title.trim(),
        body,
        workspace_id: current_workspace() || undefined,
        status,
        priority,
        project_id: project ?? undefined,
        assignee,
        exec_env: assignee ? exec_env : null,
      }));
      await load_workspace();
      toast('success', `${task.key} created`, hint);
      if (another) {
        [title, body, error] = ['', '', ''];
        await tick();
        document.getElementById('new-task-title')?.focus();
      } else {
        open = false;
        if (!project_id) go('tasks', task.key);
      }
    } catch (err) {
      error = error_text(err);
    }
    creating = false;
  }

  const SUBMIT_KEY = SIDEBAR_SHORTCUT.startsWith('⌘') ? '⌘↵' : 'Ctrl+↵';

  /** ⌘↵ / Ctrl+↵ anywhere in the dialog creates the task. */
  function shortcut(e: KeyboardEvent) {
    if (e.key === 'Enter' && (e.metaKey || e.ctrlKey)) create(e);
  }
</script>

<Dialog bind:open onclose={() => (open = false)} title="{workspace?.name ?? 'Workspace'} › New task" size="lg">
  <!-- svelte-ignore a11y_no_noninteractive_element_interactions: ⌘↵ submits from any field -->
  <form id="new-task" class="create" onsubmit={create} onkeydown={shortcut} novalidate>
    <Input id="new-task-title" aria-label="Title" placeholder="Task title" maxlength={200} bind:value={title} error={error || undefined} />
    <TextArea aria-label="Description" placeholder="Add description…" rows={4} bind:value={body} />

    <div class="create__pills" role="group" aria-label="Properties">
      <Menu
        label="Status"
        icon={STATUS_ICON[status]}
        text={STATUS_BADGE[status][1]}
        variant="secondary"
        items={(['backlog', 'todo'] as const).map((s) => ({ label: STATUS_BADGE[s][1], icon: STATUS_ICON[s], checked: s === status, onselect: () => (status = s) }))}
      />
      <Menu
        label="Priority"
        icon={priority_of(priority).icon}
        text={priority_of(priority).label}
        variant="secondary"
        items={PRIORITIES.map((p) => ({ label: p.label, icon: p.icon, checked: p.value === priority, onselect: () => (priority = p.value) }))}
      />
      <Menu
        label="Assignee"
        icon={assignee_icon}
        text={who}
        variant="secondary"
        items={assignee_items(workflows, app.agents, workspace?.default_workflow_id ?? null, assignee, (a) => (assignee = a), can('edit_workflow') ? (id) => ((open = false), go('workflows', id)) : undefined)}
      />
      <Menu
        label="Project"
        icon="folder-kanban"
        text={project_name}
        variant="secondary"
        items={[
          ...projects.map((p) => ({ label: p.name, icon: 'folder-kanban', checked: p.id === project, onselect: () => (project = p.id) })),
          null,
          { label: 'No project', icon: 'minus', checked: project === null, onselect: () => (project = null) },
        ]}
      />
    </div>

    {#if assignee}
      <details class="create__more">
        <summary class="ml-field__label">More</summary>
        {#if open}<ExecEnvPicker bind:value={exec_env} />{/if}
      </details>
    {/if}

    <p class="create__hint" role="status">{hint}</p>
  </form>
  {#snippet footer()}
    <div class="create__another"><Switch label="Create another" bind:checked={another} /></div>
    <Button variant="ice" type="submit" form="new-task" loading={creating} aria-keyshortcuts="Meta+Enter Control+Enter">
      Create task <kbd class="create__kbd" aria-hidden="true">{SUBMIT_KEY}</kbd>
    </Button>
  {/snippet}
</Dialog>

<style>
  .create {
    display: flex;
    flex-direction: column;
    gap: 14px;
  }
  .create__pills {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-2);
  }
  .create__more > summary {
    cursor: pointer;
    min-height: 44px;
    display: flex;
    align-items: center;
    gap: 4px;
  }
  .create__more[open] > summary {
    min-height: 0;
    margin-bottom: 4px;
  }
  .create__hint {
    margin: 0;
    font-size: 13px;
    color: var(--fg-2);
  }
  .create__another {
    margin-right: auto;
    display: flex;
    align-items: center;
    white-space: nowrap;
  }
  /* Phones stretch every footer item; the switch keeps its own width. */
  :global(.ml-dialog__footer > .create__another) {
    flex: none;
  }
  .create__kbd {
    margin-left: 6px;
    font: inherit;
    font-size: 12px;
    opacity: 0.75;
  }
  @media (max-width: 759px) {
    .create__kbd {
      display: none;
    }
  }
</style>
