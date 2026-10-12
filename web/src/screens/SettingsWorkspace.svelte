<!-- Settings → <workspace> → General: the workspace's profile, label left and control right, each field
     saved on blur (or on change for the logo and the default workflow) with Saving… / Saved beside it.
     `edit_workspace` edits; everyone else reads. The prefix changes through a dialog (it renumbers every
     task). Danger zone: delete (`delete_workspace`; never Main; the kernel refuses one with workflows). -->
<script lang="ts">
  import { Button, Dialog, Input, Select } from '@hryer/madlabs-design-system/svelte';
  import IconButton from '../lib/ui/IconButton.svelte';
  import TextArea from '../lib/ui/TextArea.svelte';
  import WorkspaceLogo from '../lib/ui/WorkspaceLogo.svelte';
  import { api } from '../lib/api/client';
  import { MAIN_WORKSPACE, type Workflow, type Workspace, type WorkspaceProfileChange } from '../lib/api/types';
  import { act, can, copy_text, current_workspace, error_text, keep_workspace, load_workspace, toast, workspace_of } from '../lib/app_state.svelte';
  import { show_workspace } from '../lib/router.svelte';

  const LOGO_TYPES = ['image/png', 'image/jpeg', 'image/webp'];
  const MAX_LOGO = 1024 * 1024;
  const PREFIX = /^[A-Z0-9]{1,10}$/;

  // Settings remounts this page per workspace, so the fields start from the one open now.
  const initial = workspace_of(current_workspace()) as Workspace;
  const id = initial.id;
  const workspace = $derived(workspace_of(id) ?? initial);
  const editable = $derived(can('edit_workspace'));

  let form = $state({ name: initial.name, description: initial.description, context: initial.context });
  let default_workflow = $state(initial.default_workflow_id ?? '');
  type Field = 'logo' | 'name' | 'description' | 'context' | 'default_workflow_id';
  let status = $state<Partial<Record<Field, 'saving' | 'saved'>>>({});
  let errors = $state<Partial<Record<Field, string>>>({});

  let workflows = $state<Workflow[]>([]);
  api.list_workflows(id).then((w) => (workflows = w), () => {});
  const workflow_options = $derived([{ value: '', label: 'None' }, ...workflows.map((w) => ({ value: w.id, label: w.name }))]);

  /** Saves one field when it changed; the kernel's answer replaces the workspace in the list. */
  async function save(field: Field, change: WorkspaceProfileChange | (() => Promise<Workspace>)) {
    status[field] = 'saving';
    errors[field] = undefined;
    try {
      keep_workspace(typeof change === 'function' ? await change() : await api.update_workspace(id, change));
      status[field] = 'saved';
    } catch (e) {
      status[field] = undefined;
      errors[field] = error_text(e);
    }
  }

  function blur(field: 'name' | 'description' | 'context') {
    const value = field === 'name' ? form.name.trim() : form[field];
    if (!editable || value === workspace[field]) return;
    if (field === 'name' && !value) {
      errors.name = 'Give the workspace a name.';
      return;
    }
    save(field, { [field]: value });
  }

  let file_input = $state<HTMLInputElement>();
  function pick_logo(e: Event) {
    const input = e.currentTarget as HTMLInputElement;
    const file = input.files?.[0];
    input.value = '';
    if (!file) return;
    if (!LOGO_TYPES.includes(file.type)) errors.logo = 'Use a PNG, JPEG or WebP image.';
    else if (file.size > MAX_LOGO) errors.logo = 'That image is over 1 MB. Use a smaller one.';
    else save('logo', () => api.set_workspace_logo(id, file));
  }

  // Change prefix dialog.
  let prefix_open = $state(false);
  let prefix = $state('');
  let prefix_error = $state('');
  let prefix_busy = $state(false);
  const next_prefix = $derived(prefix.trim().toUpperCase());
  const sample = $derived(Math.max(workspace.issue_counter, 1));

  function open_prefix() {
    [prefix, prefix_error, prefix_open] = ['', '', true];
  }

  async function change_prefix(e: Event) {
    e.preventDefault();
    if (!PREFIX.test(next_prefix)) {
      prefix_error = 'Letters and numbers only, up to 10 characters.';
      return;
    }
    if (next_prefix === workspace.issue_prefix) {
      prefix_error = `The prefix is already ${next_prefix}.`;
      return;
    }
    prefix_busy = true;
    try {
      keep_workspace(await api.set_issue_prefix(id, next_prefix));
      prefix_open = false;
      toast('success', `Tasks are now ${next_prefix}-…`);
      load_workspace(); // the open workspace's task keys changed
    } catch (err) {
      prefix_error = error_text(err);
    }
    prefix_busy = false;
  }

  // Danger zone.
  let deleting = $state(false);
  let delete_busy = $state(false);
  async function remove() {
    delete_busy = true;
    if (await act(() => api.delete_workspace(id), `${workspace.name} deleted`)) show_workspace(workspace_of(MAIN_WORKSPACE)?.slug ?? 'main');
    delete_busy = false;
    deleting = false;
  }
</script>

{#snippet saved(field: Field)}
  <span class="field__status" role="status">{status[field] === 'saving' ? 'Saving…' : status[field] === 'saved' ? 'Saved' : ''}</span>
{/snippet}

<section class="sun-row ws" aria-labelledby="ws-details">
  <h3 class="ws__title" id="ws-details">Workspace details</h3>
  {#if !editable}<p class="ws__note">You can see these settings. Changing them needs “Edit workspaces”.</p>{/if}

  <div class="field">
    <div class="field__label"><span id="ws-logo-label">Workspace logo</span>{@render saved('logo')}</div>
    <div class="field__control">
      <div class="logo">
        {#if editable}
          <button type="button" class="logo__pick" aria-labelledby="ws-logo-label" aria-describedby="ws-logo-hint" onclick={() => file_input?.click()}>
            <WorkspaceLogo {workspace} size={64} />
          </button>
          <input bind:this={file_input} type="file" accept={LOGO_TYPES.join(',')} hidden onchange={pick_logo} />
        {:else}
          <WorkspaceLogo {workspace} size={64} />
        {/if}
        <div class="logo__side">
          {#if editable}
            <div class="logo__actions">
              <Button variant="secondary" size="sm" iconLeft="upload" onclick={() => file_input?.click()}>{workspace.logo_url ? 'Replace' : 'Upload'}</Button>
              {#if workspace.logo_url}<Button variant="ghost" size="sm" onclick={() => save('logo', () => api.remove_workspace_logo(id))}>Remove</Button>{/if}
            </div>
          {/if}
          <span id="ws-logo-hint" class="ml-field__hint" class:ml-field__hint--error={errors.logo} role={errors.logo ? 'alert' : undefined}>
            {errors.logo ?? 'PNG, JPEG or WebP, up to 1 MB. Square works best.'}
          </span>
        </div>
      </div>
    </div>
  </div>

  <div class="field">
    <div class="field__label"><label for="ws-name">Name</label>{@render saved('name')}</div>
    <div class="field__control">
      <Input id="ws-name" maxlength={64} readonly={!editable} bind:value={form.name} onblur={() => blur('name')} error={errors.name} />
    </div>
  </div>

  <div class="field">
    <div class="field__label"><label for="ws-description">Description</label>{@render saved('description')}</div>
    <div class="field__control">
      <TextArea id="ws-description" rows={3} maxlength={1000} readonly={!editable} placeholder="What does this workspace focus on?" bind:value={form.description} onblur={() => blur('description')} error={errors.description} />
    </div>
  </div>

  <div class="field">
    <div class="field__label"><label for="ws-context">Agent context</label>{@render saved('context')}</div>
    <div class="field__control">
      <TextArea id="ws-context" rows={4} maxlength={8000} readonly={!editable} hint="Given to every agent in this workspace on every run." bind:value={form.context} onblur={() => blur('context')} error={errors.context} />
    </div>
  </div>

  <div class="field">
    <div class="field__label" id="ws-slug">Slug</div>
    <div class="field__control">
      <div class="value" role="group" aria-labelledby="ws-slug">
        <code class="ml-mono value__code">{workspace.slug}</code>
        <IconButton icon="copy" label="Copy slug" variant="ghost" size="sm" onclick={() => copy_text(workspace.slug, 'Slug copied')} />
      </div>
      <span class="ml-field__hint">Part of every workspace URL. It can't be changed.</span>
    </div>
  </div>

  <div class="field">
    <div class="field__label" id="ws-prefix">Task prefix</div>
    <div class="field__control">
      <div class="value" role="group" aria-labelledby="ws-prefix">
        <code class="ml-mono value__code">{workspace.issue_prefix}</code>
        {#if editable}<Button variant="secondary" size="sm" onclick={open_prefix}>Change prefix…</Button>{/if}
      </div>
      <span class="ml-field__hint">Used in task numbers like {workspace.issue_prefix}-123.</span>
    </div>
  </div>

  <div class="field">
    <div class="field__label"><label for="ws-workflow">Default workflow</label>{@render saved('default_workflow_id')}</div>
    <div class="field__control">
      {#if editable}
        <Select
          id="ws-workflow"
          options={workflow_options}
          bind:value={default_workflow}
          onchange={() => save('default_workflow_id', { default_workflow_id: default_workflow || null })}
          error={errors.default_workflow_id}
          hint="New tasks use it unless someone picks another."
        />
      {:else}
        <!-- Read-only: plain text, since a disabled select is too faint to read. -->
        <output id="ws-workflow" class="value"><code class="ml-mono value__code">{workflows.find((w) => w.id === default_workflow)?.name ?? 'None'}</code></output>
      {/if}
    </div>
  </div>
</section>

{#if can('delete_workspace')}
  <section class="sun-row ws ws--danger" aria-labelledby="ws-danger">
    <h3 class="ws__title" id="ws-danger">Danger zone</h3>
    <div class="danger">
      <div class="danger__text">
        <div class="danger__name">Delete workspace</div>
        <p class="ws__note">
          {#if id === MAIN_WORKSPACE}Main is the default workspace and can't be deleted.{:else}Only a workspace with no workflows can be deleted. This can't be undone.{/if}
        </p>
      </div>
      <Button variant="danger" size="sm" disabled={id === MAIN_WORKSPACE} onclick={() => (deleting = true)}>Delete workspace</Button>
    </div>
  </section>
{/if}

<Dialog open={prefix_open} onclose={() => (prefix_open = false)} title="Change the task prefix">
  <form id="prefix-form" class="prefix" onsubmit={change_prefix} novalidate>
    <Input label="New prefix" mono maxlength={10} autocomplete="off" spellcheck={false} bind:value={prefix} error={prefix_error || undefined} hint="Letters and numbers only, up to 10 characters." />
    <p class="prefix__preview">
      <span class="sun-sr-only">Task keys change from</span>
      <s class="ml-mono">{workspace.issue_prefix}-{sample}</s>
      <span aria-hidden="true">→</span>
      <span class="sun-sr-only">to</span>
      <span class="ml-mono prefix__new">{next_prefix || '…'}-{sample}</span>
    </p>
    <p class="prefix__warn">Every task is renumbered, including done and cancelled ones.</p>
  </form>
  {#snippet footer()}
    <Button variant="secondary" onclick={() => (prefix_open = false)}>Cancel</Button>
    <Button variant="ice" type="submit" form="prefix-form" loading={prefix_busy} disabled={!next_prefix}>Change to {next_prefix || '…'}</Button>
  {/snippet}
</Dialog>

<Dialog open={deleting} onclose={() => (deleting = false)} title="Delete {workspace.name}?">
  <p class="ws__note">Only a workspace with no workflows can be deleted. This can't be undone.</p>
  {#snippet footer()}
    <Button variant="secondary" onclick={() => (deleting = false)}>Cancel</Button>
    <Button variant="danger" loading={delete_busy} onclick={remove}>Delete workspace</Button>
  {/snippet}
</Dialog>

<style>
  .ws {
    flex-direction: column;
    align-items: stretch;
    flex-wrap: nowrap;
    gap: var(--space-4);
  }
  .ws--danger {
    border-color: var(--danger);
  }
  .ws__title {
    margin: 0;
    font-size: 15px;
    font-weight: 600;
    color: var(--fg-1);
  }
  .ws__note {
    margin: 0;
    color: var(--fg-2);
    font-size: var(--size-sm);
    line-height: 1.5;
  }
  .field {
    display: grid;
    gap: var(--space-2);
  }
  .field__label {
    display: flex;
    align-items: baseline;
    flex-wrap: wrap;
    gap: var(--space-2);
    font-size: 14px;
    font-weight: 600;
    color: var(--fg-1);
  }
  .field__status {
    font-size: var(--size-xs);
    font-weight: 400;
    color: var(--fg-2);
  }
  .field__control {
    display: grid;
    gap: 6px;
    min-width: 0;
  }
  .logo {
    display: flex;
    align-items: center;
    gap: var(--space-3);
  }
  .logo__pick {
    padding: 0;
    border: 0;
    border-radius: var(--radius-sm);
    background: none;
    cursor: pointer;
  }
  .logo__pick:focus-visible {
    outline: none;
    box-shadow: var(--ring-focus);
  }
  .logo__side {
    display: grid;
    gap: var(--space-1);
    min-width: 0;
  }
  .logo__actions {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-2);
  }
  .value {
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: var(--space-2);
    min-height: var(--control-md);
  }
  .value__code {
    padding: 2px 8px;
    border-radius: var(--radius-sm);
    background: var(--surface-sunken);
    color: var(--fg-1);
    overflow-wrap: anywhere;
  }
  .danger {
    display: flex;
    align-items: center;
    justify-content: space-between;
    flex-wrap: wrap;
    gap: var(--space-3);
  }
  .danger__text {
    flex: 1 1 240px;
  }
  .danger__name {
    font-size: 14px;
    font-weight: 600;
    color: var(--fg-1);
  }
  .prefix {
    display: grid;
    gap: var(--space-3);
  }
  .prefix__preview {
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: var(--space-2);
    margin: 0;
    font-size: 15px;
  }
  .prefix__preview s {
    color: var(--fg-3);
  }
  .prefix__new {
    color: var(--fg-1);
    font-weight: 600;
  }
  .prefix__warn {
    margin: 0;
    color: var(--warning-fg);
    font-size: var(--size-sm);
    line-height: 1.5;
  }

  /* Label left, control right from tablet width up. */
  @media (min-width: 768px) {
    .field {
      grid-template-columns: 180px minmax(0, 1fr);
      align-items: start;
      gap: var(--space-4);
    }
    .field__label {
      flex-direction: column;
      gap: 2px;
      padding-top: 10px;
    }
  }
</style>
