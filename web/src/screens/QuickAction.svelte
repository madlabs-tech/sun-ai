<!-- Notification deep link #/approve/<run_id>/<step_key>: a sheet over the tasks
     with the one decision this step needs, so it takes two taps — the link, then the action.
     Approval → Approve / Reject. Watchdog stop → Retry with hand-off. Running agent → Reply. -->
<script lang="ts">
  import { Badge, Button, Dialog, Icon, Input } from '@hryer/madlabs-design-system/svelte';
  import { api } from '../lib/api/client';
  import { act, app, approve, current_workspace, reject, retry, workspace_of } from '../lib/app_state.svelte';
  import { STATUS_BADGE, STUCK, money } from '../lib/format';
  import { go, href, show_workspace } from '../lib/router.svelte';

  let { run_id, step_key }: { run_id: string; step_key: string | null } = $props();

  let message = $state('');
  let busy = $state(false);

  const task = $derived(app.tasks.find((t) => t.run?.id === run_id) ?? null);

  // A link to a run on another workspace (a notification) opens that workspace.
  $effect(() => {
    if (!app.loaded || task) return;
    api.list_tasks().then(
      (all) => {
        const workspace = workspace_of(all.find((t) => t.run?.id === run_id)?.workspace_id ?? '');
        if (workspace && workspace.id !== current_workspace()) show_workspace(workspace.slug);
      },
      () => {},
    );
  });
  const step = $derived(task?.run?.steps.find((s) => s.step_key === step_key) ?? null);
  const kind = $derived(
    !app.loaded || app.workspace !== current_workspace() ? 'loading'
    : !task || !step ? 'missing'
    : step.status === 'waiting_approval' ? 'approve'
    : STUCK[step.status] ? 'retry'
    : step.status === 'running' && step.type === 'agent' ? 'reply'
    : 'done',
  );
  const TITLE = { loading: 'Loading…', missing: 'Run not found', approve: 'Approve this step?', retry: 'Restart this step?', reply: 'Reply to the agent', done: 'Nothing to do' };

  const close = () => go('tasks');

  // Screen readers and keyboards start inside the sheet (the DS Dialog doesn't move focus).
  $effect(() => document.querySelector<HTMLElement>('.ml-dialog')?.focus());

  /** Second tap: run the decision, then show the task with its new state. */
  async function decide(work: () => Promise<boolean>) {
    busy = true;
    const ok = await work();
    busy = false;
    if (ok && task) go('tasks', task.key);
  }

  function send(e: SubmitEvent) {
    e.preventDefault();
    const body = message.trim();
    if (body) decide(() => act(() => api.send_message(run_id, body), 'Sent to the agent'));
  }
</script>

<Dialog open onclose={close} title={TITLE[kind]}>
  {#if task && step}
    {@const [tone, label] = STATUS_BADGE[task.status]}
    <a class="qa-task" href={href('tasks', task.key)} aria-label="Open task {task.key}: {task.title}">
      <div class="qa-row">
        <span class="ml-mono" style="font-size:12px">{task.key}</span>
        <Badge {tone} dot>{label}</Badge>
        {#if kind === 'retry'}<Badge tone="warning">{STUCK[step.status]}</Badge>{/if}
      </div>
      <strong class="qa-title">{task.title}</strong>
      <span>{step.name} · {task.workflow?.name ?? 'No workflow'} · <span class="ml-mono">{money(task.run?.cost_usd ?? 0)}</span></span>
      <span class="qa-open"><Icon name="chevron-right" size={18} /></span>
    </a>
    {#if kind === 'retry'}
      <p>The watchdog stopped this step. Retry starts it again with a hand-off pack of what was done so far.</p>
    {:else if kind === 'reply'}
      <form id="qa-reply" onsubmit={send}>
        <Input label="Message to the agent" placeholder="e.g. Keep the diff small" bind:value={message} />
      </form>
    {:else if kind === 'done'}
      <p>This step is {step.status.replaceAll('_', ' ')}. Someone may have acted on it already.</p>
    {/if}
  {:else if kind === 'missing'}
    <p>This run isn’t in the task list any more. It may have been retried or cancelled.</p>
  {/if}

  {#snippet footer()}
    {#if task && kind === 'approve'}
      {@const t = task}
      <Button variant="secondary" iconLeft="x" disabled={busy} onclick={() => decide(() => reject(t))}>Reject</Button>
      <Button variant="mint" iconLeft="check" loading={busy} onclick={() => decide(() => approve(t))}>Approve</Button>
    {:else if task?.run && kind === 'retry'}
      {@const rid = task.run.id}
      <Button variant="ice" iconLeft="rotate-ccw" loading={busy} onclick={() => decide(() => retry(rid))}>Retry with hand-off</Button>
    {:else if task && kind === 'reply'}
      <Button variant="ice" iconLeft="send" type="submit" form="qa-reply" loading={busy}>Send</Button>
    {:else if task}
      <Button variant="ice" href={href('tasks', task.key)}>Open task</Button>
    {:else if kind === 'missing'}
      <Button variant="ice" onclick={close}>Open tasks</Button>
    {/if}
  {/snippet}
</Dialog>

<style>
  .qa-task {
    position: relative;
    display: flex;
    flex-direction: column;
    gap: 4px;
    padding: var(--space-3) var(--space-4);
    border-radius: var(--radius-lg);
    background: var(--surface-sunken);
    padding-right: var(--space-8);
    font-size: 13px;
    color: var(--fg-2);
    text-decoration: none;
    overflow-wrap: anywhere;
    transition: background var(--dur-fast) var(--ease-out);
  }
  .qa-task:hover {
    background: var(--bg-subtle);
    text-decoration: none;
  }
  .qa-open {
    position: absolute;
    right: var(--space-3);
    top: 50%;
    translate: 0 -50%;
    display: flex;
  }
  .qa-row {
    display: flex;
    align-items: center;
    gap: var(--space-2);
  }
  .qa-title {
    font-size: 15px;
    color: var(--fg-1);
  }
  p {
    margin: var(--space-3) 0 0;
  }
  form {
    margin-top: var(--space-4);
  }
</style>
