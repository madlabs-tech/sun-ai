<!-- Who sees a workspace: a checkbox per group this person may give or take access for (every group with
     `manage_groups`, else their own). Groups they can't change (`others`) are only counted. Used by the
     create-workspace dialog and Settings → <workspace> → Access. -->
<script lang="ts">
  import { Checkbox } from '@hryer/madlabs-design-system/svelte';
  import { auth, type Group } from '../api/auth';
  import { app, can } from '../app_state.svelte';
  import { toggle } from '../format';

  let {
    choices,
    picked = $bindable(),
    others = [],
    disabled = false,
  }: { choices: Group[]; picked: string[]; others?: string[]; disabled?: boolean } = $props();
</script>

<script module lang="ts">
  /** The groups this person may tick: every group with `manage_groups`, else their own. */
  export const access_choices = async (): Promise<Group[]> => (can('manage_groups') ? await auth.list_groups().catch(() => app.groups) : app.groups);
</script>

<fieldset class="access" {disabled}>
  <legend>Access</legend>
  <p class="access__hint">Who sees this workspace. Admins (with “Access all workspaces”) always do; with no group ticked, only they do.</p>
  {#each choices as g (g.id)}
    <Checkbox
      label={g.name}
      description="role:{g.key}"
      checked={picked.includes(g.id)}
      onchange={(e: Event) => (picked = toggle(picked, g.id, (e.currentTarget as HTMLInputElement).checked))}
    />
  {:else}
    <p class="access__hint">You aren’t in any group, so only admins will see it.</p>
  {/each}
  {#if others.length}
    <p class="access__hint">
      {others.length} other {others.length === 1 ? 'group' : 'groups'} also {others.length === 1 ? 'sees' : 'see'} it. Only someone who manages access can change that.
    </p>
  {/if}
</fieldset>

<style>
  .access {
    margin: 0;
    padding: 0;
    border: 0;
    min-width: 0;
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
  }
  .access legend {
    padding: 0;
    margin-bottom: var(--space-1);
    font-weight: 600;
    color: var(--fg-1);
  }
  .access__hint {
    margin: 0;
    color: var(--fg-2);
    font-size: var(--size-sm);
    line-height: 1.5;
  }
</style>
