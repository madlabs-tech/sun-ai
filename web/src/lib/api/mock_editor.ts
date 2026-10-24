// The repo editor for the mock (crates/sun-api/src/editor.rs): one sample worktree per repo, kept in
// localStorage so a saved edit survives a reload like the host's worktree does. Every host shows the
// same worktree. `?mock_role=viewer` may only read.
import type { EditorCapability, FsEntry, FsFile, FsStat } from './types';
import { MockError, REPOS, not_found } from './mock';

const MAX_FILE = 2 * 1024 * 1024;
const STORE = 'sun_mock_editor';

/** Path → a file's text, `null` for a directory, or the byte size of a file too large to hold. */
type Tree = Record<string, string | null | number>;

const seed = (): Tree => ({
  'README.md': '# Sample repo\n\nPick a file on the left to edit it.\n',
  src: null,
  'src/main.ts': "import { greet } from './greet';\n\nconsole.log(greet('Sun'));\n",
  'src/greet.ts': 'export const greet = (name: string) => `Hello, ${name}`;\n',
  'src/big.ts': Array.from({ length: 10_000 }, (_, i) => `export const line_${i} = ${i};`).join('\n') + '\n',
  'src/server.go': 'package main\n\nimport "fmt"\n\nfunc main() {\n\tfmt.Println("hello")\n}\n',
  'package.json': '{\n  "name": "sample",\n  "private": true\n}\n',
  'logo.png': '\u0089PNG\r\n\u001a\n\u0000\u0000',
  'dump.sql': 3 * 1024 * 1024,
});

function load(): Record<string, Tree> {
  try {
    return JSON.parse(localStorage.getItem(STORE) ?? '{}') as Record<string, Tree>;
  } catch {
    return {};
  }
}
let trees = load();
// A save in another tab changes "the disk" here too, as on a shared host.
addEventListener('storage', (e) => e.key === STORE && (trees = load()));
const save = () => {
  try {
    localStorage.setItem(STORE, JSON.stringify(trees));
  } catch {
    // A blocked or full store: edits last until the reload.
  }
};

const fail = (status: number, code: string, message: string) => new MockError(status, { error: { code, message } });
const invalid = (message: string) => fail(422, 'invalid_input', message);
const parent_of = (path: string) => (path.includes('/') ? path.slice(0, path.lastIndexOf('/')) : '');
const under = (path: string, root: string) => path === root || path.startsWith(root + '/');

/** Not cryptographic like the host's SHA-256: the mock only compares it. */
function hash_of(text: string): string {
  let h = 0x811c9dc5;
  for (let i = 0; i < text.length; i++) h = Math.imul(h ^ text.charCodeAt(i), 0x01000193);
  return (h >>> 0).toString(16).padStart(8, '0');
}

const stat_of = (text: string): FsStat => ({ size: new Blob([text]).size, mtime_ms: Date.now(), hash: hash_of(text) });

function file_of(value: string | number): FsFile {
  if (typeof value === 'number') return { size: value, mtime_ms: Date.now(), hash: null, too_large: true, binary: false, content: null };
  const binary = value.includes('\u0000');
  return { ...stat_of(value), too_large: false, binary, content: binary ? null : value };
}

/** Like the host: repo-relative and `/`-separated, never leaving the worktree. */
function checked(path: string): string {
  if (path.startsWith('/') || path.split('/').some((part) => part === '..' || part === '.' || (path && !part)))
    throw invalid(`\`${path}\` is not a path inside the repository.`);
  return path;
}

function capabilities(): EditorCapability[] {
  return new URLSearchParams(location.search).get('mock_role') === 'viewer' ? ['editor.read'] : ['editor.read', 'editor.write', 'terminal'];
}

function need_write() {
  if (!capabilities().includes('editor.write'))
    throw fail(403, 'forbidden', "You don't have `editor.write` on this repository. Ask a workspace owner or admin to grant it.");
}

/** `/workspaces/:id/repos/:repo/editor/...`. */
export function editor_route(method: string, parts: string[], params: URLSearchParams, body: unknown, headers: Record<string, string> = {}): unknown {
  const [, workspace, , repo_id, , what] = parts;
  if (!REPOS.some((r) => r.id === repo_id && r.workspace_id === workspace)) throw not_found(`repo ${repo_id}`);
  if (what === 'me') return { capabilities: capabilities() };
  if (!params.get('host_id')) throw invalid('Pass ?host_id=<host id>.');
  const tree = (trees[repo_id] ??= seed());
  const path = checked(params.get('path') ?? '');
  const input = (body ?? {}) as Record<string, string>;
  const is_dir = (p: string) => p === '' || tree[p] === null;

  switch (`${method} ${what}`) {
    case 'POST open':
      return { root: `/home/sun/editor/${repo_id}` };
    case 'GET tree': {
      if (!is_dir(path)) throw invalid(`\`${path}\` is not a directory.`);
      const entries: FsEntry[] = Object.entries(tree)
        .filter(([p]) => p !== path && parent_of(p) === path)
        .map(([p, value]) => ({
          name: p.slice(p.lastIndexOf('/') + 1),
          kind: value === null ? 'dir' : 'file',
          size: typeof value === 'number' ? value : value === null ? 0 : new Blob([value]).size,
          collapsed: false,
        }));
      entries.sort((a, b) => Number(a.kind !== 'dir') - Number(b.kind !== 'dir') || (a.name < b.name ? -1 : 1));
      return { entries, truncated: false };
    }
    case 'GET file': {
      const value = tree[path];
      if (value === undefined || value === null) throw not_found(`file ${path}`);
      return file_of(value);
    }
    case 'PUT file': {
      need_write();
      const text = String(body ?? '');
      if (new Blob([text]).size > MAX_FILE) throw fail(413, 'too_large', 'The editor saves files of at most 2 MiB.');
      if (is_dir(path)) throw invalid(`\`${path}\` is a directory.`);
      const value = tree[path];
      const expected = headers['if-match'];
      if (expected && (typeof value !== 'string' || hash_of(value) !== expected)) {
        const current = value === undefined ? null : typeof value === 'string' ? stat_of(value) : { size: value, mtime_ms: Date.now(), hash: '' };
        throw new MockError(409, { error: { code: 'conflict', message: 'The file changed since you opened it.' }, current } as never);
      }
      if (!is_dir(parent_of(path))) throw invalid(`\`${parent_of(path)}\` is not a directory.`);
      tree[path] = text;
      save();
      return stat_of(text);
    }
    case 'POST file': {
      need_write();
      if (!path) throw invalid('Name the new file or folder.');
      if (tree[path] !== undefined) throw invalid(`\`${path}\` already exists.`);
      if (!is_dir(parent_of(path))) throw invalid(`\`${parent_of(path)}\` is not a directory.`);
      tree[path] = input.kind === 'dir' ? null : '';
      save();
      return null;
    }
    case 'DELETE file': {
      need_write();
      if (!path || tree[path] === undefined) throw not_found(`file ${path}`);
      for (const p of Object.keys(tree)) if (under(p, path)) delete tree[p];
      save();
      return null;
    }
    case 'POST rename': {
      need_write();
      const [from, to] = [checked(input.from ?? ''), checked(input.to ?? '')];
      if (!from || tree[from] === undefined) throw not_found(`file ${from}`);
      if (!to || tree[to] !== undefined) throw invalid(`\`${to}\` already exists.`);
      if (!is_dir(parent_of(to)) || under(to, from)) throw invalid(`\`${parent_of(to)}\` is not a directory.`);
      for (const p of Object.keys(tree)) {
        if (!under(p, from)) continue;
        tree[to + p.slice(from.length)] = tree[p];
        delete tree[p];
      }
      save();
      return null;
    }
  }
  throw not_found(`${method} editor/${what}`);
}
