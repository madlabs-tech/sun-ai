<!-- Design canvas: the task's running app at phone, tablet and desktop widths,
     the task branch (after) against the base branch (before). The screens are the page paths the
     repo lists in `.sun-preview`; several sit side by side, one row per screen. Previews start
     on open, and the after side reloads when the agent changes files. Pins are checked against the
     repo's design tokens. Pins: "Pin an element" asks the preview
     bridge (web/public/preview_bridge.js) to report the next clicked element; pins show as numbered
     markers on the After frames and in the pins panel. -->
<script lang="ts">
  import { Badge, Button, Icon, Tabs } from '@hryer/madlabs-design-system/svelte';
  import IconButton from '../lib/ui/IconButton.svelte';
  import PageHeader from '../lib/ui/PageHeader.svelte';
  import PreviewFrame from '../lib/ui/PreviewFrame.svelte';
  import CanvasPins from './CanvasPins.svelte';
  import { api, watch_events } from '../lib/api/client';
  import { run_repos, type Design, type Pin, type PreviewSide, type TaskDetail } from '../lib/api/types';
  import { act, error_text, toast } from '../lib/app_state.svelte';
  import { parse_bridge, repo_tokens } from '../lib/pins';
  import { href } from '../lib/router.svelte';

  let { id }: { id: string } = $props();

  const BREAKPOINTS = [
    { width: 360, height: 740, label: 'Phone', icon: 'smartphone' },
    { width: 768, height: 1024, label: 'Tablet', icon: 'tablet' },
    { width: 1280, height: 800, label: 'Desktop', icon: 'monitor' },
  ];
  const SIDE_LABEL: Record<PreviewSide, string> = { before: 'Before · base branch', after: 'After · task branch' };
  const MODES = [
    { id: 'after', label: 'After' },
    { id: 'before', label: 'Before' },
    { id: 'compare', label: 'Side by side', icon: 'columns-2' },
  ];

  type SideState = { status: 'idle' | 'starting' | 'ready' | 'failed' | 'stopped'; url: string | null; error: string };
  const idle = (): SideState => ({ status: 'idle', url: null, error: '' });

  const HOME: Design['screens'] = [{ name: '/', path: '/' }];

  let task = $state<TaskDetail | null>(null);
  let design = $state<Design | null>(null);
  /** Page paths shown, in the repo's order; the first screen to start with. */
  let shown_paths = $state<string[]>(['/']);
  let load_error = $state('');
  let sides = $state<Record<PreviewSide, SideState>>({ before: idle(), after: idle() });
  let mode = $state('after');
  // Mobile-first: one phone frame; from the sidebar breakpoint, every width.
  let widths = $state(matchMedia('(min-width: 1024px)').matches ? [360, 768, 1280] : [360]);
  let reload = $state<Record<PreviewSide, number>>({ before: 0, after: 0 });
  let live = $state(false);
  let stage_width = $state(0);
  let pins = $state<Pin[]>([]);
  let selected = $state<string | null>(null);
  let picking = $state(false);
  let attaching = $state(false);
  /** Live iframes by `<side>-<width>-<path>`, and each After frame's scroll offset (from the bridge). */
  const frames = new Map<string, HTMLIFrameElement>();
  let scrolls = $state<Record<string, { x: number; y: number }>>({});

  const shown_sides = $derived<PreviewSide[]>(mode === 'compare' ? ['before', 'after'] : [mode as PreviewSide]);
  const shown = $derived(BREAKPOINTS.filter((b) => widths.includes(b.width)));
  const has_repo = $derived(!!task && run_repos(task).length > 0);
  const screens = $derived(design?.screens.length ? design.screens : HOME);
  const shown_screens = $derived(screens.filter((s) => shown_paths.includes(s.path)));
  const tokens = $derived(repo_tokens(design?.files ?? []));
  const frame_key = (side: PreviewSide, width: number, path: string) => `${side}-${width}-${path}`;
  /** A side's URL for a page path (`url` is the preview base, ending in `/`). */
  const page_url = (url: string | null, path: string) => (url ? url + path.slice(1) : null);

  $effect(() => {
    task = null;
    load_error = '';
    sides = { before: idle(), after: idle() };
    pins = [];
    design = null;
    shown_paths = ['/'];
    api.get_task(id).then(
      (t) => {
        task = t;
        // No design (no repo, host offline): the root page and Madlabs tokens.
        if (run_repos(t).length)
          api.get_design(t.id).then(
            (d) => {
              design = d;
              shown_paths = d.screens.slice(0, 1).map((s) => s.path);
            },
            () => {},
          );
        api.list_pins(t.id).then((p) => (pins = p), (e) => toast('danger', 'Pins didn’t load', error_text(e)));
      },
      (e) => (load_error = error_text(e)),
    );
  });

  /** Posts to every live After frame (of one page path, if given). Their origin is opaque ("null"), so the target can't be narrowed. */
  function tell_after(message: unknown, path?: string) {
    for (const [key, frame] of frames)
      if (key.startsWith('after-') && (!path || key.endsWith(`-${path}`)) && frame.isConnected) frame.contentWindow?.postMessage(message, '*');
  }

  function set_picking(on: boolean) {
    picking = on;
    tell_after({ sun: 'pick', on });
  }

  // Bridge messages: only from one of our iframes (event.source), opaque origin, documented shape.
  $effect(() => {
    const on_message = async (e: MessageEvent) => {
      const key = [...frames].find(([, f]) => f.contentWindow === e.source)?.[0];
      const message = key && e.origin === 'null' ? parse_bridge(e.data) : null;
      if (!key || !message || !task) return;
      if (message.sun === 'ready') {
        scrolls[key] = { x: 0, y: 0 };
        if (picking && key.startsWith('after-')) frames.get(key)?.contentWindow?.postMessage({ sun: 'pick', on: true }, '*');
      } else if (message.sun === 'scroll') scrolls[key] = { x: message.x, y: message.y };
      else if (message.sun === 'pick_cancelled') set_picking(false);
      else if (message.sun === 'picked' && key.startsWith('after-')) {
        set_picking(false);
        try {
          const pin = await api.create_pin(task.id, message.target);
          pins = [...pins, pin];
          selected = pin.id;
          requestAnimationFrame(() => {
            const card = document.getElementById(`pin-${pin.id}`);
            card?.scrollIntoView({ block: 'nearest', behavior: 'smooth' });
            card?.querySelector('textarea')?.focus({ preventScroll: true });
          });
        } catch (err) {
          toast('danger', 'That pin didn’t save', error_text(err));
        }
      }
    };
    const on_key = (e: KeyboardEvent) => e.key === 'Escape' && picking && set_picking(false);
    addEventListener('message', on_message);
    addEventListener('keydown', on_key);
    return () => {
      removeEventListener('message', on_message);
      removeEventListener('keydown', on_key);
    };
  });

  /** "Change it myself": the value goes live in every After frame of the pin's page. */
  function apply(pin: Pin, property: string, value: string) {
    tell_after({ sun: 'apply', selector: pin.target.selector, property, value }, pin.target.path || '/');
  }

  /** The agent's change landed: pins it was working on are done. */
  async function landed() {
    const working = pins.filter((p) => p.status === 'in_progress');
    if (!task || !working.length) return;
    const t = task;
    const done = await Promise.allSettled(working.map((p) => api.update_pin(t.id, p.id, { status: 'done' })));
    const ok = done.flatMap((r) => (r.status === 'fulfilled' ? [r.value] : []));
    pins = pins.map((p) => ok.find((d) => d.id === p.id) ?? p);
    if (ok.length) toast('success', `The agent’s change landed · ${ok.length} ${ok.length === 1 ? 'pin' : 'pins'} done`);
  }

  async function start(side: PreviewSide) {
    if (!task) return;
    sides[side] = { status: 'starting', url: null, error: '' };
    try {
      const preview = await api.start_preview(task.id, side);
      sides[side] = { status: 'ready', url: preview.url, error: '' };
    } catch (e) {
      sides[side] = { status: 'failed', url: null, error: error_text(e) };
    }
  }

  // Start each side the first time it is shown (idempotent on the kernel).
  $effect(() => {
    if (!has_repo) return;
    for (const side of shown_sides) if (sides[side].status === 'idle') start(side);
  });

  // Reload the after side when the agent's changes land (debounced; history before opening is skipped).
  $effect(() => {
    const run_id = task?.run?.id;
    if (!run_id || !has_repo) return;
    const opened = Date.now();
    let timer: ReturnType<typeof setTimeout>;
    live = true;
    // ponytail: compares the kernel's event time with this browser's clock; use a seq cursor if clocks drift.
    const stop = watch_events(
      run_id,
      0,
      (e) => {
        if (!e.kind.startsWith('file_change') || Date.parse(e.at) < opened) return;
        clearTimeout(timer);
        timer = setTimeout(() => {
          reload.after++;
          landed();
        }, 1500);
      },
      () => (live = false),
    );
    return () => {
      clearTimeout(timer);
      stop();
      live = false;
    };
  });

  function toggle(width: number) {
    // At least one width stays on.
    if (widths.includes(width)) widths = widths.length > 1 ? widths.filter((w) => w !== width) : widths;
    else widths = [...widths, width].sort((a, b) => a - b);
  }

  function toggle_screen(path: string) {
    // At least one screen stays on; shown in the repo's order.
    if (shown_paths.includes(path)) shown_paths = shown_paths.length > 1 ? shown_paths.filter((p) => p !== path) : shown_paths;
    else shown_paths = screens.map((s) => s.path).filter((p) => p === path || shown_paths.includes(p));
  }

  function reload_all() {
    reload.before++;
    reload.after++;
  }

  async function stop_servers() {
    if (!task) return;
    const t = task;
    if (await act(() => api.stop_previews(t.id), 'Preview servers stopped')) {
      const stopped = (): SideState => ({ status: 'stopped', url: null, error: '' });
      sides = { before: stopped(), after: stopped() };
    }
  }

  /** Review evidence: before/after screenshots of the shown screens and widths on the task and its PR. */
  async function attach() {
    if (!task) return;
    const t = task;
    attaching = true;
    try {
      const shots = await api.attach_screens(t.id, { paths: shown_paths, widths });
      toast('success', `${shots.length} screenshots attached`, 'Before and after, on the task’s timeline and its PR.');
    } catch (e) {
      toast('danger', 'The screenshots weren’t attached', error_text(e));
    } finally {
      attaching = false;
    }
  }

  /** Each frame fits its share of the stage; never upscaled. */
  function scale(width: number) {
    const gap = 16;
    const share = (stage_width - gap * (shown_sides.length - 1)) / shown_sides.length;
    return stage_width ? Math.min(1, share / width) : 1;
  }
</script>

<PageHeader title="Canvas" subtitle={task ? `${task.key} · ${task.title}` : 'Design canvas'}>
  <Button size="sm" variant="ghost" iconLeft="arrow-left" href={href('tasks', task?.key ?? id)}>Task</Button>
  {#if has_repo}
    <Button
      size="sm"
      variant={picking ? 'ice' : 'secondary'}
      iconLeft="mouse-pointer-click"
      aria-pressed={picking}
      disabled={sides.after.status !== 'ready'}
      onclick={() => set_picking(!picking)}>Pin an element</Button>
    <Button size="sm" variant="secondary" iconLeft="upload" loading={attaching} disabled={!task?.run} onclick={attach}>Attach to review</Button>
    <IconButton icon="refresh-cw" label="Reload previews" variant="secondary" size="sm" onclick={reload_all} />
    <Button size="sm" variant="ghost" iconLeft="square-x" onclick={stop_servers}>Stop servers</Button>
  {/if}
</PageHeader>

<div class="canvas">
  {#if load_error}
    <div class="sun-empty" role="alert">{load_error}</div>
  {:else if !task}
    <div class="sun-empty" role="status">Loading…</div>
  {:else if !has_repo}
    <div class="sun-empty">This task has no repo, so there’s no app to preview.</div>
  {:else}
    <div class="canvas__bar" role="toolbar" aria-label="Canvas view">
      <Tabs items={MODES} bind:value={mode} />
      <div class="canvas__widths" role="group" aria-label="Breakpoints">
        {#each BREAKPOINTS as b (b.width)}
          {@const on = widths.includes(b.width)}
          <Button
            size="sm"
            variant={on ? 'ice' : 'secondary'}
            iconLeft={b.icon}
            aria-pressed={on}
            aria-label="{b.label} {b.width} px"
            onclick={() => toggle(b.width)}><span class="ml-mono">{b.width}</span></Button>
        {/each}
      </div>
      {#if screens.length > 1}
        <div class="canvas__screens" role="group" aria-label="Screens">
          {#each screens as screen (screen.path)}
            {@const on = shown_paths.includes(screen.path)}
            <Button
              size="sm"
              variant={on ? 'ice' : 'secondary'}
              iconLeft="file"
              aria-pressed={on}
              title={screen.path}
              onclick={() => toggle_screen(screen.path)}>{screen.name}</Button>
          {/each}
        </div>
      {/if}
      {#if live}<Badge tone="info" dot live>Live</Badge>{/if}
    </div>
    {#if picking}
      <p class="canvas__hint" role="status"><Icon name="mouse-pointer-click" size={16} /> Click an element in an After frame to pin it. Esc cancels.</p>
    {/if}

    <div class="canvas__main">
    <div class="canvas__stage" bind:clientWidth={stage_width}>
      {#each shown_screens as screen, n (screen.path)}
        <section class="canvas__screen" aria-labelledby={screens.length > 1 ? `canvas-screen-${n}` : undefined}>
          {#if screens.length > 1}
            <h2 id="canvas-screen-{n}" class="canvas__screen-title">
              {screen.name} {#if screen.name !== screen.path}<span class="ml-mono">{screen.path}</span>{/if}
            </h2>
          {/if}
          <div class="canvas__row">
            {#each shown as b (b.width)}
              {@const s = scale(b.width)}
              <section class="canvas__group" aria-label="{screen.name} · {b.label} · {b.width} px">
                {#each shown_sides as side (side)}
                  {@const st = sides[side]}
                  {@const key = frame_key(side, b.width, screen.path)}
                  <figure class="canvas__frame">
                    <figcaption>
                      <Badge tone={side === 'after' ? 'primary' : 'neutral'}>{side === 'after' ? 'After' : 'Before'}</Badge>
                      <span class="ml-mono">{b.width} px</span>
                    </figcaption>
                    <PreviewFrame
                      url={page_url(st.url, screen.path)}
                      title="{SIDE_LABEL[side]} · {screen.name} at {b.width} px"
                      width={b.width}
                      height={b.height}
                      scale={s}
                      reload={reload[side]}
                      onframe={(f) => frames.set(key, f)}
                    >
                      {#snippet overlay()}
                        {#if side === 'after'}
                          {@const scroll = scrolls[key] ?? { x: 0, y: 0 }}
                          {#each pins as pin, i (pin.id)}
                            {#if pin.target.width === b.width && (pin.target.path || '/') === screen.path}
                              <button
                                type="button"
                                class="canvas__marker"
                                class:canvas__marker--done={pin.status === 'done'}
                                class:canvas__marker--on={selected === pin.id}
                                style="left:{pin.target.rect.x - scroll.x}px;top:{pin.target.rect.y - scroll.y}px;--marker-scale:{1 / s}"
                                aria-label="Pin {i + 1}: {pin.target.tag}{pin.target.text ? ` “${pin.target.text}”` : ''}"
                                onclick={() => {
                                  selected = pin.id;
                                  document.getElementById(`pin-${pin.id}`)?.scrollIntoView({ block: 'nearest', behavior: 'smooth' });
                                }}>{i + 1}</button>
                            {/if}
                          {/each}
                        {/if}
                      {/snippet}
                      {#if st.status === 'failed'}
                        <div class="canvas__note" role="alert">
                          <Icon name="triangle-alert" size={20} />
                          <p>{st.error}</p>
                          <Button size="sm" variant="secondary" iconLeft="rotate-ccw" onclick={() => start(side)}>Try again</Button>
                        </div>
                      {:else if st.status === 'stopped'}
                        <div class="canvas__note">
                          <p>The dev server is stopped.</p>
                          <Button size="sm" variant="secondary" iconLeft="play" onclick={() => start(side)}>Start</Button>
                        </div>
                      {:else}
                        <div class="canvas__note" role="status">
                          <span class="sun-pulse"><Icon name="loader" size={20} /></span>
                          <p>Starting the {side === 'after' ? 'task branch' : 'base branch'} dev server… The first start installs and builds, so it can take a few minutes.</p>
                        </div>
                      {/if}
                    </PreviewFrame>
                  </figure>
                {/each}
              </section>
            {/each}
          </div>
        </section>
      {/each}
    </div>
    <CanvasPins {task} {tokens} bind:pins bind:selected {apply} revert={() => tell_after({ sun: 'revert' })} oncommit={() => reload.after++} />
    </div>
  {/if}
</div>

<style>
  .canvas {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
    padding: var(--space-4);
    display: flex;
    flex-direction: column;
    gap: var(--space-4);
  }
  .canvas__bar {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: var(--space-3);
  }
  .canvas__widths,
  .canvas__screens {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-2);
  }
  .canvas__hint {
    margin: 0;
    display: flex;
    align-items: center;
    gap: var(--space-2);
    font-size: var(--size-sm);
    color: var(--info-fg);
  }
  .canvas__main {
    display: grid;
    grid-template-columns: minmax(0, 1fr);
    gap: var(--space-6);
    align-items: start;
  }
  @media (min-width: 1024px) {
    .canvas__main {
      grid-template-columns: minmax(0, 1fr) 340px;
    }
  }
  /* Numbered pin markers sit in page pixels; counter-scaled so they stay readable at any frame scale. */
  .canvas__marker {
    position: absolute;
    width: 24px;
    height: 24px;
    margin: -12px 0 0 -12px;
    padding: 0;
    border: var(--border-thick) solid var(--slate-0);
    border-radius: var(--radius-pill);
    background: var(--accent);
    color: var(--on-cobalt);
    font: var(--weight-bold) var(--size-xs) / 1 var(--font-sans);
    box-shadow: var(--shadow-sm);
    cursor: pointer;
    transform: scale(var(--marker-scale));
  }
  .canvas__marker:focus-visible {
    outline: none;
    box-shadow: var(--ring-focus);
  }
  .canvas__marker--on {
    background: var(--ice-400);
    color: var(--on-ice);
  }
  .canvas__marker--done {
    opacity: 0.55;
  }
  .canvas__stage {
    display: flex;
    flex-direction: column;
    gap: var(--space-8);
    min-width: 0;
  }
  .canvas__screen {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
  }
  .canvas__screen-title {
    margin: 0;
    display: flex;
    flex-wrap: wrap;
    align-items: baseline;
    gap: var(--space-2);
    font-size: var(--size-body);
    font-weight: var(--weight-semibold);
  }
  .canvas__screen-title .ml-mono {
    font-size: var(--size-xs);
    font-weight: var(--weight-regular);
    color: var(--fg-2);
    overflow-wrap: anywhere;
  }
  .canvas__row {
    display: flex;
    flex-wrap: wrap;
    align-items: flex-start;
    gap: var(--space-6) var(--space-4);
  }
  .canvas__group {
    display: flex;
    gap: var(--space-4);
    max-width: 100%;
  }
  .canvas__frame {
    margin: 0;
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    min-width: 0;
  }
  figcaption {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    font-size: var(--size-xs);
    color: var(--fg-2);
  }
  .canvas__note {
    height: 100%;
    box-sizing: border-box;
    padding: var(--space-4);
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: var(--space-3);
    text-align: center;
    font-size: var(--size-xs);
    color: var(--fg-2);
    overflow-wrap: anywhere;
  }
  .canvas__note p {
    margin: 0;
    max-width: 32ch;
  }
</style>
