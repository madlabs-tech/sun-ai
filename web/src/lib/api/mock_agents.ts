// In-memory agents for the mock: GET/POST /agents, PATCH/DELETE /agents/{id} (DELETE archives).
// Ready when a runtime of the chain is known and not offline, like the kernel's "logged in on an online host".
import type { Agent, AgentFields } from './types';
import { MAIN_WORKSPACE } from './types';
import { MockError, RUNTIMES, clone, not_found } from './mock';

const invalid = (message: string) => new MockError(422, { error: { code: 'invalid_input', message } });
let next_id = 0;
const new_id = () => `01JVAG${String(++next_id).padStart(4, '0')}`.padEnd(26, '0');

const agent = (name: string, description: string, instructions: string, runtime_chain: string[], skill_ids: string[], task_count: number): Agent => ({
  id: new_id(),
  workspace_id: MAIN_WORKSPACE,
  name,
  description,
  instructions,
  runtime_chain,
  skill_ids,
  created_at: new Date().toISOString(),
  archived_at: null,
  ready: false,
  ready_reason: null,
  task_count,
});

const agents: Agent[] = [
  agent('Ada', 'Fixes bugs with a failing test first.', 'Reproduce the bug in a failing test, fix it, keep the diff small.', ['claude-code', 'codex'], ['tdd'], 2),
  agent('Night Owl', 'Runs long refactors overnight.', '', ['gemini-cli'], [], 0),
];

/** An agent by id, archived ones too (a task keeps its agent). */
export const find_agent = (id: string) => agents.find((a) => a.id === id);

/** The agent with its readiness read now. */
function view(a: Agent): Agent {
  const reasons = a.runtime_chain.map((id) => {
    const r = RUNTIMES.find((x) => x.id === id);
    return r ? (r.status === 'offline' ? `${id}: log in to ${id} on a host` : null) : `${id}: no host reports it`;
  });
  const ready = reasons.some((r) => r === null);
  return clone({ ...a, ready, ready_reason: ready ? null : reasons.join('; ') });
}

/** `fields` set on `a`, checked like the kernel. */
function apply(a: Agent, fields: AgentFields): Agent {
  const next = { ...a, ...fields };
  next.name = next.name.trim();
  if (!next.name || [...next.name].length > 64) throw invalid('Give the agent a name of 1 to 64 characters.');
  if ([...next.description].length > 1000) throw invalid('Keep the description to 1000 characters.');
  if ([...next.instructions].length > 20_000) throw invalid('Keep the instructions to 20000 characters.');
  if (!next.runtime_chain.length || next.runtime_chain.length > 32) throw invalid('Pick 1 to 32 runtimes for the agent.');
  if (agents.some((x) => x.id !== a.id && !x.archived_at && x.name.toLowerCase() === next.name.toLowerCase()))
    throw invalid(`There is already an agent named \`${next.name}\`.`);
  return next;
}

export function agents_route(method: string, parts: string[], input: Record<string, unknown>): unknown {
  const fields = input as AgentFields;
  if (parts.length === 1 && method === 'GET') return agents.filter((a) => !a.archived_at).sort((a, b) => a.name.localeCompare(b.name)).map(view);
  if (parts.length === 1 && method === 'POST') {
    const created = apply(agent('', '', '', [], [], 0), fields);
    agents.push(created);
    return view(created);
  }
  const i = agents.findIndex((a) => a.id === parts[1]);
  if (i < 0) throw not_found('agent');
  if (method === 'DELETE') {
    agents[i].archived_at ??= new Date().toISOString();
    return null;
  }
  if (method === 'PATCH') {
    if (agents[i].archived_at) throw invalid(`The agent ${agents[i].name} is archived.`);
    agents[i] = apply(agents[i], fields);
    return view(agents[i]);
  }
  throw not_found('route');
}
