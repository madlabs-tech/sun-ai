// Shared UI state: workspace tasks, runtimes and credentials (sidebar + cards) and toasts.
import { api, ApiFailure } from './api/client';
import { tick } from 'svelte';
import { auth, type InvitationView, type Me, type Membership, type Permission, type Providers } from './api/auth';
import { PERMISSIONS_OF } from './role';
import type { Agent, Credential, Workspace, Features, Runtime, TaskSummary } from './api/types';
import { route, show_workspace } from './router.svelte';

const ROLE_KEY = 'sun_role';
/** The workspace this browser showed last. */
const WORKSPACE_KEY = 'sun_workspace';

export const app = $state({
  /** Every workspace; the current one is `route.workspace` (its slug, in the URL). */
  workspaces: [] as Workspace[],
  /** The signed-in person's workspaces and role in each; with `me.is_owner`, what `can` answers. */
  memberships: [] as Membership[],
  /** Their pending invitations (the workspace menu's badge, onboarding). */
  invitations: [] as InvitationView[],
  /** How this deployment lets people in (`GET /auth`): sign-up and workspace creation; `null` until loaded. */
  policy: null as Providers | null,
  /** The workspace whose tasks `tasks` holds. */
  workspace: '',
  /** That workspace's tasks. */
  tasks: [] as TaskSummary[],
  runtimes: [] as Runtime[],
  /** The workspace's credentials (key-backed chain entries): where a chain name isn't a host's tool. */
  credentials: [] as Credential[],
  /** The active agents (names, readiness): assignee pickers, cards and swimlanes. */
  agents: [] as Agent[],
  loaded: false,
  error: '',
  /** The kernel answered 401: show the token form instead of the app. */
  signed_out: false,
  /** What the signed-in person must do before anything else: give their email (the owner at first
   *  run, or a member from before invites), set a password (after an invite-code sign-in), or add a
   *  second sign-in step (the ecosystem requires one). */
  gate: '' as '' | 'email' | 'password' | 'two_step',
  /** Built-in features the kernel runs; `null` until loaded, when everything shows. */
  features: null as Features | null,
  /** Who is signed in (account menu); `null` until the first load. */
  me: null as Me | null,
  /** The desktop sidebar is hidden (the toggle or ⌘B / Ctrl+B); not kept across visits. */
  sidebar_collapsed: false,
  /** The role view picked at onboarding or on Workflows, kept per member by the kernel and cached in this browser; '' until picked. */
  role: local_get(ROLE_KEY),
});

/** True when the signed-in person may `permission` in `workspace` (the open one by default): a deployment owner
 *  always, anyone else when their role there grants it. Hides controls they can't use; the kernel decides. */
export function can(permission: Permission, workspace = current_workspace()): boolean {
  if (app.me?.is_owner) return true;
  const role = app.memberships.find((m) => m.workspace_id === workspace)?.role;
  return !!role && PERMISSIONS_OF[role].includes(permission);
}

/** True for a deployment owner: the deployment's own pages (runtimes, hosts, mods, its settings) are theirs alone. */
export const is_owner = () => !!app.me?.is_owner;

/** Re-reads the signed-in person's workspaces, roles and invitations (after joining or leaving one). */
export async function load_memberships() {
  const [me, invitations] = await Promise.all([auth.me(), auth.my_invitations().catch(() => [])]);
  app.me = me;
  app.memberships = me.memberships ?? [];
  app.invitations = invitations;
}

/** True when the signed-in person may create a workspace here. */
export const can_create_workspace = () => is_owner() || !app.policy?.workspace_creation_disabled;

/** Roles whose view hides repos, branches and runtimes unless a task uses them. */
const PLAIN_ROLES = ['marketing', 'product', 'leadership'];

/** The plain-language view: on for a non-technical role, for tasks with no repo. */
export const plain_view = (repo_count = 0) => PLAIN_ROLES.includes(app.role) && repo_count === 0;

/** Picks the role view for this member (the kernel keeps it, so it follows them); '' clears it. */
export function set_role(role: string): Promise<unknown> {
  app.role = role;
  local_set(ROLE_KEY, role);
  return auth.set_view_role(role || null);
}

/** Takes the member's role view from the kernel; a pick made only in this browser (by an older UI) is carried over. */
async function load_view_role() {
  const me = await auth.me().catch(() => null);
  if (!me || me.view_role === undefined) return; // An older kernel: the browser's pick stays.
  if (me.view_role) {
    app.role = me.view_role;
    local_set(ROLE_KEY, me.view_role);
  } else if (app.role) {
    auth.set_view_role(app.role).catch(() => {});
  }
}
let view_role_loaded = false;

/** localStorage, which a private window may refuse: then a value lasts for this visit only. */
export function local_get(key: string): string {
  try {
    return localStorage.getItem(key) ?? '';
  } catch {
    return '';
  }
}

export function local_set(key: string, value: string) {
  try {
    localStorage.setItem(key, value);
  } catch {
    // Private mode: kept in memory for this visit.
  }
}

/** Whether a page or panel of `feature` should show: off only once the kernel said so. */
export const feature_on = (feature: string) => app.features?.[feature as keyof Features] !== false;

export function error_text(e: unknown): string {
  if (e instanceof ApiFailure) return e.message;
  return "Can't reach the kernel. Start sun, or add ?mock=1 to the URL to try the UI with sample data.";
}

/** The gate is read once per sign-in; the set-email and set-password forms clear it. */
let gate_checked = false;

/** Re-reads what the signed-in person must do first, then loads the workspace. */
export function recheck_gate() {
  gate_checked = false;
  return load_workspace();
}

/** The workspace a URL or stored reference names: its slug, or its id (older links). */
const workspace_by_ref = (ref: string | null) => (ref ? app.workspaces.find((w) => w.slug === ref || w.id === ref) : undefined);

/** The workspace to show: the URL's, else the one shown last, else the first (Main). Its slug goes in the
 *  URL (an id link is rewritten in place); resolves to its id. */
function pick_workspace(): string {
  const workspace = workspace_by_ref(route.workspace) ?? workspace_by_ref(local_get(WORKSPACE_KEY)) ?? app.workspaces[0];
  if (!workspace) return '';
  if (workspace.slug !== route.workspace) show_workspace(workspace.slug, true);
  local_set(WORKSPACE_KEY, workspace.id);
  return workspace.id;
}

/** The current workspace's id ('' before the workspaces load). A link without `?workspace=` stays on the one
 *  shown (the URL gets it back), so a page it opens never lists with no workspace (an owner's: every one's). */
export const current_workspace = () => workspace_by_ref(route.workspace)?.id ?? (route.workspace ? '' : app.workspace);

/** The current workspace, once the workspaces are loaded. */
export const workspace_of = (id: string) => app.workspaces.find((b) => b.id === id);

/** Puts a changed workspace (a profile edit, logo or prefix) into the list without a reload. */
export function keep_workspace(changed: Workspace) {
  const i = app.workspaces.findIndex((w) => w.id === changed.id);
  if (i >= 0) app.workspaces[i] = changed;
}

export async function load_workspace() {
  try {
    if (!gate_checked) {
      const me = await auth.me();
      app.me = me;
      app.memberships = me.memberships ?? [];
      app.invitations = me.must_set_password || me.needs_email ? [] : await auth.my_invitations().catch(() => []);
      app.gate = me.must_set_password ? 'password' : me.needs_email ? 'email' : me.must_enroll_two_step ? 'two_step' : '';
      gate_checked = !app.gate;
      if (app.gate) {
        app.signed_out = false;
        app.loaded = true;
        return;
      }
    }
    // Features change only with a kernel restart: fetched once, an older kernel without the route has them all.
    app.features ??= await api.get_features().catch(() => null);
    app.policy ??= await auth.providers().catch(() => null);
    app.workspaces = await api.list_workspaces();
    const workspace = pick_workspace();
    // Agents are advisory here: an older kernel without them shows tasks all the same.
    [app.tasks, app.runtimes, app.credentials, app.agents] = await Promise.all([
      api.list_tasks(workspace || undefined),
      // The workspace's; with no workspace yet (onboarding) there are none.
      workspace ? api.list_runtimes(workspace) : ([] as Runtime[]),
      workspace ? api.list_credentials(workspace).catch(() => app.credentials) : ([] as Credential[]),
      workspace ? api.list_agents(workspace).catch(() => app.agents) : ([] as Agent[]),
    ]);
    app.workspace = workspace;
    app.error = '';
    app.signed_out = false;
    if (!view_role_loaded) {
      view_role_loaded = true;
      load_view_role();
    }
  } catch (e) {
    app.error = error_text(e);
    app.signed_out = e instanceof ApiFailure && e.status === 401;
    if (app.signed_out) gate_checked = false;
  }
  app.loaded = true;
}

/** Ends the session and shows sign-in; the next sign-in reloads who it is and what they may do. */
export async function sign_out() {
  await auth.sign_out().catch(() => null);
  app.gate = '';
  app.me = null;
  app.memberships = [];
  app.invitations = [];
  view_role_loaded = false;
  app.signed_out = true;
}

/** The sidebar shortcut as this platform writes it. */
export const SIDEBAR_SHORTCUT = /Mac|iPhone|iPad/.test(navigator.userAgent) ? '⌘B' : 'Ctrl+B';

/** Shows or hides the desktop sidebar. Focus on a toggle (or in the hidden sidebar) moves to the toggle still shown. */
export async function toggle_sidebar() {
  const refocus = document.activeElement?.closest('.sun-sidebar, [data-sidebar-toggle]');
  app.sidebar_collapsed = !app.sidebar_collapsed;
  if (!refocus) return;
  await tick();
  [...document.querySelectorAll<HTMLElement>('[data-sidebar-toggle]')].find((el) => el.checkVisibility())?.focus();
}

export interface ToastItem {
  id: number;
  tone: 'info' | 'success' | 'warning' | 'danger';
  title: string;
  description?: string;
}

export const toasts = $state<ToastItem[]>([]);
let toast_seq = 0;

export function toast(tone: ToastItem['tone'], title: string, description?: string) {
  const id = ++toast_seq;
  toasts.push({ id, tone, title, description });
  setTimeout(() => dismiss(id), 5200);
}

export function dismiss(id: number) {
  const i = toasts.findIndex((t) => t.id === id);
  if (i >= 0) toasts.splice(i, 1);
}

/** Copies `text` and says `done` (e.g. "Slug copied"); a blocked clipboard asks to copy it by hand. */
export async function copy_text(text: string, done: string) {
  try {
    await navigator.clipboard.writeText(text);
    toast('success', done);
  } catch {
    toast('warning', 'Copy it by hand', 'The browser blocked the clipboard.');
  }
}

/** Run an action, refresh the workspace, report failure as a toast. Resolves to whether it worked. */
export async function act(work: () => Promise<unknown>, done?: string): Promise<boolean> {
  let ok = true;
  try {
    await work();
    if (done) toast('success', done);
  } catch (e) {
    toast('danger', 'That didn’t go through', error_text(e));
    ok = false;
  }
  await load_workspace();
  return ok;
}

/** The human decisions every screen offers (task panel, approvals, notification sheet). */
export const approve = (t: TaskSummary) => act(() => api.approve_task(t.id), `${t.key} approved`);
export const reject = (t: TaskSummary) => act(() => api.reject_task(t.id, ''), 'Sent back to the agent');
export const retry = (run_id: string) => act(() => api.retry_run(run_id), 'Restarted with a hand-off pack');
