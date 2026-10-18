<!-- Chat: talk with the workspace's agents, or straight with a tool a host reported (no agent: no instructions,
     skills or memory), one person per chat (only its creator sees it). The list shows each agent with your latest
     chat, then your chats without an agent; opening one shows its messages (Task Activity in chat mode, live),
     what it is doing — queued behind which of its tasks, or replying with its words as they come — and New chat.
     A chat without an agent is started from a picker: the tool (one that can't run now greyed, with why), its
     host, model and effort (from what the hosts report), the mode (bypass only for who may pick it, with a host
     to run it on) and who pays. The picker remembers the last choice in this browser; on an open chat it switches
     model, effort or mode (a fresh session that reads the thread), and another tool or host starts a new chat.
     From 760 px the list and the thread sit side by side; below, the thread is its own view (#/chat/<chat id>)
     with a way back. -->
<script lang="ts">
  import { Badge, Button, Dialog, Icon, Radio, Select } from '@hryer/madlabs-design-system/svelte';
  import PageHeader from '../lib/ui/PageHeader.svelte';
  import ChainSettings from '../lib/ui/ChainSettings.svelte';
  import IconButton from '../lib/ui/IconButton.svelte';
  import TaskActivity from './TaskActivity.svelte';
  import { api, watch_activity, watch_events } from '../lib/api/client';
  import type { Agent, ChainEntry, ChainSpec, Chat, DirectSettings, Host, PermissionMode, Runtime, TaskActivity as Activity } from '../lib/api/types';
  import { app, can, current_workspace, error_text, local_get, local_set, toast } from '../lib/app_state.svelte';
  import { SOURCE_LABEL, chain_modes, chain_names, clock, direct_line, direct_name, entry_name, payer, tool_label } from '../lib/format';
  import { bypass_off, isolated, mode_hint, mode_options } from '../lib/permission_mode';
  import { go, href, route } from '../lib/router.svelte';

  const workspace = current_workspace();
  let chats = $state<Chat[] | null>(null);
  const refresh = () =>
    api.list_chats(workspace).then(
      (list) => (chats = list),
      (err) => chats ?? toast('danger', 'Chats not loaded', error_text(err)),
    );
  refresh();
  // ponytail: chats have no stream of their own; the list polls (for the status line), messages stream.
  $effect(() => {
    const timer = setInterval(() => document.visibilityState === 'visible' && refresh(), 3000);
    return () => clearInterval(timer);
  });

  const chat = $derived(chats?.find((c) => c.id === route.id));
  const agent = $derived(app.agents.find((a) => a.id === chat?.agent_id));
  /** Who answers in the open chat: its agent, else its tool. */
  const name = $derived(agent?.name ?? (chat?.runtime ? direct_name(chat.runtime) : ''));
  const direct_chats = $derived(chats?.filter((c) => c.runtime) ?? []);
  /** Your latest chat with `a` (the list is last active first). */
  const latest = (a: Agent) => chats?.find((c) => c.agent_id === a.id);

  let starting = $state(false);
  async function start(a: Agent) {
    if (starting) return;
    starting = true;
    try {
      const made = await api.create_chat(a.id);
      chats = [made, ...(chats ?? [])];
      go('chat', made.id);
    } catch (err) {
      toast('danger', 'Chat not started', error_text(err));
    }
    starting = false;
  }

  const open = (a: Agent) => {
    const last = latest(a);
    if (last) go('chat', last.id);
    else start(a);
  };

  // The open chat's messages, live.
  let messages = $state<Activity[]>([]);
  function take(a: Activity) {
    if (a.seq <= (messages.at(-1)?.seq ?? 0)) return;
    messages.push(a);
    refresh();
  }
  $effect(() => {
    const id = route.id;
    messages = [];
    if (!id) return;
    return watch_activity(id, 0, take, () => {});
  });

  // While the agent replies: its latest words, from the run's timeline.
  const replying = $derived(chat?.run?.status === 'running' ? chat.run.id : null);
  let typing = $state('');
  $effect(() => {
    typing = '';
    if (!replying) return;
    return watch_events(
      replying,
      0,
      (e) => {
        if (e.kind === 'text' && e.payload.role !== 'user') typing = e.payload.summary;
      },
      () => {},
    );
  });

  const preview = (c: Chat | undefined) => {
    if (!c?.last) return 'No messages yet';
    const text = String(c.last.body.text ?? '');
    return c.last.body.by_agent || c.last.body.by_tool ? text : `You: ${text}`;
  };

  // The picker of a chat without an agent.
  let runtimes = $state<Runtime[]>([]);
  api.list_runtimes(workspace).then(
    (rows) => (runtimes = rows),
    () => {},
  );
  /** `null` until listed (or not visible to the caller): placement still refuses bypass on a bare host. */
  let hosts = $state<Host[] | null>(null);
  api.list_hosts(workspace).then(
    (rows) => (hosts = rows),
    () => {},
  );
  const NOT_READY: Record<string, string> = { offline: 'Host offline', needs_login: 'Needs login', limited: 'Limit reached', unsupported: 'Not supported yet' };
  /** Each tool or key-backed runtime once: what the radio says, and why it can't take a chat now ('' when it can). */
  const tools = $derived(
    chain_names(runtimes).map((n) => {
      const reports = runtimes.filter((r) => r.name === n);
      const r = reports[0];
      const ready = reports.some((x) => x.state === 'idle' || x.state === 'busy');
      const cooldown = r.state === 'limited' && r.cooldown_until ? `, back at ${clock(r.cooldown_until)}` : '';
      const why = ready ? '' : `${NOT_READY[r.state] ?? r.state}${r.detail ? `: ${r.detail}` : cooldown}.`;
      return { name: n, label: r.host ? tool_label(r.tool) : n, why, source: SOURCE_LABEL[r.source] };
    }),
  );

  const REMEMBER = `sun.direct_chat.${workspace}`;
  let picking = $state(false);
  /** The chat whose model, effort or mode the picker switches; `null`: it starts a new chat. */
  let switching = $state<Chat | null>(null);
  /** The picked runtime name, and its one-entry chain (ChainSettings sets its host, model and effort). */
  let tool = $state('');
  let chain = $state<ChainEntry[]>([]);
  let mode = $state<PermissionMode | ''>('');
  let error = $state('');
  let saving = $state(false);

  /** A runtime name as a chain spec: a key-backed runtime pays with its own name. */
  function spec_of(n: string): ChainSpec {
    const r = runtimes.find((x) => x.name === n);
    return r && !r.host ? { tool: r.adapter, credential: r.name } : { tool: n };
  }
  const spec = $derived(!tool ? null : typeof chain[0] === 'object' ? chain[0] : spec_of(tool));
  const off = $derived(bypass_off(runtimes));
  const safe = $derived(!hosts || isolated(hosts, spec?.host));
  const modes = $derived(mode_options(chain_modes(runtimes, spec ? [spec] : []), !off, safe, switching?.permission_mode ?? null));
  $effect(() => {
    if (mode && !modes.some((o) => o.value === mode)) mode = '';
  });
  const same_target = (a: ChainSpec, b: ChainSpec) => a.tool === b.tool && a.credential === b.credential && a.host === b.host;
  /** Another tool, credential or host than the switched chat's is a new chat. */
  const patching = $derived(!!switching?.runtime && !!spec && same_target(switching.runtime, spec));

  /** The last settings picked here, if their tool can take a chat now. */
  function remembered(): DirectSettings | null {
    try {
      const settings = JSON.parse(local_get(REMEMBER)) as DirectSettings;
      return tools.some((t) => t.name === entry_name(settings.runtime) && !t.why) ? settings : null;
    } catch {
      return null;
    }
  }

  /** Opens the picker on `from`'s settings (else the remembered ones); `switch_it`: to switch `from`. */
  function pick(from: Chat | null, switch_it = false) {
    const settings = from?.runtime ? { runtime: from.runtime, permission_mode: from.permission_mode } : remembered();
    switching = switch_it ? from : null;
    tool = settings ? entry_name(settings.runtime) : '';
    chain = settings ? [settings.runtime] : [];
    mode = settings?.permission_mode ?? '';
    error = '';
    picking = true;
  }

  async function submit(e: SubmitEvent) {
    e.preventDefault();
    if (!spec) return void (error = 'Pick a tool.');
    const settings: DirectSettings = { runtime: $state.snapshot(spec), permission_mode: mode || null };
    saving = true;
    error = '';
    try {
      const made = patching ? await api.update_chat(switching!.id, settings) : await api.create_direct_chat(workspace, settings);
      local_set(REMEMBER, JSON.stringify(settings));
      chats = [made, ...(chats ?? []).filter((c) => c.id !== made.id)];
      picking = false;
      go('chat', made.id);
    } catch (err) {
      error = error_text(err);
    }
    saving = false;
  }
</script>

<PageHeader title="Chat" subtitle="Ask an agent anything, or talk to a tool directly. Only you see your chats." />

<div class="sun-page chat" class:chat--open={!!route.id}>
  <div class="chat__side">
    <nav class="chat__agents" aria-label="Agents">
      {#each app.agents as a (a.id)}
        {@const last = latest(a)}
        <button type="button" class="chat__agent" aria-current={chat && chat.agent_id === a.id ? 'true' : undefined} onclick={() => open(a)} disabled={!can('run') && !last}>
          <span class="sun-tile sun-tile--ice" aria-hidden="true"><Icon name="bot" size={18} /></span>
          <span class="chat__agent-body">
            <span class="chat__agent-name">{a.name} {#if a.ready}<Badge tone="success" dot>Ready</Badge>{/if}</span>
            <span class="chat__agent-last">{preview(last)}</span>
          </span>
        </button>
      {:else}
        <p class="sun-empty">No agents on this workspace yet. Add one on Agents, then chat with it here.</p>
      {/each}
    </nav>
    <nav class="chat__agents" aria-label="Without an agent">
      <h2 class="sun-label">Without an agent</h2>
      {#each direct_chats as c (c.id)}
        <button type="button" class="chat__agent" aria-current={chat?.id === c.id ? 'true' : undefined} onclick={() => go('chat', c.id)}>
          <span class="sun-tile" aria-hidden="true"><Icon name="terminal" size={18} /></span>
          <span class="chat__agent-body">
            <span class="chat__agent-name">{direct_name(c.runtime!)}</span>
            <span class="chat__agent-last">{preview(c)}</span>
          </span>
        </button>
      {/each}
      {#if can('run')}<Button size="sm" variant="secondary" iconLeft="plus" onclick={() => pick(null)}>Chat with a tool</Button>{/if}
    </nav>
  </div>

  <section class="chat__thread" aria-label={name ? `Chat with ${name}` : 'Chat'}>
    {#if chat && name}
      <header class="chat__head">
        <a class="chat__back" href={href('chat')}><Icon name="arrow-left" size={16} /> Chats</a>
        <h2 class="chat__title">{name}</h2>
        {#if chat.runtime && can('run')}<IconButton icon="sliders-horizontal" label="Switch model, effort or mode" variant="ghost" size="sm" onclick={() => pick(chat!, true)} />{/if}
        {#if can('run')}<Button size="sm" variant="secondary" iconLeft="plus" loading={starting} onclick={() => (agent ? start(agent) : pick(chat!))}>New chat</Button>{/if}
      </header>
      {#if chat.runtime}
        <p class="chat__settings"><span>{direct_line({ runtime: chat.runtime, permission_mode: chat.permission_mode })}</span> <span><Icon name="wallet" size={14} /> {payer(chat.runtime, runtimes)}</span></p>
      {/if}
      <div class="chat__messages">
        <TaskActivity
          chat
          task={{ id: chat.id, assignee: agent ? { kind: 'agent', id: agent.id } : null, workspace_id: chat.workspace_id }}
          activity={messages}
          runs={[]}
          runner={name}
          workflows={[]}
          projects={[]}
          on_comment={take}
        />
        <p class="chat__status" role="status">
          {#if chat.run?.status === 'queued'}
            <Icon name="clock" size={14} />
            {chat.queued_behind.length ? `Queued behind ${chat.queued_behind.join(', ')}` : `Queued: ${name} starts when a slot frees up`}
          {:else if replying}
            <Icon name="loader" size={14} /> {name} is replying…
          {/if}
        </p>
        {#if replying && typing}<p class="chat__typing">{typing}</p>{/if}
      </div>
    {:else if route.id && chats}
      <p class="sun-empty">This chat isn't here. <a href={href('chat')}>Back to your chats</a></p>
    {:else}
      <p class="sun-empty chat__pick">Pick an agent to chat with it, or chat with a tool directly.</p>
    {/if}
  </section>
</div>

<Dialog open={picking} onclose={() => (picking = false)} title={switching ? `Switch ${direct_name(switching.runtime!)}` : 'Chat with a tool'}>
  <form id="direct-chat" class="direct" onsubmit={submit} novalidate>
    <fieldset class="direct__tools">
      <legend class="ml-field__label">Tool</legend>
      {#each tools as t (t.name)}
        <Radio bind:group={tool} name="tool" value={t.name} label={t.label} description={t.why || t.source} disabled={!!t.why} onchange={() => (chain = [t.name])} />
      {:else}
        <p class="sun-empty">No host has reported a tool yet. Join a host, then chat with its tools here.</p>
      {/each}
    </fieldset>
    {#if spec}
      <ChainSettings {runtimes} bind:chain />
      <Select label="Permissions" options={modes} bind:value={mode} hint={mode_hint(mode, off, safe)} />
      <p class="direct__payer"><Icon name="wallet" size={14} /> {payer(spec, runtimes)}</p>
      {#if switching && !patching}<p class="direct__note">Another tool or host starts a new chat; this one stays as it is.</p>{/if}
    {/if}
    {#if error}<p class="direct__error" role="alert">{error}</p>{/if}
  </form>
  {#snippet footer()}
    <Button variant="secondary" onclick={() => (picking = false)}>Cancel</Button>
    <Button variant="ice" type="submit" form="direct-chat" loading={saving}>{patching ? 'Switch' : 'Start chat'}</Button>
  {/snippet}
</Dialog>

<style>
  .chat {
    display: grid;
    grid-template-columns: minmax(0, 1fr);
    gap: var(--space-4);
    min-height: 0;
  }
  .chat--open .chat__side,
  .chat:not(.chat--open) .chat__thread {
    display: none;
  }
  .chat__side {
    display: flex;
    flex-direction: column;
    gap: var(--space-4);
  }
  .chat__agents {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
  }
  .chat__agent {
    display: flex;
    align-items: center;
    gap: var(--space-3);
    width: 100%;
    padding: var(--space-2) var(--space-3);
    border: 1.5px solid transparent;
    border-radius: var(--radius-lg);
    background: none;
    color: var(--fg-1);
    font: inherit;
    text-align: left;
    cursor: pointer;
  }
  .chat__agent:hover,
  .chat__agent[aria-current='true'] {
    background: var(--surface-card);
    border-color: var(--border-subtle);
  }
  .chat__agent-body {
    display: flex;
    flex-direction: column;
    min-width: 0;
  }
  .chat__agent-name {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    font-weight: 600;
    font-size: 14px;
  }
  .chat__agent-last {
    font-size: 12px;
    color: var(--fg-2);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .chat__thread {
    display: flex;
    flex-direction: column;
    min-width: 0;
  }
  .chat__head {
    display: flex;
    align-items: center;
    gap: var(--space-3);
    padding-bottom: var(--space-3);
    border-bottom: 1.5px solid var(--border-subtle);
  }
  .chat__back {
    display: inline-flex;
    align-items: center;
    gap: var(--space-1);
    font-size: 13px;
    color: var(--fg-link);
  }
  .chat__title {
    flex: 1;
    margin: 0;
    font-size: 16px;
  }
  .chat__settings {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-1) var(--space-3);
    margin: var(--space-2) 0 0;
    font-size: 12px;
    color: var(--fg-2);
  }
  .chat__settings span,
  .direct__payer {
    display: inline-flex;
    align-items: center;
    gap: var(--space-1);
  }
  .direct {
    display: flex;
    flex-direction: column;
    gap: var(--space-4);
  }
  .direct__tools {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    margin: 0;
    padding: 0;
    border: 0;
    min-width: 0;
  }
  .direct__payer,
  .direct__note {
    margin: 0;
    font-size: 13px;
    color: var(--fg-2);
  }
  .direct__error {
    margin: 0;
    font-size: 13px;
    color: var(--danger-fg);
  }
  .chat__messages {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
    padding-top: var(--space-3);
  }
  .chat__status {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    min-height: 1.5em;
    margin: 0;
    font-size: 12px;
    color: var(--fg-2);
  }
  .chat__typing {
    margin: 0;
    padding: var(--space-3);
    border-radius: var(--radius-lg);
    border: 1.5px dashed var(--border-subtle);
    font-size: 14px;
    line-height: 1.5;
    color: var(--fg-2);
    white-space: pre-wrap;
    overflow-wrap: anywhere;
  }
  @media (min-width: 760px) {
    .chat {
      grid-template-columns: 260px minmax(0, 1fr);
    }
    .chat--open .chat__side,
    .chat:not(.chat--open) .chat__thread {
      display: flex;
    }
    .chat__back {
      display: none;
    }
  }
</style>
