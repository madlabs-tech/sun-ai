<!-- Settings → Audit log: who signed in, changed access, secrets or settings, newest first. Needs manage_settings (the shell gates the page). -->
<script lang="ts">
  import { Icon } from '@hryer/madlabs-design-system/svelte';
  import Loaded from '../lib/ui/Loaded.svelte';
  import { auth, type AuditAction, type AuditEntry } from '../lib/api/auth';
  import { clock } from '../lib/format';

  const ACTION: Record<AuditAction, [string, string]> = {
    login: ['log-in', 'Signed in'],
    role_change: ['user-cog', 'Changed access of'],
    group_change: ['users', 'Changed group'],
    member_change: ['user-plus', 'Changed member'],
    secret_read: ['key-round', 'Read secret'],
    secret_change: ['key-round', 'Changed secret'],
    settings_change: ['settings', 'Changed setting'],
    mod_install: ['puzzle', 'Installed mod'],
    approval: ['user-check', 'Decided approval'],
    policy_change: ['shield', 'Set permission mode of'],
    trigger_failed: ['triangle-alert', 'Trigger failed:'],
    workspace_change: ['kanban', 'Changed workspace'],
    agent_change: ['bot', 'Changed agent'],
    editor_grant: ['shield-check', 'Changed editor access of'],
    editor_file: ['file-text', 'Changed file in'],
    terminal: ['terminal', 'Used a terminal on'],
  };

  const audit = auth.list_audit();
</script>

<Loaded load={audit} empty="Nothing recorded yet.">
  {#snippet row(a: AuditEntry)}
    {@const actor = a.actor.replace(/^user:/, '')}
    <article class="sun-row" style="align-items:center">
      <span class="sun-tile"><Icon name={ACTION[a.action][0]} size={18} /></span>
      <div class="sun-row__body">
        <div class="sun-row__title">
          <span class="ml-mono">{actor}</span>
          {ACTION[a.action][1].toLowerCase()}
          {#if a.target !== actor}<span class="ml-mono">{a.target}</span>{/if}
        </div>
        {#if a.detail}<div class="sun-row__meta">{a.detail}</div>{/if}
      </div>
      <span class="sun-row__meta ml-mono" style="margin:0">{clock(a.at)}</span>
    </article>
  {/snippet}
</Loaded>
