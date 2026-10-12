// The Assignee picker's choices, shared by the create dialog and the task page: the workspace's workflows
// (its default first), the agents (ready or disabled with the reason) and None, a plain task.
import type { Agent, Assignee, Workflow } from './api/types';
import type { MenuItem } from './ui/Menu.svelte';

export const same_assignee = (a: Assignee | null, b: Assignee | null) => a?.kind === b?.kind && a?.id === b?.id;

/** The assignee's name: an agent, a workflow, or "None". */
export function assignee_name(a: Assignee | null, workflows: Workflow[], agents: Agent[]): string {
  if (!a) return 'None';
  const found = a.kind === 'agent' ? agents.find((x) => x.id === a.id) : workflows.find((x) => x.id === a.id);
  return found?.name ?? (a.kind === 'agent' ? 'Archived agent' : 'Workflow');
}

/** Menu items for picking an assignee; `edit` opens a workflow in the builder (absent without the permission). */
export function assignee_items(
  workflows: Workflow[],
  agents: Agent[],
  default_id: string | null,
  current: Assignee | null,
  pick: (a: Assignee | null) => void,
  edit?: (workflow_id: string) => void,
): (MenuItem | null)[] {
  const ordered = [...workflows].sort((a, b) => Number(b.id === default_id) - Number(a.id === default_id));
  const open = current?.kind === 'workflow' ? current.id : (default_id ?? workflows[0]?.id);
  const choice = (a: Assignee | null) => ({ checked: same_assignee(a, current), onselect: () => pick(a) });
  return [
    ...ordered.map((w) => ({
      group: 'Workflow',
      icon: 'workflow',
      label: w.id === default_id ? `Default · ${w.name}` : w.name,
      ...choice({ kind: 'workflow', id: w.id }),
    })),
    ...(edit && open ? [{ group: 'Workflow', icon: 'pencil', label: 'Edit in builder', onselect: () => edit(open) }] : []),
    ...agents.map((a) => ({
      group: 'Agents',
      icon: 'bot',
      label: a.name,
      badge: a.ready ? 'Ready' : undefined,
      disabled: !a.ready,
      hint: a.ready ? undefined : (a.ready_reason ?? 'Not ready'),
      ...choice({ kind: 'agent', id: a.id }),
    })),
    null,
    { icon: 'minus', label: 'None', hint: 'A plain task: nothing runs', ...choice(null) },
  ];
}
