<!-- Settings → <workspace> → Access (`edit_workspace`): which groups see this workspace. Without
     `manage_groups`, only your own groups are offered; the rest are kept as they are. -->
<script lang="ts">
  import { Button } from '@hryer/madlabs-design-system/svelte';
  import type { Group } from '../lib/api/auth';
  import { api } from '../lib/api/client';
  import { MAIN_WORKSPACE, type Workspace } from '../lib/api/types';
  import { current_workspace, error_text, keep_workspace, load_workspace, toast, workspace_of } from '../lib/app_state.svelte';
  import { show_workspace } from '../lib/router.svelte';
  import WorkspaceAccess, { access_choices } from '../lib/ui/WorkspaceAccess.svelte';

  const workspace = workspace_of(current_workspace()) as Workspace;
  let choices = $state<Group[]>([]);
  let picked = $state<string[]>([]);
  /** Groups on the workspace this person can't change: kept. */
  let others = $state<string[]>([]);
  let busy = $state(false);
  let error = $state('');

  access_choices().then((all) => {
    choices = all;
    const ids = all.map((g) => g.id);
    picked = workspace.group_ids.filter((g) => ids.includes(g));
    others = workspace.group_ids.filter((g) => !ids.includes(g));
  });

  async function save(e: Event) {
    e.preventDefault();
    busy = true;
    error = '';
    try {
      const saved = await api.set_workspace_groups(workspace.id, [...others, ...picked]);
      keep_workspace(saved);
      toast('success', 'Access saved');
      // Taking your own access away hides the workspace from you: go home.
      await load_workspace();
      if (!workspace_of(workspace.id)) show_workspace(workspace_of(MAIN_WORKSPACE)?.slug ?? 'main');
    } catch (err) {
      error = error_text(err);
    }
    busy = false;
  }
</script>

<form class="sun-row access-page" aria-label="Access" onsubmit={save}>
  <WorkspaceAccess {choices} bind:picked {others} />
  {#if error}<p class="ml-field__hint ml-field__hint--error" role="alert">{error}</p>{/if}
  <div><Button type="submit" variant="ice" loading={busy}>Save access</Button></div>
</form>

<style>
  .access-page {
    flex-direction: column;
    align-items: stretch;
    flex-wrap: nowrap;
    gap: var(--space-4);
  }
  .access-page p {
    margin: 0;
  }
</style>
