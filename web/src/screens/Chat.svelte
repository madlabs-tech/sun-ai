<!-- Chat: talk with the workspace's agents, one person and one agent per chat (only its creator sees it).
     The list shows each agent with your latest chat; opening one shows its messages (Task Activity in chat
     mode, live), what the agent is doing — queued behind which of its tasks, or replying with its words as
     they come — and New chat. From 760 px the list and the thread sit side by side; below, the thread is
     its own view (#/chat/<chat id>) with a way back. -->
<script lang="ts">
  import { Badge, Button, Icon } from '@hryer/madlabs-design-system/svelte';
  import PageHeader from '../lib/ui/PageHeader.svelte';
  import TaskActivity from './TaskActivity.svelte';
  import { api, watch_activity, watch_events } from '../lib/api/client';
  import type { Agent, Chat, TaskActivity as Activity } from '../lib/api/types';
  import { app, can, current_workspace, error_text, toast } from '../lib/app_state.svelte';
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
    return c.last.body.by_agent ? text : `You: ${text}`;
  };
</script>

<PageHeader title="Chat" subtitle="Ask an agent anything. Only you see your chats." />

<div class="sun-page chat" class:chat--open={!!route.id}>
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

  <section class="chat__thread" aria-label={agent ? `Chat with ${agent.name}` : 'Chat'}>
    {#if chat && agent}
      <header class="chat__head">
        <a class="chat__back" href={href('chat')}><Icon name="arrow-left" size={16} /> Agents</a>
        <h2 class="chat__title">{agent.name}</h2>
        {#if can('run')}<Button size="sm" variant="secondary" iconLeft="plus" loading={starting} onclick={() => start(agent!)}>New chat</Button>{/if}
      </header>
      <div class="chat__messages">
        <TaskActivity
          chat
          task={{ id: chat.id, assignee: { kind: 'agent', id: agent.id }, workspace_id: chat.workspace_id }}
          activity={messages}
          runs={[]}
          runner={agent.name}
          workflows={[]}
          projects={[]}
          on_comment={take}
        />
        <p class="chat__status" role="status">
          {#if chat.run?.status === 'queued'}
            <Icon name="clock" size={14} />
            {chat.queued_behind.length ? `Queued behind ${chat.queued_behind.join(', ')}` : `Queued: ${agent.name} starts when a slot frees up`}
          {:else if replying}
            <Icon name="loader" size={14} /> {agent.name} is replying…
          {/if}
        </p>
        {#if replying && typing}<p class="chat__typing">{typing}</p>{/if}
      </div>
    {:else if route.id && chats}
      <p class="sun-empty">This chat isn't here. <a href={href('chat')}>Back to your chats</a></p>
    {:else}
      <p class="sun-empty chat__pick">Pick an agent to chat with it.</p>
    {/if}
  </section>
</div>

<style>
  .chat {
    display: grid;
    grid-template-columns: minmax(0, 1fr);
    gap: var(--space-4);
    min-height: 0;
  }
  .chat--open .chat__agents,
  .chat:not(.chat--open) .chat__thread {
    display: none;
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
    .chat--open .chat__agents,
    .chat:not(.chat--open) .chat__thread {
      display: flex;
    }
    .chat__back {
      display: none;
    }
  }
</style>
