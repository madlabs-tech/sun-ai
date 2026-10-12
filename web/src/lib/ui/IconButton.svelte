<script lang="ts">
  /** DS IconButton inside the DS Tooltip, so every icon-only button names its action on hover and focus. */
  import { IconButton, Tooltip, type IconButtonProps } from '@hryer/madlabs-design-system/svelte';

  /** `shortcut`: the key hint the tooltip shows, e.g. "⌘B". */
  let { shortcut, ...props }: IconButtonProps & { shortcut?: string } = $props();
  let below = $state(false);

  const MARGIN = 8;

  /** Keeps the bubble on screen: shifts it sideways near the left/right edge, flips it below near the top. */
  function fit(button: HTMLButtonElement) {
    const tip = button.parentElement;
    const bubble = tip?.querySelector<HTMLElement>('.ml-tip__bubble');
    if (!tip || !bubble) return;
    bubble.setAttribute('aria-hidden', 'true'); // the button's aria-label already says it
    const place = () => {
      const r = tip.getBoundingClientRect();
      const half = bubble.offsetWidth / 2;
      const center = r.left + r.width / 2;
      const shift = Math.min(0, innerWidth - MARGIN - (center + half)) || Math.max(0, MARGIN - (center - half));
      tip.style.setProperty('--sun-tip-shift', `${shift}px`);
      below = r.top - bubble.offsetHeight - MARGIN < 0;
    };
    place();
    button.addEventListener('pointerenter', place);
    button.addEventListener('focus', place);
    return () => {
      button.removeEventListener('pointerenter', place);
      button.removeEventListener('focus', place);
    };
  }
</script>

<Tooltip content={props.label} {shortcut} placement={below ? 'bottom' : 'top'} class="sun-tip">
  <IconButton {...props} title={undefined} {@attach fit} />
</Tooltip>

<style>
  /* The DS centers the bubble on the button; --sun-tip-shift moves it back on screen and the arrow back over the button. */
  :global(.sun-tip .ml-tip__bubble) {
    margin-left: var(--sun-tip-shift, 0px);
  }
  :global(.sun-tip .ml-tip__bubble::after) {
    margin-left: calc(-5px - var(--sun-tip-shift, 0px));
  }
</style>
