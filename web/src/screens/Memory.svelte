<!-- Long-term memory: proposals to approve or reject, active items to retire, and whether
     agents are improving: repeat mistakes, first-pass gates and review rejections. -->
<script lang="ts">
  import { Badge, Card } from '@hryer/madlabs-design-system/svelte';
  import PageHeader from '../lib/ui/PageHeader.svelte';
  import MemoryCard from '../lib/ui/MemoryCard.svelte';
  import { api } from '../lib/api/client';
  import type { MemoryItem, MemoryMetrics, PassRate, RepeatRate } from '../lib/api/types';
  import { error_text } from '../lib/app_state.svelte';
  import { day } from '../lib/format';

  let items = $state<MemoryItem[]>([]);
  let metrics = $state<MemoryMetrics | null>(null);
  let error = $state('');
  let loaded = $state(false);

  async function load() {
    try {
      [items, metrics] = await Promise.all([api.memory(), api.memory_metrics()]);
      error = '';
    } catch (e) {
      error = error_text(e);
    }
    loaded = true;
  }
  load();

  const proposed = $derived(items.filter((m) => m.status === 'proposed'));
  const active = $derived(items.filter((m) => m.status === 'active'));
  const pct = (rate: number | null) => (rate === null ? '–' : `${Math.round(rate * 100)}%`);

  /** The rate of a run of weeks, so the trend line compares like with like. */
  function rate_of(weeks: RepeatRate[]): number | null {
    const corrections = weeks.reduce((n, w) => n + w.corrections, 0);
    return corrections ? weeks.reduce((n, w) => n + w.repeats, 0) / corrections : null;
  }
  const trend = $derived.by(() => {
    if (!metrics) return '';
    const before = rate_of(metrics.weeks.slice(0, 4));
    const after = rate_of(metrics.weeks.slice(4));
    if (before === null || after === null) return 'Not enough corrections yet to show a trend.';
    if (Math.round(after * 100) === Math.round(before * 100)) return `Flat at ${pct(after)} over the last 4 weeks.`;
    return `${after < before ? 'Down' : 'Up'} from ${pct(before)} to ${pct(after)} over the last 4 weeks.`;
  });
  /** First-pass gate rate, last 4 weeks against the 4 before: up is good here. */
  const first_pass_trend = $derived.by(() => {
    if (!metrics) return '';
    const rate = (weeks: PassRate[]) => {
      const steps = weeks.reduce((n, w) => n + w.gate_steps, 0);
      return steps ? weeks.reduce((n, w) => n + w.first_pass, 0) / steps : null;
    };
    const before = rate(metrics.first_pass.weeks.slice(0, 4));
    const after = rate(metrics.first_pass.weeks.slice(4));
    if (before === null || after === null) return 'Not enough gate checks yet to show a trend.';
    if (Math.round(after * 100) === Math.round(before * 100)) return `Flat at ${pct(after)} over the last 4 weeks.`;
    return `${after > before ? 'Up' : 'Down'} from ${pct(before)} to ${pct(after)} over the last 4 weeks.`;
  });
  /** Review rejections, last 4 weeks against the 4 before: fewer is better. */
  const rejections_trend = $derived.by(() => {
    if (!metrics) return '';
    const sum = (weeks: RepeatRate[]) => weeks.reduce((n, w) => n + w.corrections, 0);
    const before = sum(metrics.review_rejections.weeks.slice(0, 4));
    const after = sum(metrics.review_rejections.weeks.slice(4));
    if (!before && !after) return 'No rejections in the last 8 weeks.';
    if (before === after) return `Flat at ${after} over the last 4 weeks.`;
    return `${after < before ? 'Down' : 'Up'} from ${before} to ${after} over the last 4 weeks.`;
  });
  const week_text = (w: RepeatRate & { week_start: string }) =>
    `Week of ${day(w.week_start)}: ${w.corrections ? `${pct(w.rate)}, ${w.repeats} of ${w.corrections} repeated` : 'no corrections'}`;

  /** One row of a by-workflow / by-runtime list. */
  type Line = { name: string; value: string; n: string };
  const lines = <T extends { name: string }>(rows: T[], value: (r: T) => string, n: (r: T) => string): Line[] =>
    rows.map((r) => ({ name: r.name, value: value(r), n: n(r) }));
</script>

<!-- A metric's breakdown: by workflow and by runtime, side by side from 640 px. -->
{#snippet split(label: string, groups: [string, Line[]][])}
  <div class="split">
    {#each groups as [title, rows] (title)}
      <div>
        <h3 class="split__head">{title}</h3>
        <ul class="rates" aria-label="{label} {title.toLowerCase()}">
          {#each rows as r (r.name)}
            <li><span class="ml-mono rates__name">{r.name}</span><span class="ml-mono">{r.value}</span><span class="rates__n">{r.n}</span></li>
          {:else}
            <li class="rates__n">None yet</li>
          {/each}
        </ul>
      </div>
    {/each}
  </div>
{/snippet}

<PageHeader title="Memory" subtitle="What agents learned from corrections. Nothing reaches an agent until you approve it." />

<div class="sun-page">
  {#if error}
    <div class="sun-empty" role="alert">{error}</div>
  {:else if !loaded}
    <div class="sun-empty" role="status">Loading…</div>
  {/if}

  {#if metrics}
    <Card as="section" padding="sm" class="metric" aria-labelledby="repeat-head">
      <h2 class="sun-label" id="repeat-head">Repeat mistakes</h2>
      <div class="metric__top">
        <span class="metric__value ml-mono">{pct(metrics.overall.rate)}</span>
        <p class="metric__note">
          of corrections repeated a cause already corrected in the same workflow
          <span class="ml-mono">({metrics.overall.repeats} of {metrics.overall.corrections})</span>.
          <b>{trend}</b>
        </p>
      </div>
      <ol class="weeks" aria-label="Repeat rate by week, oldest first">
        {#each metrics.weeks as w (w.week_start)}
          <li title={week_text(w)}>
            <span class="weeks__bar" class:weeks__bar--empty={!w.corrections} style="height: {Math.max(4, Math.round((w.rate ?? 0) * 100))}%"></span>
            <span class="visually-hidden">{week_text(w)}</span>
          </li>
        {/each}
      </ol>
      <div class="weeks__axis" aria-hidden="true"><span>{day(metrics.weeks[0]?.week_start ?? '')}</span><span>This week</span></div>
      {@render split('Repeat rate', [
        ['By workflow', lines(metrics.by_workflow, (r) => pct(r.rate), (r) => `${r.corrections} corrections`)],
        ['By runtime', lines(metrics.by_runtime, (r) => pct(r.rate), (r) => `${r.corrections} corrections`)],
      ])}
    </Card>

    {@const fp = metrics.first_pass}
    <Card as="section" padding="sm" class="metric" aria-labelledby="first-pass-head">
      <h2 class="sun-label" id="first-pass-head">Gates passed first time</h2>
      <div class="metric__top">
        <span class="metric__value ml-mono">{pct(fp.overall.rate)}</span>
        <p class="metric__note">
          of gate steps passed on their first check
          <span class="ml-mono">({fp.overall.first_pass} of {fp.overall.gate_steps})</span>.
          <b>{first_pass_trend}</b>
        </p>
      </div>
      {@render split('First-pass rate', [
        ['By workflow', lines(fp.by_workflow, (r) => pct(r.rate), (r) => `${r.gate_steps} gate steps`)],
        ['By runtime', lines(fp.by_runtime, (r) => pct(r.rate), (r) => `${r.gate_steps} gate steps`)],
      ])}
    </Card>

    {@const rj = metrics.review_rejections}
    {@const rj_max = Math.max(1, ...rj.weeks.map((w) => w.corrections))}
    <Card as="section" padding="sm" class="metric" aria-labelledby="rejections-head">
      <h2 class="sun-label" id="rejections-head">Review rejections</h2>
      <div class="metric__top">
        <span class="metric__value ml-mono">{rj.overall.corrections}</span>
        <p class="metric__note">
          rejected approvals and change requests on Sun’s PRs.
          <b>{rejections_trend}</b>
        </p>
      </div>
      <ol class="weeks" aria-label="Review rejections by week, oldest first">
        {#each rj.weeks as w (w.week_start)}
          {@const text = `Week of ${day(w.week_start)}: ${w.corrections} ${w.corrections === 1 ? 'rejection' : 'rejections'}`}
          <li title={text}>
            <span class="weeks__bar weeks__bar--rejections" class:weeks__bar--empty={!w.corrections} style="height: {Math.max(4, Math.round((w.corrections / rj_max) * 100))}%"></span>
            <span class="visually-hidden">{text}</span>
          </li>
        {/each}
      </ol>
      <div class="weeks__axis" aria-hidden="true"><span>{day(rj.weeks[0]?.week_start ?? '')}</span><span>This week</span></div>
      {@render split('Review rejections', [
        ['By workflow', lines(rj.by_workflow, (r) => String(r.corrections), (r) => (r.corrections === 1 ? 'rejection' : 'rejections'))],
        ['By runtime', lines(rj.by_runtime, (r) => String(r.corrections), (r) => (r.corrections === 1 ? 'rejection' : 'rejections'))],
      ])}
    </Card>
  {/if}

  {#if loaded && !error}
    <section class="list" aria-labelledby="proposals-head">
      <h2 class="sun-label" id="proposals-head">Proposals <Badge tone={proposed.length ? 'warning' : 'neutral'} count>{proposed.length}</Badge></h2>
      {#each proposed as m (m.id)}
        <MemoryCard item={m} on_change={load} />
      {:else}
        <div class="sun-empty">No proposals. Sun proposes one when the same cause is corrected 3 times in a workflow, or when you star a correction on a task.</div>
      {/each}
    </section>

    <section class="list" aria-labelledby="active-head">
      <h2 class="sun-label" id="active-head">Active <Badge tone="neutral" count>{active.length}</Badge></h2>
      {#each active as m (m.id)}
        <MemoryCard item={m} on_change={load} />
      {:else}
        <div class="sun-empty">Nothing active yet. Approved items go into every matching agent’s context.</div>
      {/each}
    </section>
  {/if}
</div>

<style>
  .sun-page > :global(.metric),
  .list {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
  }
  h2 {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    margin: 0;
  }
  .metric__top {
    display: flex;
    align-items: baseline;
    gap: var(--space-3);
    flex-wrap: wrap;
  }
  .metric__value {
    font-size: 40px;
    line-height: 1;
    font-weight: 700;
    color: var(--fg-1);
  }
  .metric__note {
    flex: 1 1 220px;
    margin: 0;
    font-size: 13px;
    line-height: 1.5;
    color: var(--fg-2);
  }
  .metric__note b {
    display: block;
    color: var(--fg-1);
    font-weight: 600;
  }
  .weeks {
    list-style: none;
    margin: 0;
    padding: 0;
    height: 72px;
    display: grid;
    grid-template-columns: repeat(8, 1fr);
    gap: 6px;
    align-items: end;
  }
  .weeks li {
    height: 100%;
    display: flex;
    align-items: flex-end;
  }
  .weeks__bar {
    width: 100%;
    border-radius: 6px 6px 2px 2px;
    background: var(--ice-400);
  }
  .weeks__bar--rejections {
    background: var(--amber-400);
  }
  .weeks__bar--empty {
    background: none;
    border-top: 2px dashed var(--border-default);
    border-radius: 0;
  }
  .weeks__axis {
    display: flex;
    justify-content: space-between;
    font-size: var(--size-xs);
    color: var(--fg-3);
    margin-top: calc(-1 * var(--space-2));
  }
  .split {
    display: grid;
    grid-template-columns: 1fr;
    gap: var(--space-3);
  }
  @media (min-width: 640px) {
    .split {
      grid-template-columns: 1fr 1fr;
    }
  }
  .split__head {
    margin: 0 0 6px;
    font-size: var(--size-xs);
    font-weight: 600;
    color: var(--fg-2);
  }
  .rates {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 4px;
    font-size: 13px;
    color: var(--fg-1);
  }
  .rates li {
    display: flex;
    gap: var(--space-2);
    align-items: baseline;
    min-width: 0;
  }
  .rates__name {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .rates__n {
    color: var(--fg-3);
    font-size: var(--size-xs);
  }
  .visually-hidden {
    position: absolute;
    width: 1px;
    height: 1px;
    overflow: hidden;
    clip-path: inset(50%);
    white-space: nowrap;
  }
</style>
