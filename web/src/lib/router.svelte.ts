// Hash router: #/<page>/<id>/<sub>. Hash routing keeps the static build servable from
// rust-embed with no server-side fallback route.
import { feature_on, is_owner } from './app_state.svelte';

/** The sidebar section an item sits in: none (above the groups), a named group, or the footer. */
export type NavSection = '' | 'Work' | 'AI Team' | 'footer';

export interface NavItem {
  id: string;
  label: string;
  icon: string;
  section: NavSection;
  /** The deployment's own page (its reads are owner-only until they are per workspace): deployment owners only. */
  owner?: true;
}

/** The sidebar shows these by section, in this order within each. The first TAB_COUNT (Tasks, Approvals,
 *  Agents) are also the phone's bottom tabs; the rest live under "More". */
export const NAV: NavItem[] = [
  { id: 'tasks', label: 'Tasks', icon: 'kanban', section: 'Work' },
  { id: 'approvals', label: 'Approvals', icon: 'user-check', section: '' },
  { id: 'agents', label: 'Agents', icon: 'bot', section: 'AI Team' },
  { id: 'projects', label: 'Projects', icon: 'folder-kanban', section: 'Work' },
  { id: 'workflows', label: 'Workflows', icon: 'workflow', section: 'Work' },
  { id: 'knowledge', label: 'Knowledge', icon: 'library', section: 'Work' },
  { id: 'memory', label: 'Memory', icon: 'lightbulb', section: 'Work' },
  { id: 'evals', label: 'Evals', icon: 'flask-conical', section: 'Work' },
  { id: 'runtimes', label: 'Runtimes', icon: 'cpu', section: 'AI Team' },
  { id: 'hosts', label: 'Hosts', icon: 'server', section: 'AI Team' },
  { id: 'catalog', label: 'Catalog', icon: 'sparkles', section: 'AI Team' },
  { id: 'mods', label: 'Mods', icon: 'puzzle', section: 'AI Team', owner: true },
  { id: 'market', label: 'Marketplace', icon: 'store', section: 'AI Team', owner: true },
  { id: 'settings', label: 'Settings', icon: 'settings', section: 'footer' },
];

export const TAB_COUNT = 3;

/** True when page `id` shows: its feature is on, and a deployment page only for deployment owners. */
export const page_on = (id: string) => feature_on(id) && (!NAV.find((n) => n.id === id)?.owner || is_owner());

/** NAV without the pages of features the kernel turned off (`knowledge`, `evals`, `market`, `catalog`), and
 *  without the deployment's pages for anyone but its owners. */
export const visible_nav = () => NAV.filter((n) => page_on(n.id));

/** `approve` is the notification deep link: #/approve/<run_id>/<step_key>.
 *  `canvas` is a task's design canvas: #/canvas/<task_id>. `invite` is an invitation link: #/invite/<token>.
 *  `billing` is where Sun Cloud's hosted checkout returns: #/billing/return?status=success|canceled&workspace=<id>. */
const PAGES = new Set([...NAV.map((n) => n.id), 'more', 'login', 'approve', 'canvas', 'invite', 'billing']);

/** How a git connect came back: `github_connected=1` / `github_error=<reason>` (same for `gitlab`), with
 *  `return_to=repositories` when the picker should open. Taken out of the URL here, shown and cleared by
 *  the Code settings page. `outcome` is `connected` or the error reason. */
export const vcs_landing = $state({ provider: '' as '' | 'github' | 'gitlab', outcome: '', return_to: '' });

function parse() {
  // Members moved into Settings: an old #/members link becomes #/settings/members, in place of its history entry.
  const moved = location.hash.replace(/^#\/?members(?=[/?]|$)/, '#/settings/members');
  if (moved !== location.hash) history.replaceState(null, '', moved);
  // `?workspace=<slug>` after the path keeps the current workspace in the URL (an id still works: it is
  // rewritten to the slug once the workspaces load).
  const [path, search = ''] = location.hash.replace(/^#\/?/, '').split('?');
  const params = new URLSearchParams(search);
  // The connect landing params are one-shot: taken out of the URL so a reload doesn't repeat them.
  let landed = false;
  const take = (name: string) => {
    const value = params.get(name);
    if (value !== null) {
      params.delete(name);
      landed = true;
    }
    return value;
  };
  for (const provider of ['github', 'gitlab'] as const) {
    const [ok, error] = [take(`${provider}_connected`), take(`${provider}_error`)];
    if (ok === null && error === null) continue;
    Object.assign(vcs_landing, { provider, outcome: error || (ok === '1' ? 'connected' : 'failed'), return_to: take('return_to') ?? '' });
  }
  if (landed) {
    const rest = params.toString();
    history.replaceState(null, '', `#/${path}${rest ? `?${rest}` : ''}`);
  }
  // Old links say `?board=` (and `#/board`, which falls back to tasks): rewrite them, keeping the other params.
  const legacy = params.get('board');
  if (legacy && !params.has('workspace')) {
    params.delete('board');
    params.set('workspace', legacy);
    history.replaceState(null, '', `#/${path}?${params}`);
  }
  const workspace = params.get('workspace') ?? legacy;
  const [page = 'tasks', id = null, sub = null] = path
    .split('/')
    .filter(Boolean)
    .map(decodeURIComponent);
  return PAGES.has(page) ? { page, id, sub, workspace } : { page: 'tasks', id: null, sub: null, workspace };
}

export const route = $state(parse());

addEventListener('hashchange', () => Object.assign(route, parse()));

/** A link inside the current workspace. */
export const href = (...parts: string[]) =>
  '#/' + parts.map(encodeURIComponent).join('/') + (route.workspace ? `?workspace=${encodeURIComponent(route.workspace)}` : '');

export const go = (...parts: string[]) => (location.hash = href(...parts));

/** Like `go`, in place of the current history entry (e.g. an old task link rewritten to its key). */
export function replace(...parts: string[]) {
  history.replaceState(null, '', href(...parts));
  Object.assign(route, parse());
}

/** Shows workspace `slug` on the current page, keeping its other params (a new history entry unless `replace`). */
export function show_workspace(slug: string, replace = false) {
  const [hash, search = ''] = location.hash.split('?');
  const params = new URLSearchParams(search);
  params.set('workspace', slug);
  const next = `${hash || '#/tasks'}?${params}`;
  if (replace) history.replaceState(null, '', next);
  else location.hash = next;
  Object.assign(route, parse());
}
