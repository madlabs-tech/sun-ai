<!-- Canvas pins: each pinned element with its source, a before/after crop, design
     token findings (the repo's tokens, else Madlabs) and two ways forward — Ask AI (steer now, or batch for "Send all") and Change it
     myself (live value in the preview, saved as a diff, then committed to the task branch under your
     name or handed to the AI). The canvas owns picking and the bridge; this panel owns the pins' API
     calls. -->
<script lang="ts">
  import { Badge, Button, Card, Icon, Input, Select, Tabs } from '@hryer/madlabs-design-system/svelte';
  import IconButton from '../lib/ui/IconButton.svelte';
  import { ApiFailure, api, crop_url } from '../lib/api/client';
  import type { Pin, PinStatus, TaskDetail } from '../lib/api/types';
  import { error_text, toast } from '../lib/app_state.svelte';
  import { diff_lines, off_tokens, pin_message, where, type TokenSet } from '../lib/pins';

  let {
    task,
    pins = $bindable(),
    selected = $bindable(),
    apply,
    revert,
    oncommit,
    tokens,
  }: {
    task: TaskDetail;
    pins: Pin[];
    selected: string | null;
    /** Shows a value live in the task-branch previews. */
    apply: (pin: Pin, property: string, value: string) => void;
    /** Undoes every live value. */
    revert: () => void;
    /** A pin's edit was committed to the task branch. */
    oncommit?: () => void;
    /** What pins are checked against. */
    tokens: TokenSet;
  } = $props();

  const STATUS: Record<PinStatus, { label: string; tone: 'neutral' | 'warning' | 'info' | 'success' }> = {
    open: { label: 'Open', tone: 'neutral' },
    queued: { label: 'In batch', tone: 'warning' },
    in_progress: { label: 'With the agent', tone: 'info' },
    done: { label: 'Done', tone: 'success' },
  };
  const WAYS = [
    { id: 'ask', label: 'Ask AI', icon: 'sparkles' },
    { id: 'edit', label: 'Change it myself', icon: 'pencil' },
  ];

  // Per-pin form state, keyed by pin id.
  let way = $state<Record<string, string>>({});
  let comment = $state<Record<string, string>>({});
  let property = $state<Record<string, string>>({});
  let value = $state<Record<string, string>>({});
  let busy = $state(false);

  const number_of = (pin: Pin) => pins.indexOf(pin) + 1;
  const queued = $derived(pins.filter((p) => p.status === 'queued'));
  const run_id = $derived(task.run?.id ?? null);
  // Crops are taken on the host in the background: poll until they land (or give up after a minute).
  const missing_crops = $derived(pins.filter((p) => !p.before_crop || (p.status === 'done' && !p.after_crop)).length);

  $effect(() => {
    if (!missing_crops) return;
    let tries = 0;
    const timer = setInterval(async () => {
      if (++tries > 20) return clearInterval(timer);
      const fresh = await api.list_pins(task.id).catch(() => null);
      if (!fresh) return;
      pins = pins.map((p) => {
        const f = fresh.find((x) => x.id === p.id);
        return f ? { ...p, before_crop: f.before_crop, after_crop: f.after_crop } : p;
      });
    }, 3000);
    return () => clearInterval(timer);
  });

  $effect(() => {
    for (const p of pins) {
      comment[p.id] ??= p.comment;
      property[p.id] ??= p.target.text ? 'text' : 'color';
      value[p.id] ??= current(p, property[p.id]);
    }
  });

  function current(pin: Pin, prop: string) {
    return pin.edits.find((e) => e.property === prop)?.to ?? (prop === 'text' ? pin.target.text : (pin.target.styles[prop] ?? ''));
  }
  const original = (pin: Pin, prop: string) => (prop === 'text' ? pin.target.text : (pin.target.styles[prop] ?? ''));

  function replace(next: Pin) {
    pins = pins.map((p) => (p.id === next.id ? next : p));
  }

  async function update(pin: Pin, change: Parameters<typeof api.update_pin>[2]) {
    const next = await api.update_pin(task.id, pin.id, change);
    replace(next);
    return next;
  }

  async function guarded(work: () => Promise<unknown>) {
    busy = true;
    try {
      await work();
    } catch (e) {
      toast('danger', 'That didn’t go through', error_text(e));
    } finally {
      busy = false;
    }
  }

  /** Steers the agent with these pins in one message. With no agent running, the message is queued
   *  for the run's next agent step; only a run that ended keeps them in the batch. */
  async function send(list: Pin[]) {
    if (!run_id) {
      toast('warning', 'This task has no run yet', 'The pins wait in the batch until it runs.');
      return;
    }
    const text = list.map((p) => pin_message(p, number_of(p), tokens)).join('\n\n');
    let queued_for_next = false;
    try {
      await api.send_message(run_id, text).catch(async () => {
        await api.send_message(run_id, text, true);
        queued_for_next = true;
      });
    } catch (e) {
      for (const p of list) if (p.status !== 'queued') await update(p, { status: 'queued' });
      toast('warning', 'Kept in the batch', `${error_text(e)} Press Send all when the agent runs.`);
      return;
    }
    for (const p of list) await update(p, { status: 'in_progress' });
    const what = list.length > 1 ? `${list.length} pins` : 'Pin';
    if (queued_for_next) toast('success', `${what} queued for the next agent step`, 'No agent is running now. Each pin is marked done when that step finishes.');
    else toast('success', list.length > 1 ? `${list.length} pins sent to the agent` : 'Sent to the agent', 'Each pin is marked done when its change lands.');
  }

  const ask = (pin: Pin, batch: boolean) =>
    guarded(async () => {
      const text = (comment[pin.id] ?? '').trim();
      if (!text) {
        toast('warning', 'Write what should change first');
        return;
      }
      const next = await update(pin, batch ? { comment: text, status: 'queued' } : { comment: text });
      if (batch) toast('success', 'Added to the batch', 'Send all when you’re ready.');
      else await send([next]);
    });

  const save_edit = (pin: Pin) =>
    guarded(async () => {
      const prop = property[pin.id];
      const to = (value[pin.id] ?? '').trim();
      const from = original(pin, prop);
      const edits = pin.edits.filter((e) => e.property !== prop);
      if (to !== from) edits.push({ property: prop, from, to });
      await update(pin, { edits });
      toast('success', 'Edit recorded', 'It’s a suggested diff on the pin.');
    });

  /** Commits the saved edit; an edit that doesn't map to its source line goes to the AI instead. */
  const commit = (pin: Pin) =>
    guarded(async () => {
      try {
        const done = await api.commit_pin(task.id, pin.id);
        replace(done.pin);
        revert();
        oncommit?.();
        toast('success', `Committed ${done.sha.slice(0, 7)} to the task branch`, 'Under your name. The gates run again on it.');
      } catch (e) {
        if (!(e instanceof ApiFailure) || e.status !== 422) throw e;
        toast('warning', 'Sun can’t commit this edit as it is', error_text(e));
      }
    });

  function pick_property(pin: Pin, prop: string) {
    property[pin.id] = prop;
    value[pin.id] = current(pin, prop);
  }

  function live(pin: Pin, v: string) {
    value[pin.id] = v;
    apply(pin, property[pin.id], v);
  }

  function undo(pin: Pin) {
    revert();
    value[pin.id] = original(pin, property[pin.id]);
  }

  const remove = (pin: Pin) =>
    guarded(async () => {
      await api.delete_pin(task.id, pin.id);
      pins = pins.filter((p) => p.id !== pin.id);
    });

  const property_options = (pin: Pin) => [
    ...(pin.target.text ? [{ value: 'text', label: 'Text' }] : []),
    ...Object.keys(pin.target.styles).map((k) => ({ value: k, label: k })),
  ];
</script>

<section class="pins" aria-labelledby="pins-title">
  <header class="pins__head">
    <h2 id="pins-title" class="sun-label">Pins</h2>
    {#if pins.length}<Badge count>{pins.length}</Badge>{/if}
    <span class="pins__spacer"></span>
    {#if queued.length}
      <Button size="sm" variant="ice" iconLeft="send" loading={busy} onclick={() => guarded(() => send(queued))}>Send all ({queued.length})</Button>
    {/if}
  </header>

  {#if !pins.length}
    <p class="sun-empty pins__empty">No pins yet. Press <strong>Pin an element</strong>, then click a button or block in an After frame.</p>
  {/if}

  {#each pins as pin, i (pin.id)}
    {@const st = STATUS[pin.status]}
    {@const off = off_tokens(pin.target.styles, tokens)}
    {@const source = where(pin)}
    <Card as="article" variant={selected === pin.id ? 'tint' : 'outline'} padding="sm" class="pin" id="pin-{pin.id}" aria-labelledby="pin-{pin.id}-title">
      <div class="pin__head">
        <span class="pin__n" aria-hidden="true">{i + 1}</span>
        <h3 id="pin-{pin.id}-title" class="pin__title">
          <span class="visually-hidden">Pin {i + 1}:</span>
          <span class="ml-mono">&lt;{pin.target.tag}&gt;</span>
          {#if pin.target.text}<span class="pin__text">“{pin.target.text}”</span>{/if}
        </h3>
        <Badge tone={st.tone} dot live={pin.status === 'in_progress'}>{st.label}</Badge>
        <IconButton icon="trash-2" label="Remove pin {i + 1}" variant="ghost" size="sm" disabled={busy} onclick={() => remove(pin)} />
      </div>
      <dl class="pin__meta">
        <dt>Selector</dt><dd class="ml-mono" title={pin.target.selector}>{pin.target.selector}</dd>
        <dt>Source</dt>
        <dd class={source ? 'ml-mono' : 'pin__wrap'}>{source ?? 'Not reported — the app has no dev source metadata'}</dd>
        <dt>Width</dt><dd class="ml-mono">{pin.target.width} px</dd>
      </dl>

      {#if pin.before_crop}
        <div class="pin__crops" role="group" aria-label="Pin {i + 1} before and after">
          <figure class="pin__crop">
            <img src={crop_url(task.id, pin.id, 'before', pin.before_crop)} alt="Pin {i + 1} when it was pinned" />
            <figcaption>Before</figcaption>
          </figure>
          {#if pin.status === 'done'}
            <figure class="pin__crop">
              {#if pin.after_crop}
                <img src={crop_url(task.id, pin.id, 'after', pin.after_crop)} alt="Pin {i + 1} after the change" />
              {:else}
                <div class="pin__crop-wait" role="status" aria-busy="true">Taking the screenshot…</div>
              {/if}
              <figcaption>After</figcaption>
            </figure>
          {/if}
        </div>
      {/if}

      {#if off.length}
        <div class="pin__tokens" role="note">
          <Icon name="triangle-alert" size={16} />
          <div>
            <p><strong>{off.length === 1 ? '1 value isn’t a design token' : `${off.length} values aren’t design tokens`}</strong></p>
            <p class="pin__source">Checked against <span class="ml-mono">{tokens.source}</span></p>
            <ul>
              {#each off as o (o.property)}
                <li><code>{o.property}: {o.value}</code> <span>→ use <code>{o.suggestion}</code></span></li>
              {/each}
            </ul>
          </div>
        </div>
      {/if}

      {#if pin.status === 'in_progress'}
        <div class="pin__row">
          <p class="pin__muted">Sent. It’s marked done when the agent’s change lands.</p>
          <Button size="sm" variant="secondary" iconLeft="check" disabled={busy} onclick={() => guarded(() => update(pin, { status: 'done' }))}>Mark done</Button>
        </div>
      {:else if pin.status !== 'done'}
        <Tabs items={WAYS} value={way[pin.id] ?? 'ask'} onchange={(id) => (way[pin.id] = id)} />
        {#if (way[pin.id] ?? 'ask') === 'ask'}
          <form class="pin__form" onsubmit={(e) => { e.preventDefault(); ask(pin, (e.submitter as HTMLButtonElement | null)?.value === 'batch'); }}>
            <label class="ml-field">
              <span class="ml-field__label">What should change?</span>
              <span class="ml-control pin__textarea">
                <textarea rows="3" maxlength="4000" placeholder="e.g. Make this the primary button and tighten the padding" bind:value={comment[pin.id]}></textarea>
              </span>
            </label>
            <div class="pin__row">
              <Button size="sm" variant="ice" iconLeft="send" type="submit" value="now" loading={busy}>Send now</Button>
              <Button size="sm" variant="secondary" iconLeft="list-end" type="submit" value="batch" disabled={busy}>Add to batch</Button>
            </div>
          </form>
        {:else}
          <div class="pin__form">
            <div class="pin__edit">
              <Select label="Property" options={property_options(pin)} value={property[pin.id]} onchange={(e: Event) => pick_property(pin, (e.currentTarget as HTMLSelectElement).value)} />
              <Input label="New value" mono value={value[pin.id] ?? ''} oninput={(e: Event) => live(pin, (e.currentTarget as HTMLInputElement).value)} hint="Shows live in the After frames." />
            </div>
            <div class="pin__row">
              <Button size="sm" variant="secondary" iconLeft="save" disabled={busy} onclick={() => save_edit(pin)}>Save edit</Button>
              <Button size="sm" variant="ghost" iconLeft="rotate-ccw" onclick={() => undo(pin)}>Undo live change</Button>
            </div>
          </div>
        {/if}
      {/if}

      {#if pin.edits.length}
        <figure class="pin__diff">
          <figcaption>Suggested diff{source ? ` · ${source}` : ''}</figcaption>
          <pre aria-label="Suggested diff">{#each diff_lines(pin) as line, j (j)}<span class={line.startsWith('+') ? 'add' : 'del'}>{line}</span>
{/each}</pre>
          {#if pin.status === 'open' || pin.status === 'queued'}
            {#if !source}<p class="pin__muted">No source line, so Sun can’t commit this itself. Hand it to the AI.</p>{/if}
            <div class="pin__row">
              {#if source}
                <Button size="sm" variant="ice" iconLeft="git-branch" loading={busy} onclick={() => commit(pin)}>Commit to branch</Button>
              {/if}
              <Button size="sm" variant="secondary" iconLeft="sparkles" disabled={busy} onclick={() => guarded(() => send([pin]))}>Hand to AI</Button>
            </div>
          {/if}
        </figure>
      {/if}
    </Card>
  {/each}
</section>

<style>
  .pins {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
    min-width: 0;
  }
  .pins__head {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    min-height: var(--control-sm);
  }
  .pins__head h2 {
    margin: 0;
  }
  .pins__spacer {
    flex: 1;
  }
  .pins__empty {
    margin: 0;
  }
  :global(.pin) {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
    scroll-margin: var(--space-4);
  }
  .pin__head {
    display: flex;
    align-items: center;
    gap: var(--space-2);
  }
  .pin__n {
    flex: none;
    display: grid;
    place-items: center;
    width: 24px;
    height: 24px;
    border-radius: var(--radius-pill);
    background: var(--accent);
    color: var(--on-cobalt);
    font-size: var(--size-xs);
    font-weight: var(--weight-bold);
  }
  .pin__title {
    flex: 1;
    min-width: 0;
    margin: 0;
    font-size: var(--size-sm);
    font-weight: var(--weight-semibold);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .pin__text {
    color: var(--fg-2);
    font-weight: var(--weight-regular);
  }
  .pin__meta {
    display: grid;
    grid-template-columns: auto minmax(0, 1fr);
    gap: var(--space-1) var(--space-3);
    margin: 0;
    font-size: var(--size-xs);
  }
  .pin__meta dt {
    color: var(--fg-3);
  }
  .pin__meta dd {
    margin: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .pin__meta dd.pin__wrap {
    white-space: normal;
    color: var(--fg-2);
  }
  .pin__muted {
    margin: 0;
    color: var(--fg-2);
    font-size: var(--size-xs);
  }
  .pin__tokens {
    display: flex;
    gap: var(--space-2);
    padding: var(--space-2) var(--space-3);
    border-radius: var(--radius-sm);
    background: var(--warning-soft);
    color: var(--warning-fg);
    font-size: var(--size-xs);
  }
  .pin__tokens p,
  .pin__tokens ul {
    margin: 0;
  }
  .pin__tokens ul {
    padding: 0;
    list-style: none;
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
    margin-top: var(--space-1);
    color: var(--fg-1);
  }
  .pin__tokens li {
    display: flex;
    flex-wrap: wrap;
    column-gap: var(--space-2);
  }
  .pin__tokens > div {
    min-width: 0;
  }
  .pin__tokens code {
    font-family: var(--font-mono);
    overflow-wrap: anywhere;
  }
  .pin__tokens li > span,
  .pin__tokens .pin__source {
    color: var(--fg-2);
  }
  .pin__source {
    overflow-wrap: anywhere;
  }
  .pin__form {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
  }
  .pin__textarea {
    height: auto;
    padding: var(--space-2) var(--space-3);
  }
  .pin__textarea textarea {
    width: 100%;
    border: 0;
    outline: 0;
    resize: vertical;
    background: transparent;
    color: inherit;
    font: inherit;
    font-size: var(--size-body);
  }
  .pin__edit {
    display: grid;
    gap: var(--space-3);
  }
  .pin__row {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: var(--space-2);
  }
  .pin__diff {
    margin: 0;
    display: flex;
    flex-direction: column;
    align-items: flex-start;
    gap: var(--space-2);
  }
  .pin__diff figcaption {
    font-size: var(--size-xs);
    color: var(--fg-2);
  }
  .pin__diff pre {
    align-self: stretch;
    margin: 0;
    padding: var(--space-2) var(--space-3);
    border-radius: var(--radius-sm);
    background: var(--surface-sunken);
    font-family: var(--font-mono);
    font-size: var(--size-xs);
    overflow-x: auto;
  }
  .pin__diff .add {
    color: var(--success-fg);
  }
  .pin__diff .del {
    color: var(--danger-fg);
  }
  .pin__crops {
    display: grid;
    grid-template-columns: repeat(2, minmax(0, 1fr));
    gap: var(--space-2);
  }
  .pin__crop {
    margin: 0;
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
    min-width: 0;
  }
  .pin__crop img,
  .pin__crop-wait {
    display: block;
    /* Crops are real CSS pixels: shrink to fit, never upscale. */
    width: auto;
    height: auto;
    max-width: 100%;
    max-height: 160px;
    border: 1px solid var(--border);
    border-radius: var(--radius-sm);
    background: var(--surface-sunken);
  }
  .pin__crop-wait {
    display: grid;
    place-items: center;
    min-height: 48px;
    padding: var(--space-2);
    color: var(--fg-2);
    font-size: var(--size-xs);
  }
  .pin__crop figcaption {
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
