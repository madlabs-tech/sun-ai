// In-memory eval replay for the mock: evals, their reports and saved sets.
// A new eval reports `running` once, then `done`, like a short replay on the kernel.
import type { EvalOutcome, EvalReport, EvalRun, EvalSet, EvalSpec } from './types';
import { MockError, clone, not_found } from './mock';

const outcome = (status: string, causes: EvalOutcome['causes'], repeats: number, actions: string[] = []): EvalOutcome => ({
  run_id: '01JARUNEVAL0000000000000000'.slice(0, 26),
  status,
  passed: status === 'succeeded',
  end_reason: status === 'succeeded' ? null : 'gate `test` failed after 2 send-backs',
  causes,
  repeats,
  actions,
});

const REPORT: EvalReport = {
  runtime: 'replay',
  replayed: 3,
  tasks: [
    {
      task_id: 'SUN-145',
      title: 'Fix wallet balance after offline sync',
      original: outcome('failed', ['test_failure', 'test_failure', 'wrong_scope'], 2),
      replay: outcome('succeeded', [], 0, ['github.open_pr']),
      error: null,
    },
    {
      task_id: 'SUN-139',
      title: 'Retry webhook deliveries with backoff',
      original: outcome('failed', ['test_failure', 'test_failure'], 2),
      replay: outcome('failed', ['test_failure'], 1),
      error: null,
    },
    {
      task_id: 'SUN-131',
      title: 'Add CSV export to the ledger page',
      original: outcome('succeeded', ['lint'], 0, []),
      replay: outcome('succeeded', [], 0, ['github.open_pr']),
      error: null,
    },
  ],
  original: { passed: 1, pass_rate: 1 / 3, mistakes: 6, repeats: 4 },
  replay: { passed: 2, pass_rate: 2 / 3, mistakes: 1, repeats: 1 },
  pass_delta: 1 / 3,
  repeat_delta: -3,
};

const evals: EvalRun[] = [
  {
    id: '01JAEVAL000000000000000001',
    name: 'Tests before done',
    spec: {
      name: 'Tests before done',
      pick: { by: 'cause', cause: 'test_failure' },
      candidate: { memory: ['Run the tests and make them pass before you finish a step.'] },
    },
    status: 'done',
    report: REPORT,
    error: null,
    created_at: '2026-10-07T08:10:00Z',
    finished_at: '2026-10-07T08:14:00Z',
  },
];

const sets: EvalSet[] = [];
/** GETs a new eval answers `running` before it is done. */
const pending = new Map<string, number>();

const invalid = (message: string) => new MockError(422, { error: { code: 'invalid_input', message } });

/** A read of a running eval: the first answers `running`, the next finishes it. */
function progress(run: EvalRun) {
  const left = pending.get(run.id) ?? 0;
  if (left > 0) pending.set(run.id, left - 1);
  else if (run.status === 'running') {
    const memory = run.spec.candidate?.memory ?? [];
    Object.assign(run, {
      status: 'done',
      finished_at: new Date().toISOString(),
      report: memory.length ? REPORT : { ...REPORT, tasks: REPORT.tasks.map((t) => ({ ...t, replay: t.original })), replay: REPORT.original, pass_delta: 0, repeat_delta: 0 },
    });
  }
}

export function eval_route(method: string, parts: string[], input: Record<string, unknown>): unknown {
  const key = `${method} /${parts[0]}${parts[1] ? '/:id' : ''}`;
  switch (key) {
    case 'GET /evals':
      evals.forEach(progress);
      return clone(evals);
    case 'GET /evals/:id': {
      const run = evals.find((e) => e.id === parts[1]);
      if (!run) throw not_found('Eval');
      progress(run);
      return clone(run);
    }
    case 'POST /evals': {
      const spec = input as unknown as EvalSpec;
      if (!spec.name?.trim()) throw invalid('name: one line of 1–200 chars');
      if (!spec.pick) throw invalid('missing field `pick`');
      const run: EvalRun = {
        id: `01JAEVAL${String(evals.length + 1).padStart(18, '0')}`,
        name: spec.name.trim(),
        spec: clone(spec),
        status: 'running',
        report: null,
        error: null,
        created_at: new Date().toISOString(),
        finished_at: null,
      };
      evals.unshift(run);
      pending.set(run.id, 1);
      return clone(run);
    }
    case 'GET /eval_sets':
      return clone(sets);
    case 'POST /eval_sets': {
      const task_ids = input.task_ids as string[] | undefined;
      if (!String(input.name ?? '').trim() || !task_ids?.length) throw invalid('an eval set needs a name and 1–50 tasks');
      const set: EvalSet = { id: `01JASET${String(sets.length + 1).padStart(19, '0')}`, name: String(input.name).trim(), task_ids, created_at: new Date().toISOString() };
      sets.unshift(set);
      return clone(set);
    }
  }
  throw not_found('Route');
}
