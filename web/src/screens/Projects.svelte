<!-- Projects: the workspace's projects with their open and done task counts. New project, and Edit or
     Delete from each row's menu (deleting keeps the tasks, outside any project). Opening one shows the
     Tasks views of its tasks (#/projects/<id>, App.svelte). -->
<script lang="ts">
  import { Button, Dialog, Icon, Input } from '@hryer/madlabs-design-system/svelte';
  import Menu from '../lib/ui/Menu.svelte';
  import PageHeader from '../lib/ui/PageHeader.svelte';
  import { api } from '../lib/api/client';
  import type { Project } from '../lib/api/types';
  import { app, can, current_workspace, error_text, load_workspace, toast } from '../lib/app_state.svelte';
  import { href } from '../lib/router.svelte';

  let projects = $state<Project[] | null>(null);
  let load_error = $state('');

  async function reload() {
    try {
      projects = await api.list_projects(current_workspace());
      load_error = '';
    } catch (e) {
      load_error = error_text(e);
    }
  }
  $effect(() => {
    if (current_workspace()) reload();
  });

  const counts = (p: Project) => {
    const mine = app.tasks.filter((t) => t.project_id === p.id);
    return { open: mine.filter((t) => t.status !== 'done' && t.status !== 'cancelled').length, done: mine.filter((t) => t.status === 'done').length, all: mine.length };
  };

  /** The dialog: a new project (`null`), one being edited, or closed (`undefined`). */
  let editing = $state<Project | null | undefined>(undefined);
  let form = $state({ name: '', description: '' });
  let form_error = $state('');
  let busy = $state(false);
  let deleting = $state<Project | null>(null);
  let delete_error = $state('');

  function open_form(p: Project | null) {
    editing = p;
    form = { name: p?.name ?? '', description: p?.description ?? '' };
    form_error = '';
  }

  async function save(e?: Event) {
    e?.preventDefault();
    const name = form.name.trim();
    if (!name) {
      form_error = 'Give the project a name.';
      return;
    }
    busy = true;
    try {
      if (editing) await api.update_project(editing.id, { name, description: form.description.trim() });
      else await api.create_project(current_workspace(), name, form.description.trim());
      toast('success', editing ? `${name} saved` : `${name} created`);
      editing = undefined;
      await reload();
    } catch (err) {
      form_error = error_text(err);
    }
    busy = false;
  }

  async function remove() {
    if (!deleting) return;
    busy = true;
    try {
      await api.delete_project(deleting.id);
      toast('success', `${deleting.name} deleted`);
      deleting = null;
      await Promise.all([reload(), load_workspace()]);
    } catch (err) {
      delete_error = error_text(err);
    }
    busy = false;
  }
</script>

<PageHeader title="Projects" subtitle="Group related tasks and follow them together.">
  {#if can('run')}<Button variant="ice" size="sm" iconLeft="plus" onclick={() => open_form(null)}>New project</Button>{/if}
</PageHeader>

<div class="sun-page">
  {#if load_error}
    <div class="sun-empty" role="alert">{load_error}</div>
  {:else if projects && !projects.length}
    <div class="sun-empty">No projects yet. Create one, then pick it on a task.</div>
  {/if}
  {#each projects ?? [] as p (p.id)}
    {@const c = counts(p)}
    <article class="sun-row project">
      <span class="sun-tile sun-tile--ice" aria-hidden="true"><Icon name="folder-kanban" size={20} /></span>
      <div class="sun-row__body">
        <a class="sun-row__title project__link" href={href('projects', p.id)}>{p.name}</a>
        {#if p.description}<div class="sun-row__meta project__desc">{p.description}</div>{/if}
        <div class="sun-row__meta ml-mono">{c.open} open · {c.done} done</div>
      </div>
      {#if can('run')}<span class="project__menu">
        <Menu
          label="Project actions"
          name="Actions for {p.name}"
          icon="ellipsis"
          items={[
            { label: 'Edit', icon: 'pencil', onselect: () => open_form(p) },
            { label: 'Delete', icon: 'trash-2', danger: true, onselect: () => ((deleting = p), (delete_error = '')) },
          ]}
        />
      </span>{/if}
    </article>
  {/each}
</div>

<Dialog open={editing !== undefined} onclose={() => (editing = undefined)} title={editing ? `Edit ${editing.name}` : 'New project'}>
  <form id="project-form" class="project-form" onsubmit={save}>
    <Input label="Name" placeholder="e.g. Mobile launch" maxlength={64} bind:value={form.name} error={form_error || undefined} />
    <div class="ml-field">
      <label class="ml-field__label" for="project-description">Description <span class="dim">(optional)</span></label>
      <div class="ml-control ml-control--md project-form__area">
        <textarea id="project-description" class="ml-control__input" rows="3" maxlength="1000" bind:value={form.description}></textarea>
      </div>
    </div>
  </form>
  {#snippet footer()}
    <Button variant="secondary" onclick={() => (editing = undefined)}>Cancel</Button>
    <Button variant="ice" type="submit" form="project-form" loading={busy}>{editing ? 'Save' : 'Create project'}</Button>
  {/snippet}
</Dialog>

<Dialog open={!!deleting} onclose={() => (deleting = null)} title="Delete {deleting?.name ?? 'project'}?">
  {#if deleting}
    {@const n = counts(deleting).all}
    <p class="project__warn">
      {n ? `Its ${n} ${n === 1 ? 'task stays' : 'tasks stay'} on the workspace, outside any project.` : 'It has no tasks.'} This can't be undone.
    </p>
    {#if delete_error}<p class="project__error" role="alert">{delete_error}</p>{/if}
  {/if}
  {#snippet footer()}
    <Button variant="secondary" onclick={() => (deleting = null)}>Cancel</Button>
    <Button variant="danger" loading={busy} onclick={remove}>Delete project</Button>
  {/snippet}
</Dialog>

<style>
  .project {
    position: relative;
    align-items: center;
  }
  .project__link {
    text-decoration: none;
  }
  .project__link::after {
    content: '';
    position: absolute;
    inset: 0;
    border-radius: inherit;
  }
  .project:has(.project__link:focus-visible) {
    box-shadow: var(--ring-focus);
  }
  .project__link:focus-visible {
    outline: none;
  }
  .project:hover {
    border-color: var(--border-default);
  }
  .project__desc {
    overflow-wrap: anywhere;
  }
  .project__menu {
    position: relative;
    z-index: 1;
  }
  .project-form {
    display: flex;
    flex-direction: column;
    gap: var(--space-4);
  }
  .project-form__area {
    height: auto;
    padding-block: var(--space-2);
  }
  .project-form .dim {
    color: var(--fg-3);
    font-weight: 400;
  }
  .project__warn {
    margin: 0;
    color: var(--fg-2);
  }
  .project__error {
    margin: var(--space-3) 0 0;
    color: var(--danger-fg);
  }
</style>
