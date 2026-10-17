<!-- The task page's action bar, what fits the task's status: answer an escalated decision, approve or
     reject, retry failed steps, resume a paused run (or switch its runtime), steer a workflow's running agent
     now or for the next step, cancel. An agent's task has no steer box: its comments reach the agent. Stop
     (to Blocked) is in the page header. -->
<script lang="ts">
  import { Button, Dialog, Input, Radio } from '@hryer/madlabs-design-system/svelte';
  import IconButton from '../lib/ui/IconButton.svelte';
  import { api } from '../lib/api/client';
  import type { DecisionAnswer, RunEvent, TaskDetail } from '../lib/api/types';
  import { act, app, approve, reject, retry } from '../lib/app_state.svelte';
  import { SOURCE_LABEL, clock, is_paused, needs_approval, runtime_name, stuck_label } from '../lib/format';

  let { task, events, plain }: { task: TaskDetail; events: RunEvent[]; plain: boolean } = $props();

  const run = $derived(task.run);
  const run_id = $derived(run?.id);
  const stuck = $derived(stuck_label(task));
  let message = $state('');
  let confirm_cancel = $state(false);

  /** Steps a person can retry from where they failed; siblings the end cancelled restart with them. */
  const RETRYABLE = new Set(['failed', 'rejected', 'stalled', 'looping', 'budget_exceeded']);
  const retryable = $derived(task.status === 'failed' ? (run?.steps.filter((s) => RETRYABLE.has(s.status)) ?? []) : []);
  const kept = $derived(run?.steps.filter((s) => s.status === 'succeeded').length ?? 0);
  /** The test command Sun found in the repo of an unverified run. */
  const suggested_test = $derived(events.filter((e) => e.kind === 'gate_suggested').at(-1)?.payload.command as string | undefined);

  /** Steer now, or queue for when the current step ends (the submitter decides). */
  function send(e: SubmitEvent) {
    e.preventDefault();
    const queue = (e.submitter as HTMLButtonElement | null)?.value === 'queue';
    const body = message.trim();
    if (!run_id || !body) return;
    message = '';
    act(() => api.send_message(run_id, body, queue), queue ? 'Queued for the next step' : undefined);
  }

  // An escalated decide step: the model's suggestion and its question, from the latest
  // `decision` event of that step. A person approves the suggestion or answers differently.
  type Question = { type: 'choice' | 'score' | 'noul'; instructions: string; criteria: unknown };
  const deciding = $derived(run?.steps.find((s) => s.type === 'decide' && s.status === 'waiting_approval') ?? null);
  const asked = $derived(deciding ? events.filter((e) => e.kind === 'decision' && e.step_key === deciding.step_key).at(-1)?.payload : undefined);
  const question = $derived(asked?.question as Question | undefined);
  /** Radio options as `[value, label, description]`; a person answers yes/no where the model gives a probability. */
  const options = $derived.by((): [string, string, string][] => {
    const criteria = question?.criteria;
    if (question?.type === 'choice' && criteria && typeof criteria === 'object')
      return Object.entries(criteria as Record<string, unknown>).map(([label, about]) => [label, label, String(about)]);
    if (question?.type === 'score' && Array.isArray(criteria)) return criteria.map((level, i) => [String(i), String(level), `Level ${i}`]);
    if (question?.type === 'noul') return [['1', 'Yes', String(criteria)], ['0', 'No', '']];
    return [];
  });
  const suggestion = $derived.by(() => {
    const answer = asked?.answer as Record<string, unknown> | null | undefined;
    if (!answer) return null;
    const label = options.find(([value]) => value === String(answer.choice ?? answer.score ?? ''))?.[1];
    const said = label ?? ('noul' in answer ? `${Math.round(Number(answer.noul) * 100)}% likely` : String(answer.choice ?? answer.score));
    return { said, sure: Math.round(Number(asked?.confidence ?? 0) * 100) };
  });
  let answering = $state(false);
  let my_answer = $state('');

  async function answer_decision(e: SubmitEvent) {
    e.preventDefault();
    if (!question || !my_answer) return;
    const answer: DecisionAnswer =
      question.type === 'choice' ? { choice: my_answer } : question.type === 'score' ? { score: Number(my_answer) } : { noul: Number(my_answer) };
    if (await act(() => api.answer_decision(task.id, answer), 'Answer saved')) [answering, my_answer] = [false, ''];
  }

  // Switch runtime: the running or limit-hit agent step moves to another runtime of the
  // chain with the same hand-off pack as an automatic failover, in the same worktree.
  const switchable = $derived(run?.steps.find((s) => s.type === 'agent' && (s.status === 'running' || s.status === 'limit_hit')) ?? null);
  const switch_targets = $derived.by(() => {
    return (run?.runtime_chain ?? [])
      .filter((r) => r !== switchable?.runtime_id)
      .map((r) => {
        const rt = app.runtimes.find((x) => x.name === r);
        const cooling = rt?.cooldown_until ? ` · cooling down until ${clock(rt.cooldown_until)}` : '';
        return { value: r, label: runtime_name(app.runtimes, r), about: (rt ? SOURCE_LABEL[rt.source] : 'Runtime') + cooling };
      });
  });
  let picking_runtime = $state(false);
  let switch_to = $state('');
  let switching = $state(false);

  async function switch_runtime(e: SubmitEvent) {
    e.preventDefault();
    if (!run_id || !switchable || !switch_to) return;
    const [rid, key, to] = [run_id, switchable.step_key, switch_to];
    switching = true;
    if (await act(() => api.switch_runtime(rid, key, to), `Switched to ${runtime_name(app.runtimes, to)} with a hand-off pack`)) {
      [picking_runtime, switch_to] = [false, ''];
    }
    switching = false;
  }

  function cancel_task() {
    confirm_cancel = false;
    act(() => api.cancel_task(task.id), 'Task cancelled');
  }
</script>

<div class="actions">
  {#if needs_approval(task) && deciding && question && answering}
    <form class="actions__compose" onsubmit={answer_decision}>
      <fieldset class="answer">
        <legend class="sun-label">{question.instructions}</legend>
        {#each options as [value, label, description] (value)}
          <Radio bind:group={my_answer} {value} {label} {description} name="answer" />
        {/each}
      </fieldset>
      <p class="hint">An answer other than the suggestion is recorded as a correction.</p>
      <div class="row">
        <Button variant="ghost" onclick={() => (answering = false)}>Back</Button>
        <Button variant="mint" iconLeft="check" type="submit" disabled={!my_answer}>Save answer</Button>
      </div>
    </form>
  {:else if needs_approval(task) && deciding}
    <p class="hint">
      {#if suggestion}Suggested: <strong>{suggestion.said}</strong> · {suggestion.sure}% sure, so a person decides.{:else}No confident answer came back. Pick one.{/if}
    </p>
    <div class="row">
      <Button variant="secondary" iconLeft="x" onclick={() => reject(task)}>Reject</Button>
      {#if question}<Button variant="secondary" iconLeft="pencil" onclick={() => (answering = true)}>Answer differently</Button>{/if}
      {#if suggestion}<Button variant="mint" iconLeft="check" onclick={() => approve(task)}>Approve</Button>{/if}
    </div>
  {:else if needs_approval(task)}
    <div class="row">
      <Button variant="secondary" iconLeft="x" onclick={() => reject(task)}>Reject</Button>
      <Button variant="mint" iconLeft="check" onclick={() => approve(task)}>Approve</Button>
    </div>
  {:else if retryable.length && run}
    {@const rid = run.id}
    <p class="hint">Retry picks up from the failed step{plain ? '' : ' in the same worktree'}. {kept === 1 ? 'The finished step is kept.' : `The ${kept} finished steps are kept.`}</p>
    <div class="row">
      {#each retryable as s (s.step_key)}
        {@const key = s.step_key}
        <Button size="sm" variant="ice" iconLeft="rotate-ccw" onclick={() => act(() => api.retry_step(rid, key), `Retrying ${s.name}`)}>Retry “{s.name}”</Button>
      {/each}
      <Button size="sm" variant="ghost" iconLeft="refresh-cw" onclick={() => retry(rid)}>Start a new run</Button>
    </div>
  {:else if stuck && run}
    {@const rid = run.id}
    <p class="hint">The watchdog paused this step. Pick what happens next.</p>
    <div class="row">
      <Button size="sm" variant="ice" iconLeft="rotate-ccw" onclick={() => retry(rid)}>Retry with hand-off</Button>
      <Button size="sm" variant="ghost" onclick={() => (confirm_cancel = true)}>Cancel task</Button>
    </div>
  {:else if is_paused(task) && run}
    <p class="hint">
      {#if switchable?.status !== 'limit_hit'}Paused by a person. The agent waits until you resume.{:else if plain}The AI hit its usage limit. Resume once it resets.{:else}The runtime hit its limit. Switch to another one now, or resume after it resets.{/if}
    </p>
    <div class="row">
      {#if switchable && switch_targets.length && !plain}
        <Button size="sm" variant="secondary" iconLeft="arrow-left-right" onclick={() => (picking_runtime = true)}>Switch runtime</Button>
      {/if}
      <Button size="sm" variant="ice" iconLeft="play" onclick={() => act(() => api.resume_task(task.id), 'Resumed')}>Resume</Button>
      <Button size="sm" variant="ghost" onclick={() => (confirm_cancel = true)}>Cancel task</Button>
    </div>
  {:else if run?.status === 'running' && task.assignee?.kind === 'agent'}
    <div class="row">
      {#if switchable && switch_targets.length && !plain}
        <Button size="sm" variant="secondary" iconLeft="arrow-left-right" onclick={() => (picking_runtime = true)}>Switch runtime</Button>
      {/if}
      <Button size="sm" variant="ghost" onclick={() => (confirm_cancel = true)}>Cancel</Button>
    </div>
  {:else if run?.status === 'running'}
    <form class="actions__compose" onsubmit={send}>
      <div class="row row--fill">
        <div style="flex:1;min-width:0"><Input placeholder="Steer the agent now…" aria-label="Message to the agent" bind:value={message} /></div>
        <IconButton icon="send" label="Send" variant="ice" type="submit" value="now" />
      </div>
      <div class="row">
        <Button size="sm" variant="secondary" iconLeft="list-end" type="submit" value="queue">Queue for next step</Button>
        {#if switchable && switch_targets.length && !plain}
          <Button size="sm" variant="secondary" iconLeft="arrow-left-right" onclick={() => (picking_runtime = true)}>Switch runtime</Button>
        {/if}
        <Button size="sm" variant="ghost" onclick={() => (confirm_cancel = true)}>Cancel</Button>
      </div>
    </form>
  {:else if task.status === 'todo' || task.status === 'backlog'}
    <div class="row"><Button size="sm" variant="ghost" onclick={() => act(() => api.cancel_task(task.id), 'Task cancelled')}>Cancel task</Button></div>
  {:else if task.status === 'verifying'}
    <p class="hint">The engine decides when this is done. Failing gates go back to the agent before you're asked.</p>
  {:else if run}
    <p class="hint">{run.end_reason === 'unverified' ? 'Done without a test gate or approval — check the output yourself.' : 'Finished. The execution log is the full record.'}</p>
    {#if run.end_reason === 'unverified' && suggested_test}
      <p class="hint" role="note">Suggested gate: this repo tests with <code class="ml-mono">{suggested_test}</code>. Add <code class="ml-mono">test</code> to the workflow's gate step so the next “done” is checked.</p>
    {/if}
  {/if}
</div>

{#if switchable}
  <Dialog bind:open={picking_runtime} onclose={() => (picking_runtime = false)} title="Switch runtime">
    <form id="switch-runtime" onsubmit={switch_runtime}>
      <fieldset class="answer">
        <legend class="sun-label">Continue <span class="ml-mono">{switchable.step_key}</span> on</legend>
        {#each switch_targets as t (t.value)}
          <Radio bind:group={switch_to} value={t.value} label={t.label} description={t.about} name="switch_to" />
        {/each}
      </fieldset>
      <p class="hint">The current session stops. The new runtime picks up in the same worktree with a hand-off pack, and the gates check its work again.</p>
    </form>
    {#snippet footer()}
      <Button variant="secondary" onclick={() => (picking_runtime = false)}>Keep current</Button>
      <Button variant="ice" iconLeft="arrow-left-right" type="submit" form="switch-runtime" loading={switching} disabled={!switch_to}>Switch runtime</Button>
    {/snippet}
  </Dialog>
{/if}

<Dialog bind:open={confirm_cancel} onclose={() => (confirm_cancel = false)} title="Cancel {task.key}?">
  <p style="margin:0;color:var(--fg-2)">The agent stops now and the run ends as cancelled. This can’t be undone.</p>
  {#snippet footer()}
    <Button variant="secondary" onclick={() => (confirm_cancel = false)}>Keep running</Button>
    <Button variant="danger" onclick={cancel_task}>Cancel task</Button>
  {/snippet}
</Dialog>

<style>
  .actions:empty {
    display: none;
  }
  .actions,
  .actions__compose {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
  }
  .row {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: var(--space-2);
  }
  .row--fill {
    flex-wrap: nowrap;
  }
  .hint {
    margin: 0;
    font-size: 13px;
    color: var(--fg-2);
  }
  .answer {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    margin: 0;
    padding: 0;
    border: 0;
    min-width: 0;
  }
  .answer legend {
    margin-bottom: var(--space-2);
  }
</style>
