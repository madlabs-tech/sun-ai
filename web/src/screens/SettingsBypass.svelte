<!-- Settings → Security → Bypass permissions (deployment owners): who besides the owners may run “Anything”
     sessions — nobody, picked members, or everyone. Owners always may. sun.yaml `runtimes.allow_bypass` is the
     ceiling above it: off, the control shows disabled. Anyone not allowed still runs, as Edit files, and the
     task says so. -->
<script lang="ts">
  import { Button, Checkbox, Icon, Radio } from '@hryer/madlabs-design-system/svelte';
  import { auth, type BypassPolicy, type Member } from '../lib/api/auth';
  import { error_text, toast } from '../lib/app_state.svelte';

  type Mode = BypassPolicy['mode'];
  const OPTIONS: { value: Mode; label: string; description: string }[] = [
    { value: 'off', label: 'Owners only', description: 'Everyone else’s “Anything” sessions run as Edit files.' },
    { value: 'members', label: 'Owners and the members picked below', description: 'The rest run as Edit files.' },
    { value: 'everyone', label: 'Every member', description: 'Anyone who can start a run may run it as “Anything”.' },
  ];

  let saved = $state<BypassPolicy | null>(null);
  let mode = $state<Mode>('off');
  let picked = $state<string[]>([]);
  let people = $state<Member[]>([]);
  let load_error = $state('');
  let saving = $state(false);

  Promise.all([auth.get_bypass(), auth.list_members()]).then(([policy, list]) => {
    saved = policy;
    [mode, picked] = [policy.mode, [...policy.member_ids]];
    people = list.filter((m) => !m.is_owner && !m.deactivated_at);
  }, (e) => (load_error = error_text(e)));

  const changed = $derived(
    !!saved && (mode !== saved.mode || (mode === 'members' && [...picked].sort().join() !== [...saved.member_ids].sort().join())),
  );

  function toggle(id: string, on: boolean) {
    picked = on ? [...picked, id] : picked.filter((p) => p !== id);
  }

  async function save(e: SubmitEvent) {
    e.preventDefault();
    saving = true;
    try {
      saved = await auth.set_bypass({ mode, member_ids: mode === 'members' ? picked : [] });
      picked = [...saved.member_ids];
      toast('success', 'Bypass permissions saved');
    } catch (err) {
      toast('danger', 'Not saved', error_text(err));
    }
    saving = false;
  }
</script>

<form class="sun-row" aria-labelledby="bypass-head" onsubmit={save} data-testid="bypass-policy">
  <span class="sun-tile"><Icon name="shield-off" size={18} /></span>
  <div class="sun-row__body">
    <div class="sun-row__title" id="bypass-head">Bypass permissions</div>
    <p class="sun-row__meta bypass__copy">
      Who may run sessions in “Anything” mode, where the agent runs any command without asking. Owners always may.
      Two layers: sun.yaml <span class="ml-mono">runtimes.allow_bypass</span> must be on, then this decides who.
    </p>
    {#if load_error}
      <div class="sun-empty" role="alert">{load_error}</div>
    {:else if saved}
      {#if !saved.allow_bypass}
        <p class="sun-row__meta bypass__off" role="note" data-testid="bypass-ceiling">
          <Icon name="lock" size={14} />
          <span>Turn on <span class="ml-mono">runtimes.allow_bypass</span> in sun.yaml first (container or VM sessions only).</span>
        </p>
      {/if}
      <fieldset class="bypass__options" aria-labelledby="bypass-head" disabled={!saved.allow_bypass}>
        {#each OPTIONS as option (option.value)}
          <Radio bind:group={mode} name="bypass_mode" value={option.value} label={option.label} description={option.description} />
        {/each}
      </fieldset>
      {#if mode === 'members' && saved.allow_bypass}
        <fieldset class="bypass__members" aria-label="Members who may">
          {#each people as person (person.id)}
            <Checkbox
              label={person.username}
              description={person.email ?? undefined}
              checked={picked.includes(person.id)}
              onchange={(e: Event) => toggle(person.id, (e.currentTarget as HTMLInputElement).checked)}
            />
          {:else}
            <p class="sun-row__meta">No members besides the owners yet.</p>
          {/each}
        </fieldset>
      {/if}
      {#if saved.allow_bypass}
        <div class="bypass__actions"><Button type="submit" variant="ice" loading={saving} disabled={!changed}>Save</Button></div>
      {/if}
    {:else}
      <div class="sun-empty" role="status">Loading…</div>
    {/if}
  </div>
</form>

<style>
  .bypass__copy {
    margin-bottom: 0;
    line-height: 1.5;
    max-width: 60ch;
  }
  .bypass__off {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    margin: var(--space-3) 0 0;
  }
  .bypass__options,
  .bypass__members {
    border: 0;
    margin: var(--space-3) 0 0;
    padding: 0;
    min-width: 0;
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    max-width: 560px;
  }
  .bypass__members {
    padding-left: var(--space-6);
  }
  .bypass__actions {
    margin-top: var(--space-3);
  }
</style>
