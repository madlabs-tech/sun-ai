// In-memory knowledge for the mock: sources, docs and chunks, search, embedding settings,
// and each task's context selection with its pack preview. Mirrors the kernel's 422 messages.
import type { ContextPreview, Embeddings, KnowledgeKind, KnowledgeSource, SearchHit, SourceSettings, TaskContext, UploadDoc, UploadFile } from './types';
import { MAIN_WORKSPACE } from './types';
import { MockError, clone, find_workspace, find_task, not_found } from './mock';

interface Chunk {
  heading_path: string;
  body: string;
}
interface Doc {
  id: string;
  source_id: string;
  external_id: string;
  title: string;
  version: string;
  url: string | null;
  updated_at: string | null;
  fetched_at: string;
  chunks: Chunk[];
}

/** The kernel's estimate: 4 bytes per token, rounded up. */
const estimate = (text: string) => Math.ceil(new TextEncoder().encode(text).length / 4);
const invalid = (message: string) => new MockError(422, { error: { code: 'invalid_input', message } });

/** Required param per kind (gdrive's `folder` is optional); `upload` has none. */
const PARAMS: Record<KnowledgeKind, [required: string[], optional: string[]]> = {
  upload: [[], []],
  repo: [['path'], []],
  gdrive: [[], ['folder']],
  confluence: [['space'], []],
  jira: [['project'], []],
  slack: [['channel'], []],
};

const source = (id: string, kind: KnowledgeKind, name: string, settings: SourceSettings, last_sync: KnowledgeSource['last_sync']) =>
  ({ id, kind, name, settings, cursor: null, last_sync });

const sources: Omit<KnowledgeSource, 'docs' | 'chunks' | 'last_indexed_at'>[] = [
  source('src_runbooks', 'upload', 'Runbooks', {}, null),
  source('src_wiki', 'confluence', 'Engineering wiki', { space: 'ENG', max_age_days: 180 }, {
    at: '2026-10-07T09:45:00Z',
    report: { indexed: 2, unchanged: 14, skipped: ['architecture.drawio: not text', 'load-test.xlsx: larger than 5 MB'], retired: 0 },
    error: null,
  }),
  source('src_tickets', 'jira', 'Tickets', { project: 'MAD' }, {
    at: '2026-10-07T09:50:00Z',
    error: 'Jira answered 401: the token expired. Run `sun secret set JIRA_TOKEN` and re-index.',
  }),
];

const doc = (id: string, source_id: string, title: string, version: string, updated_at: string, url: string | null, chunks: Chunk[]): Doc =>
  ({ id, source_id, external_id: url ?? `${title.toLowerCase().replace(/\W+/g, '-')}.md`, title, version, url, updated_at, fetched_at: '2026-10-07T09:45:00Z', chunks });

const docs: Doc[] = [
  doc('doc_rollback', 'src_runbooks', 'Deploy rollback', '1728290000000', '2026-09-12T08:00:00Z', null, [
    { heading_path: 'Deploy rollback', body: 'Roll back when error rate stays above 2% for five minutes after a deploy. Page the on-call before you start.' },
    { heading_path: 'Deploy rollback > Steps', body: '1. Run `sun deploy rollback --to previous`.\n2. Watch the error rate for ten minutes.\n3. Post the incident link in #ops.\n\nNever roll back a migration by hand: restore the backup taken before it ran.' },
  ]),
  doc('doc_oncall', 'src_runbooks', 'On-call handbook', '1727000000000', '2026-08-20T08:00:00Z', null, [
    { heading_path: 'On-call handbook > Escalation', body: 'Acknowledge a page within 10 minutes. After 30 minutes without a fix, escalate to the payments lead.' },
  ]),
  doc('doc_payments', 'src_wiki', 'Payments architecture', 'v14', '2026-09-30T10:00:00Z', 'https://acme.atlassian.net/wiki/spaces/ENG/pages/101', [
    { heading_path: 'Payments architecture > Session timeout', body: 'Mobile sessions expire after 15 minutes idle. The refresh token is valid for 30 days and is rotated on every refresh.' },
    { heading_path: 'Payments architecture > Retries', body: 'Clients retry a failed refresh three times with backoff (1 s, 4 s, 16 s), then sign the user out.' },
  ]),
  doc('doc_release', 'src_wiki', 'Release checklist', 'v3', '2026-03-02T10:00:00Z', 'https://acme.atlassian.net/wiki/spaces/ENG/pages/202', [
    { heading_path: 'Release checklist', body: 'Before a release: changelog updated, migrations reviewed, feature flags default off, rollback plan linked.' },
  ]),
  doc('doc_mad88', 'src_tickets', 'MAD-88 Checkout button states', 'rev 5', '2026-10-01T12:00:00Z', 'https://acme.atlassian.net/browse/MAD-88', [
    { heading_path: 'MAD-88 Checkout button states', body: 'Disabled, loading and error states for the checkout button. Loading keeps the label and adds a spinner.' },
  ]),
];

/** Workspace links: an imported source is the same row on several workspaces. Seeds are on Main. */
const links = new Map<string, Set<string>>(sources.map((s) => [s.id, new Set([MAIN_WORKSPACE])]));
const on_workspace = (workspace: string | null) => (s: { id: string }) => !workspace || !!links.get(s.id)?.has(workspace);

/** Admin import: links source `id` to `workspace` too. */
export function link_source(workspace: string, id: string) {
  find_workspace(workspace);
  links.get(find_source(id).id)?.add(workspace);
}

let embeddings: Embeddings = { provider: 'none', base_url: '', model: '', dimensions: null, has_api_key: false };
const contexts = new Map<string, TaskContext>();

function find_source(id: string) {
  const s = sources.find((x) => x.id === id);
  if (!s) throw not_found(`knowledge source ${id}`);
  return s;
}

function find_doc(id: string) {
  const d = docs.find((x) => x.id === id);
  if (!d) throw not_found(`knowledge doc ${id}`);
  return d;
}

function check_settings(kind: string, settings: unknown): SourceSettings {
  if (!(kind in PARAMS)) throw invalid(`Unknown source kind \`${kind}\`. Use upload, repo or one of: gdrive, confluence, jira, slack.`);
  if (settings == null) settings = {};
  if (typeof settings !== 'object' || Array.isArray(settings)) throw invalid('The source settings must be an object.');
  const [required, optional] = PARAMS[kind as KnowledgeKind];
  const config = settings as Record<string, unknown>;
  for (const key of required) if (!(key in config)) throw invalid(`A ${kind} source needs \`${key}\`.`);
  for (const [key, value] of Object.entries(config)) {
    if (key === 'max_age_days') {
      if (!Number.isInteger(value) || (value as number) < 1 || (value as number) > 36_500) throw invalid('`max_age_days` must be a whole number of days.');
    } else if (key === 'export_webhook' && kind !== 'upload' && kind !== 'repo') {
      if (typeof value !== 'string' || !/^(https:\/\/\S+|http:\/\/(localhost|127\.0\.0\.1|\[::1\])[:/]\S*)$/.test(value)) throw invalid('`export_webhook` must be an https URL (or http to localhost).');
    } else if (required.includes(key) || optional.includes(key)) {
      if (typeof value !== 'string' || !value.trim() || value.length > 200) throw invalid(`\`${key}\` must be 1..=200 characters.`);
    } else throw invalid(`A ${kind} source has no setting \`${key}\`.`);
  }
  if (kind === 'repo' && !String(config.path).startsWith('/')) throw invalid("A repo source's `path` must be an absolute folder on the kernel's machine.");
  return config as SourceSettings;
}

function check_context(input: Record<string, unknown>): TaskContext {
  const pinned = ((input.pinned ?? []) as string[]).map((p) => p.trim()).filter(Boolean);
  const context = {
    sources: (input.sources ?? null) as string[] | null, budget_tokens: (input.budget_tokens ?? null) as number | null, pinned,
    block_over_budget: input.block_over_budget === true,
  };
  if (context.budget_tokens === 0) throw invalid('The budget must be at least 1 token.');
  if (pinned.length > 20) throw invalid('Pin at most 20 constraints.');
  if (pinned.some((p) => p.length > 500)) throw invalid('Keep each pinned constraint under 500 characters.');
  context.sources?.forEach(find_source);
  return context;
}

const summary_of = (s: (typeof sources)[number]): KnowledgeSource => {
  const own = docs.filter((d) => d.source_id === s.id);
  return {
    ...s,
    docs: own.length,
    chunks: own.reduce((n, d) => n + d.chunks.length, 0),
    last_indexed_at: own.map((d) => d.fetched_at).sort().at(-1) ?? null,
  };
};

const doc_view = ({ chunks, ...d }: Doc) => ({ ...d, chunks: chunks.length });

const citation = (d: Doc, c: Chunk) =>
  `${d.title}${c.heading_path && c.heading_path !== d.title ? ' > ' + c.heading_path.replace(`${d.title} > `, '') : ''} · ${d.version}`;

function search(q: string, only: string[]): SearchHit[] {
  const terms = q.toLowerCase().split(/\s+/).filter(Boolean);
  return docs
    .filter((d) => !only.length || only.includes(d.source_id))
    .flatMap((d) => d.chunks.map((c) => ({ d, c, score: terms.filter((t) => `${d.title} ${c.heading_path} ${c.body}`.toLowerCase().includes(t)).length })))
    .filter((h) => h.score > 0)
    .sort((a, b) => b.score - a.score)
    .map(({ d, c }) => ({
      citation: [citation(d, c), d.updated_at?.slice(0, 10), d.url].filter(Boolean).join(' · '),
      text: c.body,
      pointer: `doc:${d.id}`,
      doc_id: d.id,
      flags: [],
    }));
}

/** Same rules as the kernel's pack compiler: P0 always in, then by priority while it fits; the rest become pointers. */
function preview(task_title: string, context: TaskContext): ContextPreview {
  const budget = context.budget_tokens ?? 8000;
  const task_body = `# ${task_title}\n\nDo the work described in the task, cite the sources you use and stop when the gates pass.`;
  const candidates = [
    { priority: 0, label: `Task: ${task_title}`, pointer: 'sun.task', tokens: estimate(task_body) },
    ...docs
      .filter((d) => !context.sources || context.sources.includes(d.source_id))
      .map((d, i) => ({ priority: 4, label: `Source: ${citation(d, d.chunks[0])}`, pointer: `.sun/context/sources/${i + 1}.md`, tokens: estimate(d.chunks.map((c) => c.body).join('\n\n')) })),
  ];
  let used = 0;
  const items = candidates.map((item) => {
    const included = item.priority === 0 || used + item.tokens <= budget;
    used += included ? item.tokens : estimate(`- ${item.label}: ${item.pointer}`);
    return { ...item, included };
  });
  const token_estimate = used + 12;
  const n_docs = items.filter((i) => i.priority === 4 && i.included).length;
  const trimmed = items.filter((i) => !i.included).length;
  const tokens = token_estimate >= 1000 ? `${(token_estimate / 1000).toFixed(1)}k` : String(token_estimate);
  // Sources with docs among the task's top hits, each doc weighted 1/(60+rank) (RRF), best first.
  const suggested = new Map<string, { score: number; docs: Set<string> }>();
  search(task_title, []).slice(0, 20).forEach((h, rank) => {
    const source_id = find_doc(h.doc_id).source_id;
    const s = suggested.get(source_id) ?? { score: 0, docs: new Set() };
    if (!s.docs.has(h.doc_id)) s.score += 1 / (60 + rank);
    s.docs.add(h.doc_id);
    suggested.set(source_id, s);
  });
  const suggested_sources = [...suggested]
    .map(([source_id, s]) => ({
      source_id, name: find_source(source_id).name, score: s.score, docs: s.docs.size,
      selected: !context.sources || context.sources.includes(source_id),
    }))
    .sort((a, b) => b.score - a.score);
  const blocked = context.block_over_budget && trimmed
    ? `The context pack is over budget: ${trimmed} item(s) don't fit in ${budget} tokens. Raise the task's budget, remove sources or let Sun trim the pack.`
    : null;
  return {
    summary: `${tokens} tokens · ${n_docs} docs · ${trimmed} trimmed (pointers)`, budget_tokens: budget, token_estimate, items,
    // A blended $3 per million tokens, as if a runtime had reported its costs.
    suggested_sources, cost_estimate_usd: token_estimate * 3e-6, blocked,
  };
}

/** `knowledge/**`, `settings/**` and `tasks/:id/context**`, already split into path parts. */
export function knowledge_route(method: string, parts: string[], params: URLSearchParams, input: Record<string, unknown>): unknown {
  const at = parts[0] === 'tasks' ? 1 : 2;
  const id = parts[at];
  const key = `${method} /${parts.map((p, i) => (i === at ? ':id' : p)).join('/')}`;

  switch (key) {
    case 'GET /knowledge/sources':
      return clone(sources.filter(on_workspace(params.get('workspace'))).map(summary_of));
    case 'POST /knowledge/sources': {
      const name = String(input.name ?? '').trim();
      if (!name || name.length > 200) throw invalid('Give the source a name of 1 to 200 characters.');
      const kind = String(input.kind ?? '').trim();
      const workspace = find_workspace(String(input.workspace_id ?? MAIN_WORKSPACE)).id;
      const s = source(`src_${Date.now().toString(36)}`, kind as KnowledgeKind, name, check_settings(kind, input.settings), null);
      sources.push(s);
      links.set(s.id, new Set([workspace]));
      return clone(summary_of(s));
    }
    case 'DELETE /knowledge/sources/:id': {
      // Off this workspace; deleted with its docs once no workspace has it.
      const s = find_source(id);
      const workspaces = links.get(s.id);
      if (!workspaces?.delete(params.get('workspace') ?? MAIN_WORKSPACE)) throw not_found(`knowledge source ${id}`);
      if (workspaces.size) return null;
      sources.splice(sources.indexOf(s), 1);
      links.delete(s.id);
      for (let i = docs.length - 1; i >= 0; i--) if (docs[i].source_id === s.id) docs.splice(i, 1);
      return null;
    }
    case 'GET /knowledge/sources/:id/docs':
      return clone(docs.filter((d) => d.source_id === find_source(id).id).map(doc_view));
    case 'POST /knowledge/sources/:id/docs': {
      const s = find_source(id);
      // Binary files: the mock can't read them, so each becomes one doc (an xlsx one sheet tab).
      const files = ((input.files ?? []) as UploadFile[]).flatMap((f): UploadDoc[] => {
        const kind = f.external_id.match(/\.(xlsx|docx|pdf)$/i)?.[1].toLowerCase();
        if (!kind || !f.content_base64) return [{ ...f, content: { markdown: '' } }];
        const tab = kind === 'xlsx' ? 'Sheet1' : '';
        return [{
          ...f, external_id: tab ? `${f.external_id}#${tab}` : f.external_id, title: tab ? `${f.title} / ${tab}` : f.title,
          content: { markdown: `# ${f.title}\n\nText extracted from ${f.external_id}.` },
        }];
      });
      const upload = [...((input.docs ?? []) as UploadDoc[]), ...files];
      if (!upload.length || upload.length > 500) throw invalid('Send 1 to 500 docs at a time.');
      const report = { indexed: 0, unchanged: 0, skipped: [] as string[], retired: 0 };
      for (const u of upload) {
        const markdown = u.content?.markdown ?? '';
        if (!markdown.trim()) {
          report.skipped.push(`${u.external_id}: empty`);
          continue;
        }
        const old = docs.find((d) => d.source_id === s.id && d.external_id === u.external_id);
        if (old && old.version === u.version) {
          report.unchanged++;
          continue;
        }
        if (old) docs.splice(docs.indexOf(old), 1);
        const sections = markdown.split(/\n(?=#{1,3} )/).filter((b) => b.trim());
        docs.push({
          id: `doc_${Date.now().toString(36)}${report.indexed}`, source_id: s.id, external_id: u.external_id, title: u.title, version: u.version,
          url: null, updated_at: u.updated_at ?? null, fetched_at: new Date().toISOString(),
          chunks: sections.map((body) => ({ heading_path: body.match(/^#{1,3} (.+)/)?.[1].trim() ?? u.title, body: body.trim() })),
        });
        report.indexed++;
      }
      s.last_sync = { at: new Date().toISOString(), report, error: null };
      return report;
    }
    case 'POST /knowledge/sources/:id/reindex': {
      const s = find_source(id);
      if (s.kind === 'upload') throw invalid('`upload` sources have no connector; upload their docs again.');
      // The Jira token stays expired, so its re-index keeps failing (the error is kept on the source).
      if (s.last_sync?.error) throw new MockError(502, { error: { code: 'mod_failed', message: s.last_sync.error } });
      const report = { indexed: 0, unchanged: docs.filter((d) => d.source_id === s.id).length, skipped: [], retired: 0 };
      s.last_sync = { at: new Date().toISOString(), report, error: null };
      return report;
    }
    case 'GET /knowledge/docs/:id': {
      const d = find_doc(id);
      return clone({
        doc: doc_view(d),
        chunks: d.chunks.map((c) => ({ heading_path: c.heading_path, body: c.body, token_estimate: estimate(c.body), citation: citation(d, c) })),
      });
    }
    case 'GET /knowledge/search': {
      const q = params.get('q') ?? '';
      if (!q.trim() || q.length > 1000) throw invalid('Search for 1 to 1000 characters.');
      // Only the workspace's sources (default Main), narrowed to `sources`.
      const asked = (params.get('sources') ?? '').split(',').filter(Boolean);
      const workspace = sources.filter(on_workspace(params.get('workspace') ?? MAIN_WORKSPACE)).map((s) => s.id);
      const only = asked.length ? workspace.filter((b) => asked.includes(b)) : workspace;
      return only.length ? search(q, only).slice(0, 20) : [];
    }
    case 'GET /settings/embeddings':
      return clone(embeddings);
    case 'PUT /settings/embeddings': {
      const provider = input.provider as Embeddings['provider'];
      if (!['none', 'openai', 'ollama'].includes(provider)) throw invalid('unknown variant: use none, openai or ollama');
      const model = String(input.model ?? '').trim();
      if (provider !== 'none' && (!model || model.length > 256)) throw invalid('embedding model must be 1..=256 bytes');
      const api_key = input.api_key as string | undefined;
      embeddings = {
        provider, model, base_url: String(input.base_url ?? '').trim(), dimensions: (input.dimensions ?? null) as number | null,
        has_api_key: api_key === undefined ? embeddings.has_api_key : api_key !== '',
      };
      return clone(embeddings);
    }
    case 'GET /tasks/:id/context':
      return clone(contexts.get(find_task(id).id) ?? { sources: null, budget_tokens: null, pinned: [], block_over_budget: false });
    case 'PUT /tasks/:id/context': {
      const t = find_task(id);
      const context = check_context(input);
      contexts.set(t.id, context);
      return clone(context);
    }
    case 'POST /tasks/:id/context/preview':
      return preview(find_task(id).title, check_context(input));
  }
  throw not_found(`${method} /${parts.join('/')}`);
}
