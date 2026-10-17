// The permissions picker of the agent form and of a chat without an agent: which modes it offers, and its hint.
import { PERMISSION_MODES, type Host, type PermissionMode, type Runtime } from './api/types';
import { can } from './app_state.svelte';
import { PERMISSION_MODE_COPY as MODE } from './format';

/** Why the caller can't pick bypass ('' when they can): the operator's `runtimes.allow_bypass`, as every runtime
 *  reports it, and `manage_runtimes`. */
// ponytail: BLO-164's `can_bypass` on /me replaces this body.
export function bypass_off(runtimes: Runtime[]): string {
  if (!runtimes.some((r) => r.allow_bypass)) return '“Anything” is off on this deployment (sun.yaml runtimes.allow_bypass).';
  if (!can('manage_runtimes')) return '“Anything” needs the manage runtimes permission.';
  return '';
}

/** Some online host (`host` only, when set) has a container or VM ready: where a bypass session may run. */
export const isolated = (hosts: Host[], host?: string) =>
  hosts.some(
    (h) => h.status === 'online' && (!host || h.id === host) && (h.capabilities.exec_envs ?? []).some((e) => e.ready && e.kind !== 'bare'),
  );

/** The picker's options: the `offered` modes (`chain_modes`), bypass only when the caller may pick it and it has
 *  somewhere to run. `current` always shows, so what was saved in a mode stays editable. */
export const mode_options = (offered: PermissionMode[], allow_bypass: boolean, isolated: boolean, current: PermissionMode | '' | null) => [
  { value: '', label: 'Host default' },
  ...PERMISSION_MODES.filter((m) => m === current || (offered.includes(m) && (m !== 'bypass_permissions' || (allow_bypass && isolated)))).map((m) => ({
    value: m,
    label: MODE[m][0],
  })),
];

/** What the picked mode lets a session do, and why bypass isn't offered. `off`: `bypass_off`'s answer. */
export function mode_hint(mode: PermissionMode | '', off: string, isolated: boolean): string {
  if (mode === 'bypass_permissions' && off) return `${off} Pick another mode and you can’t pick it back.`;
  if (mode === 'bypass_permissions' && !isolated)
    return `${MODE[mode][1]} No online host has a container or VM ready, so its runs are refused until one does.`;
  if (mode) return MODE[mode][1];
  const why = off || (isolated ? '' : '“Anything” needs an online host with a container or VM ready.');
  return why ? `Each host’s sun.yaml default. ${why}` : 'Each host’s sun.yaml default.';
}
