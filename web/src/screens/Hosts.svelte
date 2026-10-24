<!-- Hosts: the machines that run the open workspace's agent sessions. Each names the workspaces it serves and who
     enrolled it. "Add host" (manage_runtimes) mints a join token that attaches the new host here; a deployment owner,
     whoever enrolled a host, or a member of a group it is shared with may attach it to more workspaces (the kernel
     says which: GET /hosts/attachable); anyone with manage_runtimes may detach one. -->
<script lang="ts">
  import { Badge, Button, Dialog, Icon, Input, Tag } from '@hryer/madlabs-design-system/svelte';
  import IconButton from '../lib/ui/IconButton.svelte';
  import PageHeader from '../lib/ui/PageHeader.svelte';
  import Loaded from '../lib/ui/Loaded.svelte';
  import { api } from '../lib/api/client';
  import { auth } from '../lib/api/auth';
  import type { AttachableHost, DeviceTool, Host, HostToken, KernelUse, SessionStats } from '../lib/api/types';
  import { app, can, copy_text, current_workspace, error_text, is_owner, toast, workspace_of } from '../lib/app_state.svelte';
  import { bytes, clock, uptime } from '../lib/format';

  const workspace = current_workspace();
  const here = workspace_of(workspace)?.name ?? 'this workspace';
  /** The kernel's own use and killing sessions are the deployment's: its owners only. */
  const owner = is_owner();
  const manage = can('manage_runtimes');
  /** The signed-in person's member id (the kernel's token holder has none). */
  const me = app.memberships[0]?.member_id ?? null;

  let load = $state(api.list_hosts(workspace));
  const kernel: Promise<KernelUse | null> = owner ? api.kernel_use().catch(() => null) : Promise.resolve(null);

  /** Usernames by member id, for "enrolled by": every account for an owner, this workspace's people otherwise. */
  let names = $state<Record<string, string>>({});
  (owner
    ? auth.list_members().then((list) => list.map((m) => [m.id, m.username]))
    : auth.workspace_members(workspace).then((list) => list.map((m) => [m.member_id, m.username]))
  ).then(
    (pairs) => (names = Object.fromEntries(pairs)),
    () => {},
  );
  const enrolled = (h: Pick<Host, 'enrolled_by'>) =>
    h.enrolled_by === null
      ? 'no enroller on record'
      : h.enrolled_by === me
        ? 'enrolled by you'
        : `enrolled by ${names[h.enrolled_by] ?? 'someone outside this workspace'}`;
  const serves = (h: Pick<Host, 'workspace_ids'>) => h.workspace_ids.map((id) => workspace_of(id)?.name ?? '').filter(Boolean).join(', ') || 'no workspace';

  /** Hosts not attached here that the caller may attach, as the kernel decides: any for an owner, otherwise the ones
   *  they enrolled and the ones shared with a group they are in. */
  let others = $state<AttachableHost[]>([]);
  async function load_others() {
    if (!manage) return;
    const list = await api.attachable_hosts(workspace).catch(() => [] as AttachableHost[]);
    others = list.filter((h) => !h.attached);
  }
  load_others();

  function reload() {
    load = api.list_hosts(workspace);
    load_others();
  }

  let acting = $state('');
  async function attach(h: Pick<Host, 'id' | 'name'>) {
    acting = h.id;
    try {
      await api.attach_host(workspace, h.id);
      toast('success', `${h.name} attached`, `${here}’s runs may go to it now.`);
      reload();
    } catch (e) {
      toast('danger', `Couldn’t attach ${h.name}`, error_text(e));
    }
    acting = '';
  }

  let detaching = $state<Host | null>(null);
  async function detach() {
    if (!detaching) return;
    const h = detaching;
    acting = h.id;
    try {
      await api.detach_host(workspace, h.id);
      toast('success', `${h.name} detached`, `${here}’s new runs no longer go to it.`);
      detaching = null;
      reload();
    } catch (e) {
      toast('danger', `Couldn’t detach ${h.name}`, error_text(e));
    }
    acting = '';
  }

  // Linux installs into /usr/local/bin, so it needs root; macOS installs into ~/.local/bin and must not.
  const INSTALL_LINUX = 'curl -fsSL https://get-sun.madlabs.tech | sudo sh -s -- --no-service';
  const INSTALL_MACOS = 'curl -fsSL https://get-sun.madlabs.tech | sh -s -- --no-service';
  let enrol = $state<HostToken | null>(null);
  let enrol_open = $state(false);
  let host_name = $state('');
  /** Single-quoted for the shell unless it is plainly safe. */
  const shell_word = (word: string) => (/^[\w.@-]+$/.test(word) ? word : `'${word.replaceAll("'", `'\\''`)}'`);
  const command = $derived.by(() => {
    if (!enrol) return '';
    const name = host_name.trim() ? ` --name ${shell_word(host_name.trim())}` : '';
    return `sun host join --kernel ${enrol.kernel_url}${name} ${enrol.token}`;
  });
  const valid_for = (h: HostToken) => Math.max(0, Math.round((new Date(h.expires_at).getTime() - Date.now()) / 60_000));

  async function add_host() {
    try {
      enrol = await api.create_host_token(workspace);
      host_name = '';
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
    `${rss === null ? '?' : bytes(rss)} RAM · ${cpu === null ? '?' : cpu.toFixed(1)}% avg CPU`;
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
      reload();
    } catch (e) {
      reason_error = error_text(e);
    } finally {
      busy = false;
    }
  }
</script>

<PageHeader title="Hosts" subtitle="Machines that run {here}’s agent sessions. They dial in to the kernel.">
  {#if manage}<Button size="sm" variant="ice" iconLeft="plus" onclick={add_host}>Add host</Button>{/if}
</PageHeader>

<Dialog bind:open={enrol_open} onclose={() => (enrol_open = false)} title="Add a host">
  <div class="sun-hosts__enrol">
    <Input label="Host name" bind:value={host_name} placeholder="The machine’s hostname" hint="Shown in the host list. Leave it empty for the hostname." />
    <Input label="Run this on the machine" mono readonly value={command} hint={enrol ? `Works once, for ${valid_for(enrol)} min (until ${clock(enrol.expires_at)}). The host serves ${here}.` : ''}>
      {#snippet trailing()}<IconButton icon="copy" label="Copy command" variant="ghost" size="sm" onclick={copy} />{/snippet}
    </Input>
    <Input label="No sun on the machine yet? Install it on Linux" mono readonly value={INSTALL_LINUX}>
      {#snippet trailing()}<IconButton icon="copy" label="Copy Linux install command" variant="ghost" size="sm" onclick={() => copy_text(INSTALL_LINUX, 'Command copied')} />{/snippet}
    </Input>
    <Input label="Or on macOS" mono readonly value={INSTALL_MACOS}>
      {#snippet trailing()}<IconButton icon="copy" label="Copy macOS install command" variant="ghost" size="sm" onclick={() => copy_text(INSTALL_MACOS, 'Command copied')} />{/snippet}
    </Input>
  </div>
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

{#if detaching}
  <Dialog open onclose={() => (detaching = null)} title="Detach {detaching.name}?">
    <p class="sun-kill__lede">
      {here}’s new runs stop going to it; sessions already on it finish. Only a deployment owner, whoever enrolled it, or a
      member of a group it’s shared with can attach it again.
    </p>
    {#snippet footer()}
      <Button variant="secondary" onclick={() => (detaching = null)}>Cancel</Button>
      <Button variant="danger" loading={acting === detaching?.id} onclick={detach}>Detach</Button>
    {/snippet}
  </Dialog>
{/if}

{#snippet status(h: Pick<Host, 'status'>)}
  {#if h.status === 'online'}<Badge tone="success" dot>Online</Badge>{:else}<Badge dot>Offline</Badge>{/if}
{/snippet}

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
  <Loaded {load} empty={manage ? 'No hosts serve this workspace yet. Add one, or attach one below.' : 'No hosts serve this workspace yet. Ask a workspace admin to add one.'}>
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
          <div class="sun-row__meta">Serves {serves(h)} · {enrolled(h)}</div>
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
                  {#if owner}<Button size="sm" variant="ghost" iconLeft="square" aria-label="Kill session {s.step_key ?? s.session_id} on {h.name}" onclick={() => ask_kill(h.name, s)}>Kill</Button>{/if}
                </li>
              {/each}
            </ul>
          {/if}
        </div>
        <div class="sun-row__aside">
          {@render status(h)}
          {#if manage}<Button size="sm" variant="secondary" aria-label="Detach {h.name}" onclick={() => (detaching = h)}>Detach</Button>{/if}
        </div>
      </article>
    {/snippet}
  </Loaded>

  {#if others.length}
    <section class="sun-hosts__others" aria-labelledby="other-hosts">
      <h2 id="other-hosts">Hosts you can attach</h2>
      <p class="sun-row__meta">{owner ? 'Every other host of this deployment.' : 'Hosts you enrolled, and hosts shared with a group you’re in.'}</p>
      {#each others as h (h.id)}
        <article class="sun-row" aria-label={h.name}>
          <span class="sun-tile"><Icon name="server" size={18} /></span>
          <div class="sun-row__body">
            <div class="sun-row__title"><span class="ml-mono">{h.name}</span></div>
            <div class="sun-row__meta"><span class="ml-mono">{h.capabilities.os}/{h.capabilities.arch}</span> · serves {serves(h)} · {h.via === 'share' ? 'shared with a group you’re in' : enrolled(h)}</div>
          </div>
          <div class="sun-row__aside">
            {@render status(h)}
            <Button size="sm" variant="ice" iconLeft="link" loading={acting === h.id} onclick={() => attach(h)}>Attach to this workspace</Button>
          </div>
        </article>
      {/each}
    </section>
  {/if}
</div>

<style>
  .sun-hosts__others {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
  }
  .sun-hosts__others h2 {
    margin: var(--space-4) 0 0;
    font-family: var(--font-heading);
    font-weight: 400;
    font-size: 20px;
    color: var(--fg-1);
  }
  .sun-hosts__others > .sun-row__meta {
    margin: 0;
  }
  .sun-hosts__enrol {
    display: flex;
    flex-direction: column;
    gap: var(--space-4);
  }
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
