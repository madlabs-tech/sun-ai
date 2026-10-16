<!-- A task's Activity: comments (an agent's final message is its reply), moves, priority, assignee and project changes, and runs ("{runner} started
     a run", "{runner} completed a run", "Run 2 failed"), merged by time (the page streams the log live).
     A comment box sticks to the bottom: ⌘↵ / Ctrl+↵ sends, and on an agent's task the comment wakes it. -->
<script lang="ts">
  import { Button, Icon } from '@hryer/madlabs-design-system/svelte';
  import TextArea from '../lib/ui/TextArea.svelte';
  import { api } from '../lib/api/client';
  import type { Assignee, Project, TaskActivity, TaskDetail, TaskRun, TaskStatus, Workflow } from '../lib/api/types';
  import { SIDEBAR_SHORTCUT, app, can, error_text, toast } from '../lib/app_state.svelte';
  import { assignee_name } from '../lib/assignee';
  import { STATUS_BADGE, STATUS_ICON, actor_name, priority_of, stamp } from '../lib/format';

  let {
    task,
    activity,
    runs,
    runner,
    workflows,
    projects,
    on_comment,
  }: {
    task: TaskDetail;
    activity: TaskActivity[];
    runs: TaskRun[];
    /** Who runs the task: its agent's or workflow's name. */
    runner: string;
    workflows: Workflow[];
    projects: Project[];
    on_comment: (entry: TaskActivity) => void;
  } = $props();

  type Entry = { key: string; at: string; icon: string; who: string; text: string; comment?: string; tone?: 'danger' };

  const status_label = (s: unknown) => STATUS_BADGE[s as TaskStatus]?.[1] ?? String(s);
  const project_label = (id: unknown) => projects.find((p) => p.id === id)?.name ?? 'a project';

  function describe(a: TaskActivity): Entry {
    const b = a.body;
    const who = actor_name(b.by, app.me?.user);
    const base = { key: `a${a.seq}`, at: a.created_at, who };
    switch (a.kind) {
      case 'comment':
        if (b.by_agent) {
          const agent = app.agents.find((x) => x.id === b.by_agent)?.name ?? runner;
          return { ...base, who: agent, icon: 'bot', text: 'replied', comment: String(b.text ?? '') };
        }
        return { ...base, icon: 'message-square', text: 'commented', comment: String(b.text ?? '') };
      case 'created':
        return { ...base, icon: 'circle-plus', text: `created the task in ${status_label(b.status)}` };
      case 'status':
        return {
          ...base,
          icon: STATUS_ICON[b.to as TaskStatus] ?? 'circle',
          text: `moved it from ${status_label(b.from)} to ${status_label(b.to)}${b.source === 'pr_automation' ? ` after ${String(b.pull_requests ?? 'its pull requests')} merged` : ''}`,
        };
      case 'priority':
        return { ...base, icon: priority_of(b.to as never).icon, text: `set the priority to ${priority_of(b.to as never).label}` };
      case 'assignee':
        return { ...base, icon: 'user', text: b.to ? `assigned ${assignee_name(b.to as Assignee, workflows, app.agents)}` : 'removed the assignee' };
      case 'project':
        return { ...base, icon: 'folder-kanban', text: b.to ? `moved it to ${project_label(b.to)}` : 'took it out of its project' };
      case 'title':
        return { ...base, icon: 'pencil', text: `renamed it to “${String(b.to ?? '')}”` };
      case 'description':
        return { ...base, icon: 'pencil', text: 'edited the description' };
      case 'run':
        return { ...base, who: runner, icon: 'play', text: b.trigger === 'comment' ? 'started a run for a comment' : 'started a run' };
      case 'pull_request': {
        const pr = `PR #${String(b.number ?? '')}${b.repo ? ` on ${String(b.repo)}` : ''}`;
        if (b.action === 'merged') return { ...base, icon: 'git-merge', text: `saw ${pr} merge` };
        if (b.action === 'unlinked') return { ...base, icon: 'unlink', text: `removed ${pr}` };
        const how = ({ title: ' by its title', branch: ' by its branch name', auto: ' by a closing keyword' } as Record<string, string>)[String(b.source)] ?? '';
        return { ...base, icon: 'git-pull-request-arrow', text: `linked ${pr}${how}` };
      }
    }
  }

  /** A run's end, from the runs list (the log records only its start). */
  const ended = (r: TaskRun): Entry | null => {
    if (!r.ended_at) return null;
    const base = { key: `r${r.id}`, at: r.ended_at, who: runner };
    if (r.status === 'done') return { ...base, icon: 'circle-check', text: 'completed a run' };
    if (r.status === 'failed') return { ...base, who: `Run ${r.attempt}`, icon: 'circle-x', text: 'failed', tone: 'danger' };
    if (r.status === 'cancelled') return { ...base, who: `Run ${r.attempt}`, icon: 'circle-slash', text: 'was cancelled' };
    return null;
  };

  const entries = $derived(
    [...activity.map(describe), ...runs.map(ended).filter((e): e is Entry => !!e)].sort((a, b) => a.at.localeCompare(b.at)),
  );

  let draft = $state('');
  let sending = $state(false);
  const SEND_KEY = SIDEBAR_SHORTCUT.startsWith('⌘') ? '⌘↵' : 'Ctrl+↵';
  const wakes = $derived(task.assignee?.kind === 'agent');

  async function send(e?: Event) {
    e?.preventDefault();
    const text = draft.trim();
    if (!text || sending) return;
    sending = true;
    try {
      on_comment(await api.comment_task(task.id, text));
      draft = '';
    } catch (err) {
      toast('danger', 'Comment not sent', error_text(err));
    }
    sending = false;
  }

  function key(e: KeyboardEvent) {
    if (e.key === 'Enter' && (e.metaKey || e.ctrlKey)) send(e);
  }
</script>

<section class="activity" aria-labelledby="activity-head">
  <h2 class="sun-label" id="activity-head">Activity</h2>
  <ol class="activity__list">
    {#each entries as e (e.key)}
      <li class="entry" class:entry--comment={e.comment !== undefined}>
        <span class="entry__icon" class:entry__icon--danger={e.tone === 'danger'}><Icon name={e.icon} size={14} /></span>
        <div class="entry__body">
          <p class="entry__line"><strong>{e.who}</strong> {e.text} <time class="ml-mono entry__time" datetime={e.at}>{stamp(e.at)}</time></p>
          {#if e.comment !== undefined}<p class="entry__comment">{e.comment}</p>{/if}
        </div>
      </li>
    {:else}
      <li class="entry__empty">Nothing yet.</li>
    {/each}
  </ol>
</section>

{#if can('run')}
  <!-- svelte-ignore a11y_no_noninteractive_element_interactions: ⌘↵ sends from the comment field -->
  <form class="composer" onsubmit={send} onkeydown={key}>
    <TextArea aria-label="Comment" aria-describedby="comment-hint" placeholder="Leave a comment…" rows={2} maxlength={10000} bind:value={draft} />
    <div class="composer__row">
      <p class="composer__hint" id="comment-hint">{wakes ? `A comment wakes ${runner}: it reads it and picks the task up.` : 'Comments stay on the task for people to read.'}</p>
      <Button size="sm" variant="ice" iconLeft="send" type="submit" loading={sending} disabled={!draft.trim()} aria-keyshortcuts="Meta+Enter Control+Enter">
        Comment <kbd class="composer__kbd" aria-hidden="true">{SEND_KEY}</kbd>
      </Button>
    </div>
  </form>
{/if}

<style>
  .activity__list {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
  }
  .entry {
    display: flex;
    gap: var(--space-3);
    min-width: 0;
  }
  .entry__icon {
    width: 28px;
    height: 28px;
    flex: none;
    border-radius: 9px;
    display: grid;
    place-items: center;
    background: var(--surface-card);
    color: var(--fg-2);
  }
  .entry__icon--danger {
    color: var(--danger-fg);
  }
  .entry__body {
    flex: 1;
    min-width: 0;
    padding-top: 5px;
  }
  .entry__line {
    margin: 0;
    font-size: 13px;
    line-height: 1.45;
    color: var(--fg-2);
    overflow-wrap: anywhere;
  }
  .entry__line strong {
    color: var(--fg-1);
    font-weight: 600;
  }
  .entry__time {
    margin-left: 4px;
    font-size: 11px;
    color: var(--fg-3);
  }
  .entry--comment .entry__body {
    padding: var(--space-3);
    border-radius: var(--radius-lg);
    border: 1.5px solid var(--border-subtle);
    background: var(--surface-card);
  }
  .entry__comment {
    margin: var(--space-2) 0 0;
    font-size: 14px;
    line-height: 1.5;
    color: var(--fg-1);
    white-space: pre-wrap;
    overflow-wrap: anywhere;
  }
  .entry__empty {
    font-size: 13px;
    color: var(--fg-3);
  }
  .composer {
    position: sticky;
    bottom: 0;
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    margin-top: auto;
    padding: var(--space-3) 0;
    background: var(--bg-page);
    border-top: 1.5px solid var(--border-subtle);
  }
  .composer__row {
    display: flex;
    align-items: center;
    gap: var(--space-2);
  }
  .composer__hint {
    flex: 1;
    margin: 0;
    font-size: 12px;
    color: var(--fg-2);
  }
  .composer__kbd {
    margin-left: 6px;
    font: inherit;
    font-size: 12px;
    opacity: 0.75;
  }
  @media (max-width: 759px) {
    .composer__kbd {
      display: none;
    }
  }
</style>
