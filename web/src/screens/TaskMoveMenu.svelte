<!-- "Move to…": a task's legal status moves, plus Move up / Move down in a manual order. The keyboard
     and touch alternative to dragging a card. -->
<script lang="ts">
  import Menu, { type MenuItem } from '../lib/ui/Menu.svelte';
  import { legal_moves, type TaskPatch, type TaskSummary } from '../lib/api/types';
  import { STATUS_BADGE, STATUS_ICON } from '../lib/format';

  let { t, on_move, up, down }: { t: TaskSummary; on_move: (t: TaskSummary, patch: TaskPatch, from_menu?: boolean) => void; up?: () => void; down?: () => void } = $props();

  const items = $derived.by(() => {
    const list: (MenuItem | null)[] = legal_moves(t).map((s) => ({ label: STATUS_BADGE[s][1], icon: STATUS_ICON[s], onselect: () => on_move(t, { status: s }, true) }));
    if (up || down) list.push(null);
    if (up) list.push({ label: 'Move up', icon: 'arrow-up', onselect: up });
    if (down) list.push({ label: 'Move down', icon: 'arrow-down', onselect: down });
    return list;
  });
</script>

{#if items.length}
  <Menu label="Move to…" name="Move {t.key} to…" icon="arrow-left-right" {items} />
{/if}
