// The builder's model is the workflow file itself (1:1 with the YAML).
// Blocks edit the parsed document; the kernel (sun-workflow) is the only
// validator, so nothing here re-implements its rules.
import { Document, LineCounter, Pair, isMap, isScalar, isSeq, parse, parseDocument, stringify } from 'yaml';

export type GateEntry = string | Record<string, unknown>;

/** One `steps:` entry. Fields the builder has no block for (`needs`, `context`, `with`) are kept as is. */
export interface StepDoc {
  id: string;
  type: string;
  [field: string]: unknown;
  prompt?: string;
  skills?: string[];
  gates?: GateEntry[];
  on_fail?: { send_back_to: string; max: number };
  who?: string[];
  timeout?: { after: string; then: string };
  for?: string;
  to?: string[];
  message?: string;
  use?: string;
}

export interface WorkflowDoc {
  [field: string]: unknown;
  name: string;
  on?: string[];
  runtime?: { chain: string[]; on_limit?: string };
  budget?: { wall?: string; turns?: number | null; cost_usd?: number | null };
  steps: StepDoc[];
}

export const BLANK_WORKFLOW = `name: new-workflow
on: [manual]
steps:
  - id: work
    type: agent
    prompt: Say what the agent should do.
`;

/** Built-in step blocks; `extension` stands for any dotted step type. */
export const BLOCKS = [
  { type: 'agent', label: 'Agent', icon: 'bot', hint: 'An AI agent does the work.' },
  { type: 'gate', label: 'Gate', icon: 'shield-check', hint: 'Checks decide if the work is done.' },
  { type: 'approval', label: 'Approval', icon: 'user-check', hint: 'A person approves or rejects.' },
  { type: 'wait', label: 'Wait', icon: 'hourglass', hint: 'Pause for a while.' },
  { type: 'notify', label: 'Notify', icon: 'bell', hint: 'Send a message.' },
  { type: 'action', label: 'Plugin action', icon: 'plug', hint: 'A plugin does something, e.g. open a PR.' },
  { type: 'extension', label: 'Extension', icon: 'puzzle', hint: 'A step from an installed extension.' },
] as const;

export const block_of = (type: string) => BLOCKS.find((b) => b.type === type) ?? BLOCKS[BLOCKS.length - 1];

/** Gate names the kernel knows (sun-workflow CODE_GATES + CONTENT_GATES); dotted names are extension gates. */
export const GATES = ['diff_not_empty', 'test', 'lint', 'diagnostics_clean', 'pr_exists', 'ci_green', 'sections_present', 'length', 'format', 'links_resolve', 'terms', 'image_size', 'cites_sources'];

export const gate_name = (g: GateEntry) => (typeof g === 'string' ? g : (Object.keys(g)[0] ?? ''));

/** The YAML as a builder document, or null when it isn't a map (the YAML view shows why). */
export function to_doc(source: string): WorkflowDoc | null {
  try {
    const value = parse(source);
    if (!value || typeof value !== 'object' || Array.isArray(value)) return null;
    if (!Array.isArray(value.steps)) value.steps = [];
    return value as WorkflowDoc;
  } catch {
    return null;
  }
}

/** Drops cleared fields so an emptied input never writes `key: ""`. `needs: []` and `steps: []` mean something, so they stay. */
function clean(value: unknown): unknown {
  if (Array.isArray(value)) return value.map(clean);
  if (!value || typeof value !== 'object') return value;
  const out: Record<string, unknown> = {};
  for (const [k, v] of Object.entries(value)) {
    const c = clean(v);
    const empty = c === '' || c === null || c === undefined ||
      (Array.isArray(c) ? !c.length && k !== 'needs' && k !== 'steps' : typeof c === 'object' && !Object.keys(c).length);
    if (!empty) out[k] = c;
  }
  return out;
}

/** `doc` as YAML. With the file it came from, edits that file in place, so its comments,
 *  quoting and flow style survive block edits; a step keeps its comments when it moves. */
export function to_yaml(doc: WorkflowDoc, source?: string): string {
  const value = clean(doc);
  if (source !== undefined) {
    const file = parseDocument(source);
    if (!file.errors.length && isMap(file.contents)) {
      sync(file, file.contents, value);
      return file.toString({ lineWidth: 0 });
    }
  }
  return stringify(value, { lineWidth: 0 });
}

const step_id = (node: unknown) => (isMap(node) ? node.get('id') : undefined);

/** Makes YAML `node` hold `value`, reusing every node that can stay (and its comments). */
function sync(file: Document, node: unknown, value: unknown): unknown {
  if (isMap(node) && value && typeof value === 'object' && !Array.isArray(value)) {
    const fields = value as Record<string, unknown>;
    const key_of = (pair: Pair) => (isScalar(pair.key) ? pair.key.value : pair.key);
    node.items = node.items.filter((pair) => String(key_of(pair)) in fields);
    for (const [k, v] of Object.entries(fields)) {
      const pair = node.items.find((p) => String(key_of(p)) === k);
      if (pair) pair.value = sync(file, pair.value, v);
      else node.set(k, file.createNode(v));
    }
    return node;
  }
  if (isSeq(node) && Array.isArray(value)) {
    const old = [...node.items];
    const ids = value.map((v) => (v && typeof v === 'object' ? (v as { id?: unknown }).id : undefined));
    node.items = value.map((v, i) => {
      // Same step id (a moved step), else the item at this index unless its step lives on elsewhere.
      const match = (ids[i] !== undefined && old.find((o) => step_id(o) === ids[i])) ||
        (old[i] !== undefined && !ids.includes(step_id(old[i])) ? old[i] : undefined);
      return match ? sync(file, match, v) : file.createNode(v);
    });
    return node;
  }
  if (isScalar(node) && (value === null || typeof value !== 'object')) {
    if (node.value !== value) node.value = value;
    return node;
  }
  return file.createNode(value);
}

/** A new step of `type` with an id no other step has. */
export function new_step(type: string, taken: string[]): StepDoc {
  const base = type === 'extension' ? 'step' : type;
  let id = base;
  for (let n = 2; taken.includes(id); n++) id = `${base}-${n}`;
  const defaults: Record<string, Partial<StepDoc>> = {
    agent: { prompt: '' },
    gate: { gates: [] },
    approval: { who: ['role:admin'] },
    wait: { for: '1h' },
    notify: { to: [], message: '' },
    action: { use: '' },
    extension: { type: 'vendor.step' },
  };
  return { id, type, ...defaults[type] };
}

/** One plain line about what the step does, for its collapsed card. */
export function step_summary(s: StepDoc): string {
  switch (s.type) {
    case 'agent':
      return s.prompt?.split('\n')[0] || 'No instructions yet';
    case 'gate':
      return s.gates?.map(gate_name).join(', ') || 'No checks yet';
    case 'approval':
      return `Approved by ${s.who?.join(', ') || 'nobody yet'}`;
    case 'wait':
      return `Waits ${s.for || '…'}`;
    case 'notify':
      return `Tells ${s.to?.join(', ') || 'nobody yet'}`;
    case 'action':
      return s.use || 'No action picked';
    default:
      return s.type;
  }
}

/** Which step (index) an error line falls in, or -1 for the workflow's own fields. */
export function step_at_line(source: string, line: number): number {
  const counter = new LineCounter();
  const steps = parseDocument(source, { lineCounter: counter }).get('steps', true);
  if (!isSeq(steps) || !steps.range || line > counter.linePos(steps.range[1]).line) return -1;
  let found = -1;
  steps.items.forEach((item, i) => {
    const start = (item as { range?: number[] } | null)?.range?.[0];
    if (start !== undefined && counter.linePos(start).line <= line) found = i;
  });
  return found;
}
