// What the task page says about one pull request. A PR's health is two independent facts from the host's
// snapshot, checks and mergeability; the row folds them into one verdict that still carries a
// conflict beside failing checks. Every snapshot field defaults defensively, so an absent one never
// fabricates a positive verdict.
import type { PullRequest } from './api/types';

type Checks =
  | { kind: 'failed'; failed: number; total: number }
  | { kind: 'pending'; passed: number; total: number; running: number }
  | { kind: 'passed'; total: number }
  | { kind: 'none' }
  | { kind: 'unavailable' };

// Priority: no snapshot → unavailable; rollup failure/error or any failed count → failed; rollup
// pending/expected → pending; rollup success → passed; a snapshot without a rollup → none.
function derive_checks(pr: PullRequest): Checks {
  if (!pr.snapshot_available) return { kind: 'unavailable' };
  const rollup = pr.checks_rollup_state;
  const [total, passed, failed, running] = [pr.checks_total ?? 0, pr.checks_passed ?? 0, pr.checks_failed ?? 0, pr.checks_pending ?? 0];
  if (rollup === 'failure' || rollup === 'error' || failed > 0) return { kind: 'failed', failed, total };
  if (rollup === 'pending' || rollup === 'expected') return { kind: 'pending', passed, total, running };
  if (rollup === 'success') return { kind: 'passed', total };
  return { kind: 'none' };
}

type Merge = 'conflicting' | 'ready' | 'blocked' | 'behind' | 'unstable' | 'has_hooks' | 'none';

// `mergeable` only says whether there is a conflict; `merge_state_status === dirty` is the host's other
// view of the same fact. "Ready" comes only from `clean`, never from `mergeable`, which ignores
// required checks and branch protection.
function derive_merge(pr: PullRequest): Merge {
  if (!pr.snapshot_available) return 'none';
  const state = pr.merge_state_status;
  if (pr.mergeable === 'conflicting' || state === 'dirty') return 'conflicting';
  if (state === 'clean') return 'ready';
  if (state === 'blocked' || state === 'behind' || state === 'unstable' || state === 'has_hooks') return state;
  return 'none';
}

/** The one thing the row says: what stands between the PR and a merge, or how it ended. */
export type Verdict =
  | { kind: 'merged' }
  | { kind: 'closed' }
  | { kind: 'failed'; failed: number; total: number; conflicting: boolean }
  | { kind: 'conflicting' }
  | { kind: 'running'; passed: number; total: number; running: number }
  | { kind: 'behind' }
  | { kind: 'blocked' }
  | { kind: 'draft' }
  | { kind: 'ready' }
  | { kind: 'passed'; total: number }
  | { kind: 'no_checks' }
  | { kind: 'unknown' };

// Priority: merged / closed → failed → conflicting → running → behind → blocked → draft → ready →
// passed → no checks → unknown. `unstable` only restates the checks and `has_hooks` is "mergeable, with
// pre-receive hooks", so neither gets a verdict of its own. A draft never reads as ready.
export function derive_verdict(pr: PullRequest): Verdict {
  if (pr.state === 'merged') return { kind: 'merged' };
  if (pr.state === 'closed') return { kind: 'closed' };
  const checks = derive_checks(pr);
  const merge = derive_merge(pr);
  if (checks.kind === 'failed') return { kind: 'failed', failed: checks.failed, total: checks.total, conflicting: merge === 'conflicting' };
  if (merge === 'conflicting') return { kind: 'conflicting' };
  if (checks.kind === 'pending') return { kind: 'running', passed: checks.passed, total: checks.total, running: checks.running };
  if (merge === 'behind') return { kind: 'behind' };
  if (merge === 'blocked') return { kind: 'blocked' };
  if (pr.state === 'draft') return { kind: 'draft' };
  if (merge === 'ready' || merge === 'has_hooks') return { kind: 'ready' };
  if (checks.kind === 'passed') return { kind: 'passed', total: checks.total };
  if (checks.kind === 'none') return { kind: 'no_checks' };
  return { kind: 'unknown' };
}

/** "+0 −0" would be a lie for a PR whose host never sent its stats: show them only once one is set. */
export const has_stats = (pr: PullRequest) => (pr.additions ?? 0) + (pr.deletions ?? 0) + (pr.changed_files ?? 0) > 0;

const escape = (value: string) => value.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');

/** A PR titled "SUN-1: fix x" or "fix x (SUN-1)" repeats the task it is listed under: drop that one
 *  key, so the truncated title spends its width on what the PR does. Other keys, and a title that is
 *  nothing but the key, stay as written. */
export function strip_task_key(title: string, key: string): string {
  if (!key) return title;
  const k = escape(key);
  const stripped = title.replace(new RegExp(`^\\s*${k}(?![\\w-])\\s*:?\\s*`, 'i'), '').replace(new RegExp(`\\s*\\(${k}\\)\\s*$`, 'i'), '');
  return stripped.trim() ? stripped : title;
}

/** Diff counts for the narrow sidebar: exact below 1,000, then "6.7k", and "123k" from 100k up. */
export function diff_count(count: number): string {
  if (count < 1000) return String(count);
  const thousands = count / 1000;
  return thousands >= 100 ? `${Math.round(thousands)}k` : `${thousands.toFixed(1).replace(/\.0$/, '')}k`;
}
