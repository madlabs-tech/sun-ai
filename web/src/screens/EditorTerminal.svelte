<!-- The editor's bottom terminal panel: tabs of shells in the caller's editor worktree (lib/terminal.ts), at most
     five (the host's cap per member, parked ones included; it answers with its own words past that). Hidden, the
     shells keep running. Drag the top edge (or focus it and press the up and down arrows) to resize. A tab whose shell ended keeps
     its screen and says why. Closing the last tab hides the panel. -->
<script lang="ts">
  import { Button } from '@hryer/madlabs-design-system/svelte';
  import IconButton from '../lib/ui/IconButton.svelte';
  import type { EditorAt } from '../lib/api/client';
  import { open_terminal, type TerminalState } from '../lib/terminal';

  let { at, ready, app_key }: { at: EditorAt; ready: Promise<void>; app_key: (e: KeyboardEvent) => boolean } = $props();

  const MAX_TABS = 5;
  interface Tab {
    id: number;
    state: TerminalState;
  }

  let shown = $state(false);
  let tabs = $state<Tab[]>([]);
  let active = $state(0);
  let seq = 0;
  let height = $state(280);
  const sessions = new Map<number, ReturnType<typeof open_terminal>>();
  const ended = (s: TerminalState) => !['connecting', 'live', 'reconnecting'].includes(s.kind);
  const running = $derived(tabs.filter((t) => !ended(t.state)).length);

  export function toggle() {
    shown = !shown;
    if (shown && !tabs.length) add();
  }

  function add() {
    tabs.push({ id: ++seq, state: { kind: 'connecting' } });
    active = seq;
  }

  function close(tab: Tab) {
    const i = tabs.findIndex((t) => t.id === tab.id);
    tabs.splice(i, 1);
    if (!tabs.length) shown = false;
    else if (tab.id === active) active = tabs[Math.min(i, tabs.length - 1)].id;
  }

  function start(tab: Tab) {
    return (element: HTMLElement) => {
      let session: ReturnType<typeof open_terminal> | undefined;
      let gone = false;
      ready.then(() => {
        if (gone) return;
        session = open_terminal(element, at, (s) => (tab.state = s), app_key);
        sessions.set(tab.id, session);
        if (tab.id === active) session.focus();
      });
      return () => {
        gone = true;
        sessions.delete(tab.id);
        session?.close();
      };
    };
  }

  $effect(() => {
    if (shown) sessions.get(active)?.focus();
  });

  function status(s: TerminalState): string {
    switch (s.kind) {
      case 'connecting':
        return 'Starting a shell…';
      case 'reconnecting':
        return 'Connection lost. Reconnecting…';
      case 'exit':
        return `Session ended (exit ${s.code})`;
      case 'dead':
        return 'Host disconnected. The shell is gone.';
      case 'lost':
        return 'The connection didn’t come back within 30 s, so the shell was closed.';
      case 'revoked':
        return 'Your terminal access was revoked.';
      case 'error':
        return s.message;
      case 'live':
        return '';
    }
  }

  function drag(e: PointerEvent) {
    const handle = e.currentTarget as HTMLElement;
    const [from, start_height] = [e.clientY, height];
    handle.setPointerCapture(e.pointerId);
    const move = (m: PointerEvent) => resize(start_height + from - m.clientY);
    handle.addEventListener('pointermove', move);
    handle.addEventListener('pointerup', () => handle.removeEventListener('pointermove', move), { once: true });
  }
  const resize = (to: number) => (height = Math.round(Math.min(Math.max(to, 120), innerHeight * 0.8)));
  function nudge(e: KeyboardEvent) {
    if (e.key !== 'ArrowUp' && e.key !== 'ArrowDown') return;
    e.preventDefault();
    resize(height + (e.key === 'ArrowUp' ? 24 : -24));
  }
</script>

<section class="panel" hidden={!shown} aria-label="Terminal" style:height="{height}px">
  <!-- svelte-ignore a11y_no_noninteractive_tabindex, a11y_no_noninteractive_element_interactions: a focusable separator is ARIA's window splitter -->
  <div
    class="panel__edge"
    role="separator"
    aria-orientation="horizontal"
    aria-label="Resize the terminal"
    aria-valuenow={height}
    tabindex="0"
    onpointerdown={drag}
    onkeydown={nudge}
  ></div>
  <div class="panel__bar">
    <div class="panel__tabs" role="tablist" aria-label="Terminals">
      {#each tabs as tab, i (tab.id)}
        <div class="panel__tab" class:panel__tab--active={tab.id === active} class:panel__tab--ended={ended(tab.state)}>
          <button type="button" role="tab" aria-selected={tab.id === active} onclick={() => (active = tab.id)}>Terminal {i + 1}</button>
          <IconButton icon="x" label="Close terminal {i + 1}" variant="ghost" size="sm" onclick={() => close(tab)} />
        </div>
      {/each}
    </div>
    <IconButton
      icon="plus"
      label={running >= MAX_TABS ? `At most ${MAX_TABS} terminals per host` : 'New terminal'}
      variant="ghost"
      size="sm"
      disabled={running >= MAX_TABS}
      onclick={add}
    />
    <IconButton icon="chevron-down" label="Hide the terminal" variant="ghost" size="sm" onclick={toggle} />
  </div>
  {#each tabs as tab (tab.id)}
    <div class="panel__body" hidden={tab.id !== active}>
      {#if tab.state.kind !== 'live'}
        <div class="panel__status" role="status" class:panel__status--ended={ended(tab.state)}>
          {status(tab.state)}
          {#if ended(tab.state)}<Button size="sm" variant="secondary" onclick={() => (add(), close(tab))}>New terminal</Button>{/if}
        </div>
      {/if}
      <div class="panel__screen" {@attach start(tab)}></div>
    </div>
  {/each}
</section>

<style>
  .panel {
    position: relative;
    flex: none;
    display: flex;
    flex-direction: column;
    min-height: 0;
    border-top: 1px solid var(--border-subtle);
    background: var(--surface-sunken);
  }
  .panel[hidden],
  .panel__body[hidden] {
    display: none;
  }
  .panel__edge {
    position: absolute;
    inset: -3px 0 auto;
    height: 6px;
    cursor: row-resize;
    z-index: 1;
    touch-action: none;
  }
  .panel__edge:focus-visible {
    outline: none;
    box-shadow: var(--ring-focus);
  }
  .panel__bar {
    display: flex;
    align-items: center;
    gap: var(--space-1);
    padding: 0 var(--space-1);
    border-bottom: 1px solid var(--border-subtle);
  }
  .panel__tabs {
    flex: 1;
    display: flex;
    overflow-x: auto;
  }
  .panel__tab {
    display: flex;
    flex: none;
    align-items: center;
    color: var(--fg-2);
  }
  .panel__tab--active {
    color: var(--fg-1);
    box-shadow: inset 0 -2px 0 var(--accent);
  }
  .panel__tab--ended button {
    text-decoration: line-through;
  }
  .panel__tab button {
    min-height: 32px;
    padding: 0 var(--space-2) 0 var(--space-3);
    border: 0;
    background: transparent;
    color: inherit;
    font: inherit;
    font-size: 13px;
    cursor: pointer;
  }
  .panel__tab button:focus-visible {
    outline: none;
    box-shadow: var(--ring-focus);
  }
  .panel__body {
    flex: 1;
    min-height: 0;
    display: flex;
    flex-direction: column;
  }
  .panel__status {
    display: flex;
    align-items: center;
    gap: var(--space-3);
    padding: var(--space-1) var(--space-3);
    font-size: var(--size-xs);
    color: var(--fg-2);
  }
  .panel__status--ended {
    color: var(--warning-fg);
    background: var(--warning-soft);
  }
  .panel__screen {
    flex: 1;
    min-height: 0;
    padding: var(--space-1) 0 0 var(--space-2);
  }
</style>
