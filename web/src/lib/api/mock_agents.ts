// In-memory agents for the mock: GET/POST /agents, PATCH/DELETE /agents/{id} (DELETE archives).
// Ready when a runtime of the chain is known and not offline, like the kernel's "logged in on an online host".
import type { Agent, AgentFields, ChainEntry } from './types';
import { chain_modes, entry_name } from '../format';
import { MAIN_WORKSPACE } from './types';
import { CREDENTIALS, MockError, RUNTIMES, clone, not_found } from './mock';

const invalid = (message: string) => new MockError(422, { error: { code: 'invalid_input', message } });
let next_id = 0;
const new_id = () => `01JVAG${String(++next_id).padStart(4, '0')}`.padEnd(26, '0');

const agent = (name: string, description: string, instructions: string, runtime_chain: ChainEntry[], skill_ids: string[], task_count: number): Agent => ({
  id: new_id(),
  workspace_id: MAIN_WORKSPACE,
  name,
  description,
  instructions,
  runtime_chain,
  skill_ids,
  permission_mode: null,
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
// `?no_bypass`: Night Owl was saved in bypass before the operator turned it off. `?bypass_agent`: it is in
// bypass, the deployment allowing it.
const agent_params = new URLSearchParams(location.search);
if (agent_params.has('no_bypass') || agent_params.has('bypass_agent')) agents[1].permission_mode = 'bypass_permissions';

/** An agent by id, archived ones too (a task keeps its agent). */
export const find_agent = (id: string) => agents.find((a) => a.id === id);

/** The names of the active agents whose chain names `credential` (DELETE /credentials/{name} refuses those). */
export const agents_naming = (credential: string) =>
  agents.filter((a) => !a.archived_at && a.runtime_chain.some((e) => entry_name(e) === credential)).map((a) => a.name);

/** The agent with its readiness read now; a credential runs on its tool's hosts. */
function view(a: Agent): Agent {
  const reasons = a.runtime_chain.map(entry_name).map((id) => {
    const tool = CREDENTIALS.find((c) => c.name === id)?.default_tool ?? id;
    const of_name = RUNTIMES.filter((x) => x.name === tool);
    // Logged in on an online host; a limit doesn't log it out.
    const ready = of_name.some((x) => ['idle', 'busy', 'limited'].includes(x.state));
    return of_name.length ? (ready ? null : `${id}: log in to ${id} on a host`) : `${id}: no host reports it`;
  });
  const ready = reasons.some((r) => r === null);
  return clone({ ...a, ready, ready_reason: ready ? null : reasons.join('; ') });
}

/** A `{tool, host, model, effort}` entry checked like the kernel: the host reported the tool, the effort is in its catalog. */
function check_entry(e: ChainEntry, i: number) {
  if (typeof e === 'string') return;
  const reports = RUNTIMES.filter((r) => r.tool === e.tool && (!e.host || r.host.id === e.host));
  if (e.host && !reports.length) throw invalid(`invalid input: runtime_chain[${i}].host: that host has not reported ${e.tool}`);
  const offered = (r: (typeof reports)[number]) => (r.models.find((m) => m.id === e.model)?.effort_options ?? r.effort_options).some((l) => l.value === e.effort);
  if (e.effort && !reports.some(offered)) throw invalid(`invalid input: runtime_chain[${i}].effort: \`${e.effort}\` is not an effort level ${e.tool} offers`);
}

/** `fields` set on `a`, checked like the kernel. */
function apply(a: Agent, fields: AgentFields): Agent {
  const next = { ...a, ...fields };
  next.name = next.name.trim();
  if (!next.name || [...next.name].length > 64) throw invalid('Give the agent a name of 1 to 64 characters.');
  if ([...next.description].length > 1000) throw invalid('Keep the description to 1000 characters.');
  if ([...next.instructions].length > 20_000) throw invalid('Keep the instructions to 20000 characters.');
  if (!next.runtime_chain.length || next.runtime_chain.length > 32) throw invalid('Pick 1 to 32 runtimes for the agent.');
  next.runtime_chain.forEach(check_entry);
  if (next.permission_mode && !chain_modes(RUNTIMES, next.runtime_chain, CREDENTIALS).includes(next.permission_mode))
    throw invalid(`invalid input: permission_mode: the chain can't run in \`${next.permission_mode}\` on its hosts`);
  if (fields.permission_mode === 'bypass_permissions' && !RUNTIMES[0]?.allow_bypass)
    throw invalid('bypass_permissions lets the agent run anything. Set `runtimes.allow_bypass: true` in sun.yaml first.');
  if (agents.some((x) => x.id !== a.id && !x.archived_at && x.name.toLowerCase() === next.name.toLowerCase()))
    throw invalid(`There is already an agent named \`${next.name}\`.`);
  return next;
}

export function agents_route(method: string, parts: string[], input: Record<string, unknown>, workspace = MAIN_WORKSPACE): unknown {
  const fields = input as AgentFields;
  if (parts.length === 1 && method === 'GET') return agents.filter((a) => !a.archived_at).sort((a, b) => a.name.localeCompare(b.name)).map(view);
  if (parts.length === 1 && method === 'POST') {
    const created = { ...apply(agent('', '', '', [], [], 0), fields), workspace_id: workspace };
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
