<!-- Execution log, a task's evidence: sources to check (stale or conflicting docs), data the diff touched,
     and how the merged PR held up. Styles: TaskRunLog's shared section classes. -->
<script lang="ts">
  import { Badge, Button, Icon } from '@hryer/madlabs-design-system/svelte';
  import { api } from '../lib/api/client';
  import type { BugIssue, KnowledgeFlag, TaskQualityView } from '../lib/api/types';
  import { act } from '../lib/app_state.svelte';
  import { cited } from '../lib/format';

  let { task_id, flags, quality = $bindable() }: { task_id: string; flags: KnowledgeFlag[]; quality: TaskQualityView } = $props();

  let checking = $state(false);

  const RULE_LABEL: Record<string, string> = {
    migration: 'Migration',
    schema: 'Schema',
    seed_data: 'Seed data',
    sql: 'SQL',
    orm_model: 'ORM model',
    data_config: 'Data config',
    custom: 'Custom rule',
    database: 'Database',
    table: 'Table',
    api: 'API',
  };
  const HELD_BADGE = {
    true: ['success', 'Held up', 'circle-check'],
    false: ['danger', 'Needed follow-up', 'circle-alert'],
    null: ['info', 'Waiting for CI', 'loader'],
  } as const;
  const short_sha = (sha: string) => sha.slice(0, 7);
  /** How a bug issue relates to the merged PR. */
  const BUG_LABEL: Record<BugIssue['relation'], string> = {
    origin: 'Started task',
    referenced: 'Linked',
    regression: 'Regression',
  };
  const FLAG_BADGE = { stale: ['warning', 'Stale'], conflict: ['danger', 'Conflict'] } as const;

  async function check_now() {
    checking = true;
    await act(() => api.check_quality(task_id), 'Checked');
    quality = await api.task_quality(task_id).catch(() => quality);
    checking = false;
  }
</script>

{#if flags.length}
  <section aria-labelledby="flags-head">
    <h3 class="sun-label timeline-head" id="flags-head">Sources to check<Badge tone="warning" count>{flags.length}</Badge></h3>
    <p class="hint">Some of this task's docs are old or disagree. Agents are told not to pick one silently.</p>
    <ul class="flags">
      {#each flags as f, i (i)}
        {@const [tone, label] = FLAG_BADGE[f.kind]}
        <li class="flag">
          <div class="flag__line">
            <Badge {tone}>{label}</Badge>
            <span class="flag__message">{f.message}</span>
          </div>
          <ul class="flag__cites">
            {#each f.citations as c, j (j)}
              {@const { text, url } = cited(c)}
              <li class="ml-mono">
                <Icon name="file-text" size={12} />
                {#if url}<a href={url} target="_blank" rel="noopener noreferrer">{text}</a>{:else}{text}{/if}
              </li>
            {/each}
          </ul>
        </li>
      {/each}
    </ul>
  </section>
{/if}

{#if quality.data_touched.length}
  <section aria-labelledby="data-head">
    <h3 class="sun-label timeline-head" id="data-head">Data touched<Badge tone="warning" count>{quality.data_touched.length}</Badge></h3>
    <p class="hint">Detected from the diff, best effort. Check these files before you approve.</p>
    <ul class="touched">
      {#each quality.data_touched as f (f.path)}
        <li>
          <span class="touched__rule"><Icon name="database" size={14} />{RULE_LABEL[f.rule] ?? f.rule}</span>
          <span class="ml-mono touched__path">{f.path}</span>
        </li>
      {/each}
    </ul>
  </section>
{/if}

{#if quality.quality}
  {@const q = quality.quality}
  {@const [held_tone, held_label, held_icon] = HELD_BADGE[`${quality.held_up}`]}
  {@const regressions = q.bug_issues.filter((b) => b.relation === 'regression').length}
  <section aria-labelledby="merge-head">
    <h3 class="sun-label timeline-head" id="merge-head">After merge<Badge tone={held_tone}><Icon name={held_icon} size={12} />{held_label}</Badge></h3>
    <dl class="meta">
      <dt>Merged</dt>
      <dd><span class="ml-mono">{short_sha(q.merge_sha)}</span> into {q.base}{q.pr_number ? ` · PR #${q.pr_number}` : ''}</dd>
      <dt>CI on {q.base}</dt>
      <dd>{q.ci_state ?? 'not checked'}</dd>
      <dt>Reverted</dt>
      <dd>{#if q.reverted_by}<span class="ml-mono">{short_sha(q.reverted_by)}</span>{:else}No{/if}</dd>
      <dt>Fixes</dt>
      <dd>{q.fix_commits.length ? `${q.fix_commits.length} within ${q.window_days} days` : `None within ${q.window_days} days`}</dd>
      <dt>Bug reports</dt>
      <dd>{regressions ? `${regressions} within ${q.window_days} days` : `None within ${q.window_days} days`}</dd>
    </dl>
    {#if q.fix_commits.length}
      <ul class="touched" aria-label="Fix commits">
        {#each q.fix_commits as c (c.sha)}
          <li><span class="ml-mono touched__rule">{short_sha(c.sha)}</span><span class="touched__path">{c.subject}</span></li>
        {/each}
      </ul>
    {/if}
    {#if q.bug_issues.length}
      <ul class="touched" aria-label="Bug issues">
        {#each q.bug_issues as b (`${b.relation}:${b.key}`)}
          <li>
            <span class="touched__rule"><Icon name={b.relation === 'regression' ? 'bug' : 'link'} size={14} />{BUG_LABEL[b.relation]}</span>
            <span class="touched__path">
              {#if b.url}<a class="ml-mono" href={b.url} target="_blank" rel="noopener noreferrer">{b.key}</a>{:else}<span class="ml-mono">{b.key}</span>{/if}
              {b.title ?? ''}
            </span>
          </li>
        {/each}
      </ul>
    {/if}
    <div class="row">
      <Button size="sm" variant="secondary" iconLeft="refresh-cw" disabled={checking} onclick={check_now}>
        {checking ? 'Checking…' : 'Check now'}
      </Button>
    </div>
  </section>
{/if}
