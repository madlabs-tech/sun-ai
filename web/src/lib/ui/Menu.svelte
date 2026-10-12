<!-- Menu button on a native popover, the one menu every screen uses: `items` render a role="menu"
     with arrow keys, Home/End, Escape and Tab; `children` render a small panel (role="dialog")
     instead, for settings such as the Display options. With `text` the trigger is a labelled
     button, without it an icon button whose tooltip is `label`; `trigger` renders a custom one (the
     account menu's workspace button) and `header` content above the items. A `null` item is a separator;
   an item whose `group` differs from the one before starts a labelled group. `readonly` shows
     only the icon and `text`, for someone who may read the value but not change it. -->
<script lang="ts" module>
  import type { Snippet } from 'svelte';

  export interface MenuItem {
    label: string;
    icon?: string;
    /** Drawn before the label instead of `icon` (e.g. a workspace's logo); gets the item. */
    lead?: Snippet<[MenuItem]>;
    /** What the item stands for, for `lead` (e.g. a workspace id). */
    id?: string;
    /** Set on every item of a one-of-many choice: a menuitemradio, checked or not. */
    checked?: boolean;
    danger?: boolean;
    /** Shown but not picked; `hint` says why. */
    disabled?: boolean;
    /** A short second line, e.g. why it is disabled. */
    hint?: string;
    /** A badge at the end, e.g. "Ready". */
    badge?: string;
    /** Items with the same `group` sit under its heading. */
    group?: string;
    onselect: () => void;
  }
</script>

<script lang="ts">
  import { Badge, Button, Icon } from '@hryer/madlabs-design-system/svelte';
  import IconButton from './IconButton.svelte';

  let {
    label,
    icon,
    text,
    items = [],
    children,
    name,
    variant = 'ghost',
    trigger: custom_trigger,
    header,
    readonly = false,
  }: {
    label: string;
    /** The trigger's icon; not used with a custom `trigger`. */
    icon?: string;
    text?: string;
    items?: (MenuItem | null)[];
    children?: Snippet;
    /** The trigger's accessible name when it says more than the tooltip (e.g. which task). */
    name?: string;
    variant?: 'ghost' | 'secondary';
    /** Renders the trigger itself (e.g. the account menu's workspace button): spread `attrs` on its button. */
    trigger?: Snippet<[attrs: Record<string, unknown>]>;
    /** Shown above the items, outside the role="menu" (e.g. who is signed in). */
    header?: Snippet;
    /** Just the value: no trigger, no menu. */
    readonly?: boolean;
  } = $props();

  const uid = $props.id();
  let panel = $state<HTMLDivElement>();
  let open = $state(false);

  const trigger = () => document.querySelector<HTMLElement>(`[popovertarget="${uid}"]`);
  const focusables = () => [
    ...(panel?.querySelectorAll<HTMLElement>(children ? 'button, input, select, [tabindex]' : '[role^="menuitem"]') ?? []),
  ];

  /** Opens toward the larger side of the window from the trigger, never past its edges. Needs no
   *  measuring, so it runs before the panel shows (no jump). */
  function place(e: ToggleEvent) {
    const button = trigger();
    if (e.newState !== 'open' || !button || !panel) return;
    const r = button.getBoundingClientRect();
    const [left, up] = [r.left + r.width / 2 < innerWidth / 2, r.top + r.height / 2 > innerHeight / 2];
    const x = left ? Math.max(8, r.left) : Math.max(8, innerWidth - r.right);
    const width = innerWidth - x - 8;
    const height = up ? r.top - 12 : innerHeight - r.bottom - 12;
    Object.assign(panel.style, {
      left: left ? `${x}px` : 'auto',
      right: left ? 'auto' : `${x}px`,
      top: up ? 'auto' : `${r.bottom + 4}px`,
      bottom: up ? `${innerHeight - r.top + 4}px` : 'auto',
      maxWidth: `${width}px`,
      minWidth: `${Math.min(200, width)}px`,
      maxHeight: `${height}px`,
    });
  }

  function toggled(e: ToggleEvent) {
    open = e.newState === 'open';
    if (open && !panel?.contains(document.activeElement)) focusables()[0]?.focus();
  }

  function trigger_key(e: KeyboardEvent) {
    if (e.key === 'Escape' && panel?.matches(':popover-open')) e.stopPropagation();
    if (children || (e.key !== 'ArrowDown' && e.key !== 'ArrowUp')) return;
    e.preventDefault();
    if (!panel?.matches(':popover-open')) panel?.showPopover();
    const list = focusables();
    list[e.key === 'ArrowDown' ? 0 : list.length - 1]?.focus();
  }

  function panel_key(e: KeyboardEvent) {
    if (e.key === 'Escape') {
      // The popover closes itself and refocuses the trigger; a panel or dialog behind it stays open.
      e.stopPropagation();
      return;
    }
    if (children) return;
    const list = focusables();
    const at = list.indexOf(document.activeElement as HTMLElement);
    const next = ({ ArrowDown: at + 1, ArrowUp: at - 1, Home: 0, End: list.length - 1 } as Record<string, number>)[e.key];
    if (next !== undefined) {
      e.preventDefault();
      list[(next + list.length) % list.length]?.focus();
    } else if (e.key === 'Tab') {
      panel?.hidePopover();
    }
  }

  const pick = (item: MenuItem) => () => {
    if (item.disabled) return;
    panel?.hidePopover();
    item.onselect();
  };

  /** The group heading item `i` starts, if any. */
  const heading = (i: number) => {
    const group = items[i]?.group;
    const before = items.slice(0, i).filter((x) => x).at(-1);
    return group && group !== before?.group ? group : '';
  };

  const attrs = $derived({ popovertarget: uid, 'aria-haspopup': children ? 'dialog' : 'menu', 'aria-expanded': open } as const);
</script>

{#if readonly}
  <span class="menu__value">{#if icon}<Icon name={icon} size={16} />{/if}{text}</span>
{:else if custom_trigger}
  {@render custom_trigger({ ...attrs, onkeydown: trigger_key })}
{:else if text}
  <Button type="button" variant={variant} size="sm" iconLeft={icon} aria-label="{label}: {text}" {...attrs} onkeydown={trigger_key}>{text}</Button>
{:else}
  <IconButton type="button" icon={icon ?? 'ellipsis'} {label} {variant} size="sm" aria-label={name ?? label} {...attrs} onkeydown={trigger_key} />
{/if}

{#if !readonly}
<!-- svelte-ignore a11y_no_noninteractive_element_interactions: arrow keys move between the items inside -->
<div bind:this={panel} id={uid} popover="auto" class="menu" role="presentation" onbeforetoggle={place} ontoggle={toggled} onkeydown={panel_key}>
  {@render header?.()}
  {#if children}
    <div role="dialog" aria-label={label} class="menu__panel">{@render children()}</div>
  {:else}
    <div role="menu" aria-label={name ?? label}>
      {#each items as item, i (i)}
        {#if item}
          {#if heading(i)}<div class="menu__group" role="presentation">{heading(i)}</div>{/if}
          <button
            type="button"
            role={item.checked === undefined ? 'menuitem' : 'menuitemradio'}
            aria-checked={item.checked}
            aria-disabled={item.disabled || undefined}
            tabindex="-1"
            class="menu__item"
            class:menu__item--danger={item.danger}
            onclick={pick(item)}
          >
            {#if item.lead}{@render item.lead(item)}{:else if item.icon}<Icon name={item.icon} size={16} />{/if}
            <span class="menu__label">
              {item.label}
              {#if item.hint}<span class="menu__hint">{item.hint}</span>{/if}
            </span>
            {#if item.badge}<Badge tone="success">{item.badge}</Badge>{/if}
            {#if item.checked}<Icon name="check" size={16} />{/if}
          </button>
        {:else}
          <div role="separator" class="menu__separator"></div>
        {/if}
      {/each}
    </div>
  {/if}
</div>
{/if}

<style>
  .menu__value {
    display: inline-flex;
    align-items: center;
    gap: var(--space-2);
    padding: var(--space-1) 0;
    color: var(--fg-1);
    overflow-wrap: anywhere;
  }
  /* Popover: fixed, placed by `place()` next to the trigger. */
  .menu {
    position: fixed;
    inset: auto;
    margin: 0;
    box-sizing: border-box;
    min-width: 200px;
    max-width: calc(100vw - 16px);
    padding: var(--space-1);
    overflow-y: auto;
    border: 1.5px solid var(--border-default);
    border-radius: var(--radius-lg);
    background: var(--surface-card);
    color: var(--fg-1);
    box-shadow: var(--shadow-lg);
  }
  .menu__panel {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
    width: 260px;
    max-width: 100%;
    padding: var(--space-2);
    box-sizing: border-box;
  }
  .menu__item {
    display: flex;
    align-items: center;
    gap: 10px;
    width: 100%;
    min-height: 36px;
    padding: 0 var(--space-2);
    border: 0;
    border-radius: var(--radius-sm);
    background: transparent;
    color: var(--fg-1);
    font: inherit;
    font-size: 14px;
    text-align: left;
    cursor: pointer;
  }
  .menu__item:hover,
  .menu__item:focus-visible {
    outline: none;
    background: var(--accent-soft);
  }
  .menu__item:focus-visible {
    box-shadow: var(--ring-focus);
  }
  .menu__item--danger {
    color: var(--danger-fg);
  }
  .menu__item[aria-disabled='true'] {
    color: var(--fg-3);
    cursor: not-allowed;
  }
  .menu__label {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .menu__hint {
    display: block;
    overflow: hidden;
    text-overflow: ellipsis;
    font-size: var(--size-xs);
    color: var(--fg-2);
  }
  .menu__group {
    padding: var(--space-2) var(--space-2) var(--space-1);
    font-size: var(--size-xs);
    color: var(--fg-2);
  }
  .menu__separator {
    height: 1.5px;
    margin: var(--space-1) 0;
    background: var(--border-subtle);
  }
  @media (max-width: 1023px) {
    .menu__item {
      min-height: var(--control-md);
    }
  }
</style>
