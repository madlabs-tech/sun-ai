<!-- Device verify results from the run's `device_result` events: the device and a pass/fail badge,
     a strip of lazy screenshots (each opens full size), the flow output collapsed, and whether the
     git host plugin attached the screenshots to the PR (the outbox event after it). -->
<script lang="ts">
  import { Badge, Button, Icon } from '@hryer/madlabs-design-system/svelte';
  import { screenshot_url } from '../api/client';
  import type { DeviceResultPayload, RunEvent } from '../api/types';

  let { run_id, events }: { run_id: string; events: RunEvent[] } = $props();

  const OUTBOX = ['outbox_delivered', 'outbox_failed', 'outbox_unknown'];
  const ATTACH = {
    outbox_delivered: ['success', 'Attached to the PR'],
    outbox_failed: ['danger', 'Couldn’t attach to the PR'],
    outbox_unknown: ['warning', 'PR attachment unconfirmed'],
    pending: ['neutral', 'Attaching to the PR…'],
  } as const;

  const results = $derived(
    events
      .filter((e) => e.kind === 'device_result')
      .map((e) => {
        const p = e.payload as unknown as DeviceResultPayload;
        const attach = p.screenshots?.length
          ? (events.find((o) => o.seq > e.seq && OUTBOX.includes(o.kind) && String((o.payload as { target?: string }).target ?? '').endsWith('.attach_to_pr'))?.kind ?? 'pending')
          : null;
        return { seq: e.seq, step: e.step_key, p, attach: attach as keyof typeof ATTACH | null };
      }),
  );
  let open = $state<Record<number, boolean>>({});
</script>

{#each results as r (r.seq)}
  {@const [attach_tone, attach_label] = r.attach ? ATTACH[r.attach] : (['neutral', ''] as const)}
  <article class="device-result" aria-label="Device check{r.step ? ` ${r.step}` : ''}">
    <div class="device-result__head">
      <Icon name="smartphone" size={16} />
      <span class="device-result__device">{r.p.device ?? 'No device'}</span>
      {#if r.p.error}
        <Badge tone="warning">Didn’t run</Badge>
      {:else if r.p.passed}
        <Badge tone="success">Passed</Badge>
      {:else}
        <Badge tone="danger">Failed</Badge>
      {/if}
    </div>
    {#if r.p.error}<p class="device-result__error">{r.p.error}</p>{/if}

    {#if r.p.screenshots?.length}
      <ul class="device-result__shots" aria-label="Screenshots">
        {#each r.p.screenshots as s (s.blob)}
          {@const url = screenshot_url(run_id, s.blob)}
          <li>
            <a href={url} target="_blank" rel="noopener" title="Open {s.name} full size">
              <img src={url} alt={s.name} loading="lazy" width="72" height="144" />
            </a>
          </li>
        {/each}
      </ul>
      {#if r.attach}<Badge tone={attach_tone}>{attach_label}</Badge>{/if}
    {/if}

    {#if r.p.output}
      <Button variant="ghost" size="sm" class="sun-disclosure" iconLeft={open[r.seq] ? 'chevron-down' : 'chevron-right'} aria-expanded={!!open[r.seq]} onclick={() => (open[r.seq] = !open[r.seq])}>
        {open[r.seq] ? 'Hide' : 'Show'} output
      </Button>
      {#if open[r.seq]}<pre class="ml-mono event__detail">{r.p.output}</pre>{/if}
    {/if}
  </article>
{/each}

<style>
  .device-result {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    padding: var(--space-3);
    border-radius: var(--radius-md);
    border: 1.5px solid var(--border-subtle);
    background: var(--surface-card);
  }
  .device-result__head {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    min-width: 0;
  }
  .device-result__device {
    flex: 1;
    min-width: 0;
    font-size: 14px;
    color: var(--fg-1);
    overflow-wrap: anywhere;
  }
  .device-result__error {
    margin: 0;
    font-size: 13px;
    color: var(--fg-2);
    overflow-wrap: anywhere;
  }
  .device-result__shots {
    display: flex;
    gap: var(--space-2);
    margin: 0;
    padding: 0 0 var(--space-1);
    list-style: none;
    overflow-x: auto;
    overscroll-behavior-x: contain;
  }
  .device-result__shots a {
    display: block;
    border-radius: var(--radius-sm);
    border: 1.5px solid var(--border-subtle);
    overflow: hidden;
  }
  .device-result__shots a:focus-visible {
    outline: 2px solid var(--ice-400);
    outline-offset: 2px;
  }
  .device-result__shots img {
    display: block;
    width: 72px;
    height: 144px;
    object-fit: cover;
    background: var(--surface-sunken);
  }
</style>
