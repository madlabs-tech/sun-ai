<script lang="ts">
  import { Badge, Button, Dialog, Icon, Input, Tag } from '@hryer/madlabs-design-system/svelte';
  import IconButton from '../lib/ui/IconButton.svelte';
  import PageHeader from '../lib/ui/PageHeader.svelte';
  import Loaded from '../lib/ui/Loaded.svelte';
  import { api } from '../lib/api/client';
  import type { DeviceTool, Host, HostToken, KernelUse, SessionStats } from '../lib/api/types';
  import { copy_text, error_text, toast } from '../lib/app_state.svelte';
  import { bytes, clock, uptime } from '../lib/format';

  let load = $state(api.list_hosts());
  const kernel = api.kernel_use().catch(() => null);

  let enrol = $state<HostToken | null>(null);
  let enrol_open = $state(false);
  const command = $derived(enrol ? `sun host join ${enrol.token}` : '');

  async function add_host() {
    try {
      enrol = await api.create_host_token();
      enrol_open = true;
    } catch (e) {
      toast('danger', 'Couldn’t create a join token', error_text(e));
    }
  }

  const copy = () => copy_text(command, 'Command copied');

  const TOOL_NAMES: Record<string, string> = { ios_simulator: 'iOS Simulator', android_emulator: 'Android emulator', maestro: 'Maestro' };
  /** Device tools that aren't ready, with the fix the host reported. */
  const missing = (h: Host) =>
    Object.entries(h.capabilities.device_tools ?? {}).filter((e): e is [string, DeviceTool & { detail: string }] => !e[1].ready && !!e[1].detail);
  const usage = (rss: number | null, cpu: number | null) =>
    `${rss === null ? '?' : bytes(rss)} RAM · ${cpu === null ? '?' : cpu.toFixed(1)}% CPU`;
  const disk_total = (h: Host) => (h.disk ?? []).reduce((sum, d) => sum + d.bytes, 0);

  // Kill a session: a reason is required; the run records it.
  let killing = $state<{ host: string; session: SessionStats } | null>(null);
  let reason = $state('');
  let reason_error = $state('');
  let busy = $state(false);

  function ask_kill(host: string, session: SessionStats) {
    killing = { host, session };
    reason = '';
    reason_error = '';
  }

  async function kill() {
    if (!killing) return;
    if (!reason.trim()) {
      reason_error = 'Say why. The run’s timeline shows it.';
      return;
    }
    busy = true;
    try {
      await api.kill_session(killing.session.session_id, reason.trim());
      toast('success', 'Session killed', 'Its run is paused. Resume it from the task.');
      killing = null;
      load = api.list_hosts();
    } catch (e) {
      reason_error = error_text(e);
    } finally {
      busy = false;
    }
  }
</script>

<PageHeader title="Hosts" subtitle="Machines that run agent sessions. They dial in to the kernel.">
  <Button size="sm" variant="ice" iconLeft="plus" onclick={add_host}>Add host</Button>
</PageHeader>

<Dialog bind:open={enrol_open} onclose={() => (enrol_open = false)} title="Add a host">
  <Input label="Run this on the machine" mono readonly value={command} hint="Works once, until {enrol ? clock(enrol.expires_at) : ''}.">
    {#snippet trailing()}<IconButton icon="copy" label="Copy command" variant="ghost" size="sm" onclick={copy} />{/snippet}
  </Input>
  {#snippet footer()}<Button variant="secondary" onclick={() => (enrol_open = false)}>Done</Button>{/snippet}
</Dialog>

{#if killing}
  <Dialog open onclose={() => (killing = null)} title="Kill this session?">
    <p class="sun-kill__lede">
      The agent on <span class="ml-mono">{killing.host}</span> stops now and frees
      {bytes(killing.session.rss_bytes)} of RAM. Its run pauses; resuming starts a new session in the same worktree.
    </p>
    <Input label="Reason" bind:value={reason} error={reason_error || undefined} hint="Saved on the run’s timeline." maxlength={500} />
    {#snippet footer()}
      <Button variant="secondary" onclick={() => (killing = null)}>Cancel</Button>
      <Button variant="danger" loading={busy} onclick={kill}>Kill session</Button>
    {/snippet}
  </Dialog>
{/if}

<div class="sun-page">
  {#await kernel then k}
    {#if k}
      <article class="sun-row" aria-label="Kernel">
        <span class="sun-tile sun-tile--ice"><Icon name="cpu" size={18} /></span>
        <div class="sun-row__body">
          <div class="sun-row__title">Kernel</div>
          <div class="sun-row__meta">
            {usage(k.rss_bytes, k.cpu_percent)} · up {uptime(k.uptime_s)} · Sun itself, apart from agent sessions
          </div>
        </div>
      </article>
    {/if}
  {/await}
  <Loaded {load} empty="No hosts yet. Run sun host on a machine to connect it.">
    {#snippet row(h: Host)}
      <article class="sun-row" aria-label={h.name}>
        <span class="sun-tile"><Icon name="server" size={18} /></span>
        <div class="sun-row__body">
          <div class="sun-row__title"><span class="ml-mono">{h.name}</span></div>
          <div class="sun-row__meta">
            <span class="ml-mono">{h.capabilities.os}/{h.capabilities.arch}</span> · {h.sessions} {h.sessions === 1 ? 'session' : 'sessions'}
            {#if h.disk?.length}· {bytes(disk_total(h))} in {h.disk.length} task {h.disk.length === 1 ? 'folder' : 'folders'}{/if}
            · seen <span class="ml-mono">{clock(h.last_seen_at)}</span>
          </div>
          <div class="sun-chips" style="margin-top:8px">
            {#each [...h.capabilities.runtimes, ...h.capabilities.devices] as c (c)}<Tag size="sm">{c}</Tag>{/each}
          </div>
          {#each missing(h) as [tool, t] (tool)}
            <p class="sun-hosts__fix"><Icon name="triangle-alert" size={14} /><span><b>{TOOL_NAMES[tool] ?? tool}:</b> {t.detail}</span></p>
          {/each}
          {#if h.session_stats?.length}
            <ul class="sun-sessions" aria-label="Live sessions on {h.name}">
              {#each h.session_stats as s (s.session_id)}
                <li class="sun-session">
                  <div class="sun-session__body">
                    <span class="ml-mono sun-session__id">{s.step_key ?? 'session'} · {s.session_id.slice(-6)}</span>
                    <span class="sun-session__use">{usage(s.rss_bytes, s.cpu_percent)} · up {uptime(s.uptime_s)}</span>
                  </div>
                  <Button size="sm" variant="ghost" iconLeft="square" aria-label="Kill session {s.step_key ?? s.session_id} on {h.name}" onclick={() => ask_kill(h.name, s)}>Kill</Button>
                </li>
              {/each}
            </ul>
          {/if}
        </div>
        {#if h.status === 'online'}<Badge tone="success" dot>Online</Badge>{:else}<Badge dot>Offline</Badge>{/if}
      </article>
    {/snippet}
  </Loaded>
</div>

<style>
  .sun-kill__lede {
    margin: 0 0 var(--space-3);
    font-size: var(--size-sm);
    color: var(--fg-2);
  }
  .sun-hosts__fix {
    display: flex;
    gap: var(--space-2);
    align-items: flex-start;
    margin: var(--space-2) 0 0;
    font-size: var(--size-xs);
    color: var(--fg-2);
    overflow-wrap: anywhere;
  }
  .sun-sessions {
    list-style: none;
    margin: var(--space-3) 0 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
  }
  .sun-session {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    padding: var(--space-2) var(--space-3);
    border-radius: var(--radius-md);
    background: var(--surface-raised);
  }
  .sun-session__body {
    flex: 1;
    min-width: 0;
    display: flex;
    flex-wrap: wrap;
    column-gap: var(--space-3);
    font-size: var(--size-xs);
  }
  .sun-session__id {
    color: var(--fg-1);
    font-weight: 600;
  }
  .sun-session__use {
    color: var(--fg-2);
  }
</style>
