<!-- Settings → Security → Terminal (deployment owners): who may open a terminal in the repo editor besides holding
     its `terminal` grant: anyone granted it, or only members of the groups picked here. Deployment owners always
     may. Applies from each person's next request. -->
<script lang="ts">
  import { Button, Checkbox, Icon, Radio } from '@hryer/madlabs-design-system/svelte';
  import { auth } from '../lib/api/auth';
  import type { MemberGroup, TerminalPolicy } from '../lib/api/types';
  import { error_text, toast } from '../lib/app_state.svelte';
  import { href } from '../lib/router.svelte';

  type Mode = TerminalPolicy['mode'];
  const OPTIONS: { value: Mode; label: string; description: string }[] = [
    { value: 'granted', label: 'Anyone granted terminal access', description: 'The Terminal grant in Editor access is enough.' },
    { value: 'groups', label: 'Only members of these groups', description: 'They also need the Terminal grant.' },
  ];

  let saved = $state<TerminalPolicy | null>(null);
  let mode = $state<Mode>('granted');
  let picked = $state<string[]>([]);
  let groups = $state<MemberGroup[]>([]);
  let load_error = $state('');
  let saving = $state(false);

  Promise.all([auth.get_terminal(), auth.list_groups()]).then(([policy, list]) => {
    saved = policy;
    [mode, picked, groups] = [policy.mode, [...policy.groups], list];
  }, (e) => (load_error = error_text(e)));

  const same = (a: string[], b: string[]) => [...a].sort().join() === [...b].sort().join();
  const changed = $derived(!!saved && (mode !== saved.mode || (mode === 'groups' && !same(picked, saved.groups))));

  function toggle(id: string, on: boolean) {
    picked = on ? [...picked, id] : picked.filter((p) => p !== id);
  }

  async function save(e: SubmitEvent) {
    e.preventDefault();
    saving = true;
    try {
      saved = await auth.set_terminal({ mode, groups: mode === 'groups' ? picked : [] });
      picked = [...saved.groups];
      toast('success', 'Terminal access saved', 'It applies from each person’s next request.');
    } catch (err) {
      toast('danger', 'Not saved', error_text(err));
    }
    saving = false;
  }
</script>

<form class="sun-row" aria-labelledby="terminal-head" onsubmit={save} data-testid="terminal-policy">
  <span class="sun-tile"><Icon name="terminal" size={18} /></span>
  <div class="sun-row__body">
    <div class="sun-row__title" id="terminal-head">Who may open a terminal</div>
    <p class="sun-row__meta terminal__copy">
      A terminal is a shell on the host. Editor access grants it per workspace; this narrows it for the whole deployment.
    </p>
    {#if load_error}
      <div class="sun-empty" role="alert">{load_error}</div>
    {:else if saved}
      <fieldset class="terminal__options" aria-labelledby="terminal-head">
        {#each OPTIONS as option (option.value)}
          <Radio bind:group={mode} name="terminal_mode" value={option.value} label={option.label} description={option.description} />
        {/each}
      </fieldset>
      {#if mode === 'groups'}
        <fieldset class="terminal__groups" aria-label="Groups whose members may">
          {#each groups as group (group.id)}
            <Checkbox
              label={group.name}
              description={group.description || undefined}
              checked={picked.includes(group.id)}
              onchange={(e: Event) => toggle(group.id, (e.currentTarget as HTMLInputElement).checked)}
            />
          {:else}
            <p class="sun-row__meta">No groups yet. <a href={href('settings', 'groups')}>Create one</a> first.</p>
          {/each}
        </fieldset>
      {/if}
      <p class="sun-row__meta terminal__owner" role="note"><Icon name="shield-check" size={14} />You (deployment owner) always have access.</p>
      <div><Button type="submit" variant="ice" loading={saving} disabled={!changed}>Save</Button></div>
    {:else}
      <div class="sun-empty" role="status">Loading…</div>
    {/if}
  </div>
</form>

<style>
  .terminal__copy {
    margin-bottom: 0;
    line-height: 1.5;
    max-width: 60ch;
  }
  .terminal__options,
  .terminal__groups {
    border: 0;
    margin: var(--space-3) 0 0;
    padding: 0;
    min-width: 0;
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    max-width: 560px;
  }
  .terminal__groups {
    padding-left: var(--space-6);
  }
  .terminal__owner {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    margin: var(--space-3) 0;
  }
</style>
