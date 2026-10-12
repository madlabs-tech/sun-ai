<!-- The task page's Pull requests section (multica's pull-requests-section + pull-request-list): the
     linked PRs, what merging them does to the task, and the two exceptions a person can make: link a PR
     by hand (the + dialog), or keep this one task’s status when its PRs merge (the actions menu). Each row is
     a link to the PR: state icon, title with the task's key stripped, `#number` (`repo#number` when the
     PRs span repos), the diff size or when it ended, and one verdict pill from `derive_verdict`. The
     first three rows show; the rest fold behind "Show n more". `refresh` bumps when a `pull_request` or
     `status` activity lands, so the list follows the webhooks. -->
<script lang="ts">
  import { Badge, Button, Dialog, Icon, Input } from '@hryer/madlabs-design-system/svelte';
  import { ApiFailure, api } from '../api/client';
  import type { PullRequest, TaskDetail, TaskPullRequests } from '../api/types';
  import { error_text, load_workspace, toast } from '../app_state.svelte';
  import { STATUS_BADGE, STATUS_ICON, time_ago } from '../format';
  import { derive_verdict, diff_count, has_stats, strip_task_key, type Verdict } from '../pull_request';
  import { go } from '../router.svelte';
  import IconButton from './IconButton.svelte';
  import Menu from './Menu.svelte';

  let { task, refresh, auto_link }: { task: TaskDetail; refresh: number; auto_link: boolean } = $props();

  let data = $state<TaskPullRequests | null>(null);
  let load_error = $state('');
  const load = (id: string) =>
    api.task_pull_requests(id).then(
      (d) => (data = d),
      (e) => (load_error = error_text(e)),
    );
  $effect(() => {
    const id = task.id;
    void refresh;
    load(id);
  });

  const prs = $derived(data?.pull_requests ?? []);
  /** The host may still be running checks on an open PR: its pill needs a newer snapshot. */
  const pending = $derived(prs.some((pr) => (pr.state === 'open' || pr.state === 'draft') && (!pr.snapshot_available || pr.checks_pending > 0)));
  // ponytail: a 30 s poll while checks run, since the kernel's snapshot refresh writes no activity
  // for the SSE feed; an SSE event for snapshot changes replaces it with a push.
  const POLL_MS = 30_000;
  $effect(() => {
    if (!pending) return;
    const id = task.id;
    let timer: ReturnType<typeof setInterval> | undefined;
    const start = () => (timer = setInterval(() => load(id), POLL_MS));
    // A hidden tab stops polling; coming back fetches once and polls again.
    const visibility = () => {
      clearInterval(timer);
      if (document.visibilityState !== 'visible') return;
      load(id);
      start();
    };
    if (document.visibilityState === 'visible') start();
    document.addEventListener('visibilitychange', visibility);
    return () => {
      clearInterval(timer);
      document.removeEventListener('visibilitychange', visibility);
    };
  });
  const auto = $derived(data?.auto_complete);
  /** The repo name only tells rows apart when the PRs span repos. */
  const show_repo = $derived(new Set(prs.map((pr) => `${pr.repo_owner}/${pr.repo_name}`)).size > 1);
  // The first 3 rows show inline; from 4 the rest fold.
  let expanded = $state(false);
  const folded = $derived(prs.length >= 4 && !expanded ? prs.length - 3 : 0);
  const shown = $derived(folded ? prs.slice(0, 3) : prs);
  const label = (pr: PullRequest) => `#${pr.number}`;
  const status_label = (s: string) => STATUS_BADGE[s as keyof typeof STATUS_BADGE]?.[1] ?? s;

  const STATE_ICON = {
    open: 'git-pull-request-arrow',
    draft: 'git-pull-request-draft',
    merged: 'git-merge',
    closed: 'git-pull-request-closed',
  } as const;

  type Pill = { icon: string; tone: 'neutral' | 'success' | 'warning' | 'danger' | 'info'; label: string; title?: string; spin?: boolean };
  function pill(v: Verdict): Pill | null {
    switch (v.kind) {
      case 'merged':
        return { icon: 'git-merge', tone: 'info', label: 'Merged' };
      case 'closed':
        return { icon: 'git-pull-request-closed', tone: 'neutral', label: 'Closed' };
      case 'failed':
        return { icon: 'circle-x', tone: 'danger', label: `${v.failed}/${v.total} failed` };
      case 'conflicting':
        return { icon: 'triangle-alert', tone: 'warning', label: 'Conflicts', title: 'Merge conflicts with the base branch' };
      case 'running':
        return { icon: 'loader', tone: 'warning', spin: true, label: `${v.passed}/${v.total}`, title: `${v.passed} of ${v.total} checks passed, ${v.running} running` };
      case 'behind':
        return { icon: 'circle-slash', tone: 'neutral', label: 'Behind base' };
      case 'blocked':
        return { icon: 'circle-slash', tone: 'neutral', label: 'Blocked' };
      case 'draft':
        return { icon: 'git-pull-request-draft', tone: 'neutral', label: 'Draft' };
      case 'ready':
        return { icon: 'circle-check', tone: 'success', label: 'Ready', title: 'Ready to merge' };
      case 'passed':
        return { icon: 'circle-check', tone: 'success', label: 'Checks passed', title: `All ${v.total} checks passed` };
      case 'no_checks':
        return { icon: 'circle', tone: 'neutral', label: 'No checks yet' };
      case 'unknown':
        return null;
    }
  }

  /** The second line's middle: the diff size while open, else when it ended. */
  function meta(pr: PullRequest): string {
    const ended = pr.state === 'merged' ? pr.merged_at : pr.state === 'closed' ? pr.closed_at : null;
    if (pr.state === 'merged' || pr.state === 'closed') return ended ? time_ago(ended) : '';
    return has_stats(pr) ? `+${diff_count(pr.additions)} −${diff_count(pr.deletions)}` : '';
  }

  const source_label = (pr: PullRequest) =>
    ({ manual: 'Linked manually', title: `Linked by ${task.key} in the title`, branch: 'Linked by the branch name', auto: 'Linked automatically' })[pr.link_source];

  /** States in which a merge has nothing left to do for this task. */
  const SETTLED = ['terminal', 'at_target'];

  /** Takes the kernel's answer; when the task moved (every remaining PR merged), the page reloads too. */
  function took(after: TaskPullRequests) {
    const moved = !!auto && !SETTLED.includes(auto.state) && SETTLED.includes(after.auto_complete.state);
    data = after;
    if (moved) load_workspace();
    return moved;
  }

  // Link by URL.
  let link_open = $state(false);
  let url = $state('');
  let link_error = $state('');
  let linking = $state(false);

  function open_link() {
    [url, link_error, link_open] = ['', '', true];
  }

  async function link(e: Event) {
    e.preventDefault();
    const value = url.trim();
    if (!value || linking) return;
    linking = true;
    try {
      took(await api.link_pull_request(task.id, { url: value }));
      link_open = false;
    } catch (err) {
      link_error = err instanceof ApiFailure && err.status === 404 ? 'Sun hasn’t received this PR yet. Check that its repository is connected.' : error_text(err);
    }
    linking = false;
  }

  async function unlink(pr: PullRequest) {
    try {
      const moved = took(await api.unlink_pull_request(task.id, pr.id));
      if (moved) toast('success', `Removed ${label(pr)}`, `Every remaining PR is merged, so the task moved to ${status_label(data!.auto_complete.target_status)}.`);
      else toast('success', `Removed ${label(pr)}`);
    } catch (e) {
      toast('danger', 'Couldn’t remove the pull request.', error_text(e));
    }
  }

  async function set_disabled(disabled: boolean) {
    try {
      took(await api.set_pr_auto_complete(task.id, disabled));
    } catch (e) {
      toast('danger', 'Couldn’t update the PR merge setting.', error_text(e));
    }
  }

  const automation_items = $derived([
    ...(auto?.workspace_enabled
      ? [
          auto.task_disabled
            ? { label: 'Change status when PRs merge', icon: 'rotate-ccw', onselect: () => set_disabled(false) }
            : { label: 'Keep status when PRs merge', icon: 'circle-slash', onselect: () => set_disabled(true) },
          null,
        ]
      : []),
    { label: 'PR merge settings', icon: 'settings', onselect: () => go('settings', 'code') },
  ]);

  /** The PRs the footer names, in list order. */
  const named = $derived(prs.filter((pr) => auto?.pull_request_ids.includes(pr.id)));
  const named_list = $derived(named.map(label).join(', '));
</script>

<section aria-labelledby="prs-head">
  <div class="prs__head">
    <h2 class="sun-label" id="prs-head">Pull requests {#if prs.length}<Badge count>{prs.length}</Badge>{/if}</h2>
    {#if auto}
      <span class="prs__actions">
        <IconButton icon="plus" label="Link pull request" variant="ghost" size="sm" onclick={open_link} />
        <Menu label="Pull request automation" icon="ellipsis" items={automation_items} />
      </span>
    {/if}
  </div>

  {#if load_error}
    <p class="prs__note" role="alert">{load_error}</p>
  {:else if !data}
    <p class="prs__note">Loading…</p>
  {:else if !prs.length}
    <p class="prs__note">No linked pull requests.</p>
  {:else}
    <ul class="prs" aria-label="Linked pull requests">
      {#each shown as pr (pr.id)}
        {@const verdict = derive_verdict(pr)}
        {@const p = pill(verdict)}
        {@const middle = meta(pr)}
        <li class="pr">
          <a class="pr__link" href={pr.html_url} target="_blank" rel="noopener noreferrer" title="{pr.repo_owner}/{pr.repo_name}#{pr.number}{pr.author_login ? ` · @${pr.author_login}` : ''}">
            <Icon name={STATE_ICON[pr.state]} size={14} class="pr__state pr__state--{pr.state}" />
            <span class="pr__body">
              <span class="pr__title">{strip_task_key(pr.title, task.key)}</span>
              <span class="pr__line">
                <span class="pr__meta ml-mono">{show_repo ? pr.repo_name : ''}#{pr.number}{#if middle}<span class="pr__dot" aria-hidden="true">·</span>{middle}{/if}</span>
                {#if p}
                  <Badge tone={p.tone} title={p.title} class="pr__pill"><Icon name={p.icon} size={12} class={p.spin ? 'pr__spin' : undefined} />{p.label}</Badge>
                {/if}
              </span>
              {#if verdict.kind === 'failed' && verdict.conflicting}
                <span class="pr__problem"><Icon name="triangle-alert" size={12} />Merge conflicts</span>
              {/if}
            </span>
          </a>
          {#if auto}
            <span class="pr__menu">
              <Menu label="Pull request actions" name="Actions for {label(pr)}" icon="ellipsis" items={[{ label: 'Remove from task', icon: 'unlink', group: source_label(pr), onselect: () => unlink(pr) }]} />
            </span>
          {/if}
        </li>
      {/each}
    </ul>
    {#if prs.length >= 4}
      <button type="button" class="prs__more" onclick={() => (expanded = !expanded)}>{folded ? `Show ${folded} more` : 'Show less'}</button>
    {/if}

    <!-- What the next merge does, straight from the kernel's decision; nothing when it changes nothing. -->
    {#if auto?.state === 'waiting' && named.length}
      <p class="prs__foot"><Icon name="circle-dashed" size={14} />Moves to <span class="prs__status"><Icon name={STATUS_ICON[auto.target_status]} size={12} />{status_label(auto.target_status)}</span> when {named_list} {named.length === 1 ? 'merges' : 'merge'}</p>
    {:else if auto?.state === 'not_merged' && named.length}
      <p class="prs__foot">
        <Icon name="triangle-alert" size={14} class="prs__warn" />{named_list} closed without merging{#if named.length === 1}<span class="pr__dot" aria-hidden="true">·</span><button type="button" class="prs__action" onclick={() => unlink(named[0])}>Remove</button>{/if}
      </p>
    {:else if auto?.state === 'all_merged'}
      <p class="prs__foot"><Icon name="circle-check" size={14} />All PRs merged. The next merged PR moves this task to <span class="prs__status"><Icon name={STATUS_ICON[auto.target_status]} size={12} />{status_label(auto.target_status)}</span>.</p>
    {:else if auto?.state === 'task_disabled'}
      <p class="prs__foot"><Icon name="circle-slash" size={14} />PR merges won’t change this task<span class="pr__dot" aria-hidden="true">·</span><button type="button" class="prs__action" onclick={() => set_disabled(false)}>Turn on</button></p>
    {/if}
  {/if}
</section>

<Dialog open={link_open} onclose={() => (link_open = false)} title="Link pull request" size="sm">
  <form id="link-pr" class="prs__form" onsubmit={link} novalidate>
    <!-- Not type="url": the browser would refuse "github.com/…" without a scheme, which the kernel takes. -->
    <Input
      label="Pull request URL"
      mono
      inputmode="url"
      autocomplete="off"
      spellcheck={false}
      placeholder="Paste a pull request URL"
      bind:value={url}
      oninput={() => (link_error = '')}
      error={link_error || undefined}
      hint={!link_error && auto_link ? `PRs with ${task.key} in the title or branch name link automatically.` : undefined}
    />
  </form>
  {#snippet footer()}
    <Button variant="secondary" onclick={() => (link_open = false)}>Cancel</Button>
    <Button variant="ice" type="submit" form="link-pr" loading={linking} disabled={!url.trim()}>Link</Button>
  {/snippet}
</Dialog>

<style>
  .prs__head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--space-2);
  }
  .prs__head .sun-label {
    display: flex;
    align-items: center;
    gap: var(--space-2);
  }
  .prs__actions {
    display: flex;
    gap: 2px;
    margin-top: -10px;
  }
  .prs__note,
  .prs__foot {
    margin: 0;
    font-size: 13px;
    color: var(--fg-2);
    line-height: 1.5;
  }
  .prs {
    list-style: none;
    margin: 0 calc(-1 * var(--space-2));
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  .pr {
    position: relative;
    border-radius: var(--radius-sm);
  }
  .pr:hover {
    background: var(--bg-subtle);
  }
  .pr__link {
    display: flex;
    align-items: flex-start;
    gap: var(--space-2);
    padding: 6px var(--space-2);
    padding-right: 36px;
    border-radius: var(--radius-sm);
    color: inherit;
    text-decoration: none;
  }
  .pr__link:focus-visible {
    outline: none;
    box-shadow: var(--ring-focus);
  }
  .pr__link :global(.pr__state) {
    flex: none;
    margin-top: 2px;
  }
  .pr__link :global(.pr__state--open) {
    color: var(--success-fg);
  }
  .pr__link :global(.pr__state--draft) {
    color: var(--fg-2);
  }
  .pr__link :global(.pr__state--merged) {
    color: var(--info-fg);
  }
  .pr__link :global(.pr__state--closed) {
    color: var(--danger-fg);
  }
  .pr__body {
    flex: 1;
    min-width: 0;
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  .pr__title {
    font-size: 13px;
    font-weight: 600;
    color: var(--fg-1);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .pr__line {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--space-2);
    min-width: 0;
  }
  .pr__meta {
    min-width: 0;
    font-size: 12px;
    color: var(--fg-2);
    font-variant-numeric: tabular-nums;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .pr__dot {
    margin: 0 6px;
    color: var(--fg-3);
  }
  .pr__line :global(.pr__pill) {
    flex: none;
    display: inline-flex;
    align-items: center;
    gap: 4px;
    font-variant-numeric: tabular-nums;
  }
  :global(.pr__spin) {
    animation: pr-spin 1s linear infinite;
  }
  @keyframes pr-spin {
    to {
      transform: rotate(360deg);
    }
  }
  @media (prefers-reduced-motion: reduce) {
    :global(.pr__spin) {
      animation: none;
    }
  }
  .pr__problem {
    display: inline-flex;
    align-items: center;
    gap: 4px;
    font-size: 12px;
    color: var(--warning-fg);
  }
  /* The row's menu sits in the corner, outside the link (no button inside an anchor). */
  .pr__menu {
    position: absolute;
    top: 2px;
    right: 2px;
  }
  .prs__more,
  .prs__action {
    padding: 0;
    border: 0;
    background: none;
    color: var(--fg-2);
    font: inherit;
    font-size: 12px;
    cursor: pointer;
  }
  .prs__more {
    display: block;
    width: 100%;
    min-height: 32px;
    margin-top: 2px;
    text-align: left;
  }
  .prs__action {
    font-weight: 600;
    color: var(--fg-1);
    text-decoration: underline;
    text-underline-offset: 2px;
  }
  .prs__more:hover,
  .prs__action:hover {
    color: var(--fg-1);
  }
  .prs__more:focus-visible,
  .prs__action:focus-visible {
    outline: none;
    box-shadow: var(--ring-focus);
    border-radius: var(--radius-sm);
  }
  .prs__foot {
    display: flex;
    align-items: flex-start;
    flex-wrap: wrap;
    gap: 4px 6px;
    margin-top: var(--space-2);
    padding-top: var(--space-2);
    border-top: 1px solid var(--border-subtle);
    font-size: 12px;
  }
  .prs__foot :global(.prs__warn) {
    color: var(--warning-fg);
  }
  .prs__status {
    display: inline-flex;
    align-items: center;
    gap: 4px;
    font-weight: 600;
    color: var(--fg-1);
  }
  .prs__form {
    display: grid;
    gap: var(--space-3);
  }
  @media (max-width: 1023px) {
    .prs__more {
      min-height: 44px;
    }
  }
</style>
