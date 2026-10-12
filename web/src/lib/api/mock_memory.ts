// In-memory long-term memory for the mock: proposals, active items, reviews, stars
// and the repeat-mistake rate. Mirrors the kernel's moves and its 409/422 answers.
import type { MemoryItem, MemoryMetrics } from './types';
import { MAIN_WORKSPACE } from './types';
import { MockError, clone, corrections_of, find_task, not_found } from './mock';

const item = (n: number, fields: Partial<MemoryItem>): MemoryItem => ({
  id: `01JAMEM${String(n).padStart(4, '0')}`.padEnd(26, '0'),
  workspace_id: MAIN_WORKSPACE,
  scope: 'ecosystem',
  scope_id: null,
  scope_name: null,
  body: '',
  target: null,
  reason: '',
  source_run: null,
  status: 'proposed',
  author: 'sun',
  reviewed_by: null,
  created_at: '2026-10-07T09:40:00Z',
  pr: null,
  ...fields,
});

const items: MemoryItem[] = [
  item(1, {
    scope: 'repo',
    scope_id: '01JAREPO0001',
    scope_name: 'acme/api',
    body: 'Run the tests and make them pass before you finish a step.',
    target: 'CLAUDE.md',
    reason: 'test_failure corrected 3 times in fix-bug; latest: Gate `test` failed: FAIL wallet/offline_test.go:58',
    pr: { status: 'delivered', url: 'https://github.com/acme/api/pull/418' },
  }),
  item(2, {
    scope: 'workflow',
    scope_id: '01JAWORKFLOW',
    scope_name: 'fix-bug',
    body: 'Don’t repeat this past mistake: Only change the balance cache; the sync scheduler is out of scope.',
    target: '.sun/prompts/fix-bug.md',
    reason: 'steer (wrong_scope) starred by user:ana',
    author: 'user:ana',
    created_at: '2026-10-07T09:35:00Z',
    pr: { status: 'pending', url: null },
  }),
  item(3, {
    body: 'Write dates as YYYY-MM-DD in every customer-facing text.',
    reason: 'learning note from step draft starred by user:ben',
    status: 'active',
    author: 'user:ben',
    reviewed_by: 'user:ana',
    created_at: '2026-09-29T14:02:00Z',
  }),
  item(4, {
    scope: 'workflow',
    scope_id: '01JAWORKFLOW',
    scope_name: 'fix-bug',
    body: 'Change only what the task asks for; leave unrelated files and code alone.',
    reason: 'wrong_scope corrected 3 times in fix-bug; latest: Don’t touch the migrations',
    status: 'active',
    reviewed_by: 'user:ben',
    created_at: '2026-09-21T10:15:00Z',
  }),
];

const week = (i: number, corrections: number, repeats: number) => ({
  week_start: new Date(Date.UTC(2026, 7, 12 + 7 * i)).toISOString(),
  corrections,
  repeats,
  rate: corrections ? repeats / corrections : null,
});

const METRICS: Omit<MemoryMetrics, 'active_items' | 'proposed_items'> = {
  overall: { corrections: 41, repeats: 17, rate: 17 / 41 },
  weeks: [week(0, 6, 4), week(1, 7, 4), week(2, 5, 3), week(3, 0, 0), week(4, 8, 3), week(5, 6, 2), week(6, 5, 1), week(7, 4, 0)],
  by_workflow: [
    { name: 'fix-bug', corrections: 26, repeats: 13, rate: 0.5 },
    { name: 'newsletter', corrections: 15, repeats: 4, rate: 4 / 15 },
  ],
  by_runtime: [
    { name: 'claude-code', corrections: 24, repeats: 9, rate: 9 / 24 },
    { name: 'codex', corrections: 17, repeats: 8, rate: 8 / 17 },
  ],
  first_pass: {
    overall: { gate_steps: 52, first_pass: 34, rate: 34 / 52 },
    weeks: [[6, 3], [7, 4], [5, 3], [0, 0], [8, 5], [9, 6], [9, 6], [8, 7]].map(([gate_steps, first_pass], i) => ({
      week_start: new Date(Date.UTC(2026, 7, 12 + 7 * i)).toISOString(),
      gate_steps,
      first_pass,
      rate: gate_steps ? first_pass / gate_steps : null,
    })),
    by_workflow: [
      { name: 'fix-bug', gate_steps: 33, first_pass: 20, rate: 20 / 33 },
      { name: 'newsletter', gate_steps: 19, first_pass: 14, rate: 14 / 19 },
    ],
    by_runtime: [
      { name: 'claude-code', gate_steps: 30, first_pass: 22, rate: 22 / 30 },
      { name: 'codex', gate_steps: 22, first_pass: 12, rate: 12 / 22 },
    ],
  },
  review_rejections: {
    overall: { corrections: 9, repeats: 3, rate: 3 / 9 },
    weeks: [week(0, 2, 0), week(1, 2, 1), week(2, 1, 0), week(3, 0, 0), week(4, 2, 1), week(5, 1, 1), week(6, 1, 0), week(7, 0, 0)],
    by_workflow: [
      { name: 'fix-bug', corrections: 7, repeats: 3, rate: 3 / 7 },
      { name: 'newsletter', corrections: 2, repeats: 0, rate: 0 },
    ],
    by_runtime: [
      { name: 'codex', corrections: 5, repeats: 2, rate: 2 / 5 },
      { name: 'claude-code', corrections: 4, repeats: 1, rate: 1 / 4 },
    ],
  },
};

const starred = new Set<string>();

export function memory_route(method: string, parts: string[], input: Record<string, unknown>): unknown {
  const key = `${method} /${parts[0]}${parts[1] ? '/:id' : ''}${parts[2] ? '/' + parts[2] : ''}`;
  switch (key) {
    case 'GET /memory':
      return clone(items);
    case 'GET /memory/:id': {
      if (parts[1] !== 'metrics') throw not_found('Route');
      const count = (status: MemoryItem['status']) => items.filter((m) => m.status === status).length;
      return clone({ ...METRICS, active_items: count('active'), proposed_items: count('proposed') });
    }
    case 'POST /memory/:id/review': {
      const m = items.find((x) => x.id === parts[1]);
      if (!m) throw not_found('Memory item');
      const to = input.status;
      if (to !== 'active' && to !== 'retired') throw new MockError(422, { error: { code: 'invalid_input', message: 'Send {"status": "active"} or "retired".' } });
      const allowed = m.status === 'proposed' || (m.status === 'active' && to === 'retired');
      if (!allowed) throw new MockError(409, { error: { code: 'conflict', message: `invalid MemoryStatus transition: ${m.status} -> ${to}` } });
      Object.assign(m, { status: to, reviewed_by: 'user:you' });
      return clone(m);
    }
    case 'POST /corrections/:id/star': {
      const c = corrections_of(find_task('SUN-145')).find((x) => x.id === parts[1]);
      if (!c) throw not_found('Correction');
      if (starred.has(c.id)) return { created: false };
      starred.add(c.id);
      const line = c.body.split('\n')[0];
      items.unshift(
        item(10 + starred.size, {
          scope: 'workflow',
          scope_id: c.workflow_id,
          scope_name: 'fix-bug',
          body: `Don’t repeat this past mistake: ${line}`,
          reason: `${c.kind} (${c.cause_label}) starred by user:you`,
          source_run: c.run_id,
          author: 'user:you',
          created_at: new Date().toISOString(),
        }),
      );
      return { created: true };
    }
    case 'POST /notes/:id/star': {
      if (starred.has(parts[1])) return { created: false };
      starred.add(parts[1]);
      items.unshift(
        item(10 + starred.size, {
          scope: 'ecosystem',
          scope_id: null,
          scope_name: null,
          body: 'Wallet balances need a cache TTL; stale balances confuse people offline.',
          reason: 'learning note from step implement starred by user:you',
          author: 'user:you',
          created_at: new Date().toISOString(),
        }),
      );
      return { created: true };
    }
  }
  throw not_found('Route');
}
