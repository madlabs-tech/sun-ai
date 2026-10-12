// Git hosting for the mock: Connect GitHub / GitLab connect at once (no provider page) and land on the
// Code settings in a new tab like the kernel's callback, so connections live in localStorage, where that
// tab sees them; the workspace's repositories, by URL or picked from a connection. `?mock_git_apps=1`
// says the operator set the GitHub App and GitLab OAuth secrets (else they count as set once the
// ecosystem secrets exist, like the kernel).
import type { RemoteRepo, Repo, VcsConnection, VcsList, WebhookRotation } from './types';
import { MockError, REPOS, clone, find_workspace, not_found } from './mock';
import { has_ecosystem_secret } from './mock_settings';
import { repo_identity } from '../format';

const invalid = (message: string) => new MockError(422, { error: { code: 'invalid_input', message } });
let next_id = 0;
const new_id = (prefix: string) => `01JV${prefix}${String(++next_id).padStart(4, '0')}`.padEnd(26, '0');

const STORE = 'sun_mock_connections';
const connections: VcsConnection[] = (() => {
  try {
    return JSON.parse(localStorage.getItem(STORE) ?? '[]') as VcsConnection[];
  } catch {
    return [];
  }
})();
const save = () => {
  try {
    localStorage.setItem(STORE, JSON.stringify(connections));
  } catch {
    // A blocked store: this tab still works on its own.
  }
};

const REMOTE: Record<string, RemoteRepo[]> = {
  github: ['web', 'api', 'mobile', 'docs'].map((name) => ({
    name: `madlabs/${name}`,
    url: `https://github.com/madlabs/${name}.git`,
    web_url: `https://github.com/madlabs/${name}`,
    default_branch: 'main',
    description: name === 'docs' ? 'Public docs site' : '',
    private: name !== 'docs',
  })),
  gitlab: ['platform/app', 'platform/infra'].map((name) => ({
    name: `madlabs/${name}`,
    url: `https://gitlab.com/madlabs/${name}.git`,
    web_url: `https://gitlab.com/madlabs/${name}`,
    default_branch: 'main',
    description: '',
    private: true,
  })),
};

/** Like the kernel's `vcs_apps`: an app is configured when the operator set all its secrets. */
const apps = (): VcsList['apps'] => {
  if (new URLSearchParams(location.search).get('mock_git_apps') === '1') return { github: true, gitlab_oauth: true };
  const set = (names: string[]) => names.every(has_ecosystem_secret);
  return {
    github: set(['GITHUB_APP_ID', 'GITHUB_APP_SLUG', 'GITHUB_APP_PRIVATE_KEY', 'GITHUB_APP_WEBHOOK_SECRET']),
    gitlab_oauth: set(['GITLAB_OAUTH_CLIENT_ID', 'GITLAB_OAUTH_CLIENT_SECRET']),
  };
};

function connect(workspace: string, provider: 'github' | 'gitlab', instance_url: string, account_login: string): VcsConnection {
  const existing = connections.find((c) => c.workspace_id === workspace && c.provider === provider && c.instance_url === instance_url);
  if (existing) return existing;
  const connection: VcsConnection = {
    id: new_id('C'),
    workspace_id: workspace,
    provider,
    instance_url,
    account_login,
    installation_id: provider === 'github' ? 4242 : null,
    connected_by: 'user:me',
    created_at: new Date().toISOString(),
  };
  connections.push(connection);
  save();
  return connection;
}

function find_connection(id: string, workspace?: string): VcsConnection {
  const connection = connections.find((c) => c.id === id && (!workspace || c.workspace_id === workspace));
  if (!connection) throw not_found('git connection');
  return connection;
}

/** Paths this module answers. */
export function is_vcs_route(method: string, parts: string[]): boolean {
  if (parts[0] === 'vcs') return true;
  if (parts[0] === 'repos') return method === 'PATCH' && parts.length === 2;
  return parts[0] === 'workspaces' && (parts[2] === 'vcs' || parts[2] === 'repos');
}

export function vcs_route(method: string, parts: string[], params: URLSearchParams, input: Record<string, unknown>): unknown {
  if (parts[0] === 'vcs' && parts[1] === 'connections') {
    const connection = find_connection(parts[2]);
    if (method === 'DELETE' && parts.length === 3) {
      connections.splice(connections.indexOf(connection), 1);
      save();
      return null;
    }
    if (method === 'POST' && parts[3] === 'rotate-webhook') {
      if (connection.provider !== 'gitlab') throw invalid("GitHub webhooks are signed with the App's secret: rotate it on GitHub.");
      const hooked = REPOS.filter((r) => r.workspace_id === connection.workspace_id && repo_identity(r.url)?.startsWith('gitlab.com/'));
      return { updated: hooked.length } satisfies WebhookRotation;
    }
  }
  if (parts[0] === 'repos') {
    const repo = REPOS.find((r) => r.id === parts[1]);
    if (!repo) throw not_found('repo');
    if (typeof input.description === 'string') repo.description = input.description.trim();
    if (typeof input.default_branch === 'string' && input.default_branch.trim()) repo.default_branch = input.default_branch.trim();
    return clone(repo);
  }
  const workspace = find_workspace(parts[1]).id;
  if (parts[2] === 'repos') {
    if (method === 'GET' && parts.length === 3) return clone(REPOS.filter((r) => r.workspace_id === workspace));
    if (method === 'POST' && parts.length === 3) {
      const url = String(input.url ?? '').trim();
      const identity = repo_identity(url);
      if (!identity) throw invalid('Use a git clone URL: https://host/org/repo.git or git@host:org/repo.git.');
      if (REPOS.some((r) => r.workspace_id === workspace && repo_identity(r.url) === identity)) throw invalid('This repository is already added.');
      const repo: Repo = {
        id: new_id('R'),
        workspace_id: workspace,
        url,
        default_branch: String(input.default_branch ?? '').trim() || 'main',
        description: String(input.description ?? '').trim(),
      };
      REPOS.push(repo);
      return clone(repo);
    }
    if (method === 'DELETE' && parts.length === 4) {
      const at = REPOS.findIndex((r) => r.id === parts[3] && r.workspace_id === workspace);
      if (at < 0) throw not_found('repo');
      REPOS.splice(at, 1);
      return null;
    }
    throw not_found('route');
  }
  // /workspaces/:id/vcs[/...]
  const [, , , what, action] = parts;
  if (method === 'GET' && !what) return clone({ connections: connections.filter((c) => c.workspace_id === workspace), apps: apps(), can_manage: true } satisfies VcsList);
  if (method === 'GET' && action === 'connect' && (what === 'github' || what === 'gitlab')) {
    if (!(what === 'github' ? apps().github : apps().gitlab_oauth)) return { url: null, configured: false };
    connect(workspace, what, what === 'github' ? 'https://github.com' : 'https://gitlab.com', 'madlabs');
    const return_to = params.get('return_to');
    return { url: `#/settings/code?workspace=${workspace}&${what}_connected=1${return_to ? `&return_to=${return_to}` : ''}`, configured: true };
  }
  if (method === 'GET' && action === 'repos') {
    const connection = find_connection(what, workspace);
    const q = (params.get('q') ?? '').toLowerCase();
    return clone(REMOTE[connection.provider].filter((r) => r.name.toLowerCase().includes(q)));
  }
  throw not_found('route');
}
