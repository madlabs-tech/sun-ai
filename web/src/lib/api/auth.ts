// Sign-in, sign-up, 2-step, accounts, workspace members and invitations, and the audit log. Mirrors crates/sun-api/src/auth.
// With ?mock=1 an in-memory fake answers instead of the kernel.
import { ApiFailure, request, use_mock } from './client';
import { MAIN_WORKSPACE, type MemberGroup, type TerminalPolicy } from './types';
import { PERMISSIONS_OF, covers, type Role } from '../role';

/** What a workspace role grants, per verb (`Role::permissions` in sun-core; the table is in lib/role.ts). A
 *  deployment owner has all of them, everywhere. */
export type Permission =
  | 'run'
  | 'approve'
  | 'manage_plugins'
  | 'manage_extensions'
  | 'manage_secrets'
  | 'manage_runtimes'
  | 'manage_settings'
  | 'invite_members'
  | 'create_members'
  | 'view_members'
  | 'edit_members'
  | 'deactivate_members'
  | 'edit_workspace'
  | 'delete_workspace'
  | 'create_workflow'
  | 'edit_workflow';

export interface Member {
  id: string;
  issuer: string;
  subject: string;
  username: string;
  /** Lowercase; null only for a member from before invites who hasn't given one yet. */
  email: string | null;
  /** A deployment owner: every permission in every workspace. */
  is_owner: boolean;
  /** False while an invite is pending (or after a reset code went out). */
  has_password: boolean;
  /** They must choose a new password at their next sign-in. */
  must_set_password: boolean;
  /** When an admin deactivated them; null while active. */
  deactivated_at?: string | null;
  /** They set up a second sign-in step. */
  has_two_step?: boolean;
}

/** A second sign-in step: an authenticator app, a code mailed to you, or a passkey. */
export type FactorKind = 'totp' | 'email' | 'passkey';
export interface Factor {
  id: string;
  kind: FactorKind;
  label: string;
  created_at: string;
}
/** The ecosystem's 2-step rules (Settings, `manage_settings`). */
export interface SecurityPolicy {
  require_two_step: boolean;
  two_step_for_password_change: boolean;
}
/** Your second steps, the rules, whether the kernel can mail codes, and whether passkeys work (Sun on https or
 *  localhost). */
export interface TwoStepView {
  factors: Factor[];
  policy: SecurityPolicy;
  email_ready: boolean;
  passkey_ready: boolean;
}
/** A sign-in (or password change) waiting for its second step. */
export interface TwoStepChallenge {
  challenge: string;
  methods: FactorKind[];
}
/** The answer to a challenge: a 6-digit `code`, or a passkey's `credential` (its `toJSON()`). */
export interface TwoStepAnswer {
  challenge: string;
  method: FactorKind;
  code?: string;
  credential?: unknown;
}
/** A passkey being added: hand `options` to the browser, then confirm with the `challenge`. */
export interface PasskeyStart {
  challenge: string;
  options: PublicKeyCredentialCreationOptionsJSON;
}
/** An authenticator being added: scan `qr_svg` (a data URL) or type `secret`. */
export interface TotpStart {
  id: string;
  secret: string;
  otpauth_url: string;
  qr_svg: string;
}

export type AuditAction =
  | 'login'
  | 'role_change'
  | 'group_change'
  | 'member_change'
  | 'secret_read'
  | 'secret_change'
  | 'settings_change'
  | 'mod_install'
  | 'approval'
  | 'policy_change'
  | 'trigger_failed'
  | 'workspace_change'
  | 'agent_change'
  | 'editor_grant'
  | 'editor_file'
  | 'terminal';

export interface AuditEntry {
  id: string;
  actor: string;
  action: AuditAction;
  target: string;
  detail: string;
  at: string;
}

export interface Me {
  user: string;
  /** A deployment owner: sees and may do everything, and alone sees the deployment's settings. */
  is_owner: boolean;
  /** Their workspaces and role in each (none for the token holder). */
  memberships: Membership[];
  /** They proved their email (a code or SSO). */
  email_verified?: boolean;
  /** `sso`: any sign-in provider (Google, GitHub, OIDC). */
  via: 'token' | 'sso' | 'local';
  email: string | null;
  /** Ask for the email before anything else (the first-run owner's admin@sun.local, or a member from before invites). */
  needs_email?: boolean;
  /** Signed in with a code, or an admin set the password to change: set a password before anything else. */
  must_set_password?: boolean;
  /** The ecosystem requires 2-step and they have none: add one before anything else. */
  must_enroll_two_step?: boolean;
  /** The role view this member picked, e.g. `marketing`; absent on an older kernel. */
  view_role?: string | null;
  /** They may start “Anything” (bypass) sessions: sun.yaml allows it, and they are an owner or the owner's
   *  bypass policy lets them. Otherwise their sessions run as Edit files. Absent on an older kernel. */
  can_bypass?: boolean;
  /** The terminal policy lets them open terminals (a terminal also needs its editor grant). Absent on an older kernel. */
  can_terminal?: boolean;
}

/** Settings → Security → Bypass permissions: who besides the owners may run “Anything” sessions. */
export interface BypassPolicy {
  mode: 'off' | 'members' | 'everyone';
  /** Who may under `members`. */
  member_ids: string[];
  /** The operator's sun.yaml `runtimes.allow_bypass`, the ceiling above the policy (read-only). */
  allow_bypass: boolean;
}

/** How a sign-in provider works: the Google and GitHub presets, or any OIDC provider. */
export type SsoKind = 'google' | 'github' | 'oidc';

export interface Providers {
  /** One "Continue with {name}" each. */
  providers: { id: string; name: string; kind: SsoKind }[];
  /** Members sign in with email + password (or an emailed code in the password box). */
  local?: boolean;
  /** Anyone may create an account (`POST /signup`); a pending invitation admits an email either way. */
  allow_signup?: boolean;
  /** Only deployment owners create workspaces. */
  workspace_creation_disabled?: boolean;
}

/** A sign-in provider in Settings → Sign-in; its client secret never comes back. */
export interface SsoProvider {
  id: string;
  kind: SsoKind;
  name: string;
  client_id: string;
  /** `oidc` only. */
  issuer_url?: string | null;
  /** Lowercase email domains let in; empty = any invited member. */
  allowed_domains: string[];
  has_secret: boolean;
}
export interface SsoView {
  providers: SsoProvider[];
  /** The callback to register at a provider, with `{id}` in place of its id. */
  redirect_url: string;
}
/** What `PUT /settings/sso/{id}` takes; no `client_secret` keeps the saved one. */
export type SsoInput = Omit<SsoProvider, 'id' | 'has_secret'> & { client_secret?: string };

export type SmtpTls = 'starttls' | 'tls';
export interface SmtpSettings {
  host: string;
  port: number;
  username: string;
  from: string;
  tls: SmtpTls;
}
/** The mail server invites go through; the password never comes back. */
export interface SmtpView {
  smtp: SmtpSettings | null;
  has_password: boolean;
}

// ---------- workspace members, invitations and sign-up ----------

/** A member's role in one workspace (`GET /me` lists theirs). */
export interface Membership {
  workspace_id: string;
  member_id: string;
  role: Role;
  /** When they joined it. */
  created_at: string;
}
/** One member of a workspace, with who they are (`GET /workspaces/{id}/members`, oldest first). */
export interface WorkspaceMember extends Membership {
  username: string;
  email: string | null;
}
/** A pending invitation, as the workspace's admins see it. */
export interface Invitation {
  id: string;
  workspace_id: string;
  email: string;
  role: Role;
  inviter_id: string | null;
  created_at: string;
  expires_at: string;
  accepted_at: string | null;
  declined_at: string | null;
}
/** An invitation as its invitee sees it: the link's page and `GET /me/invitations`. */
export interface InvitationView {
  id: string;
  workspace_id: string;
  workspace_name: string;
  email: string;
  role: Role;
  /** The inviter's username; null for the kernel token holder or a removed member. */
  inviter: string | null;
  expires_at: string;
}
/** What inviting answers: `link` only when the deployment can't send mail, for the admin to pass on. */
export interface Invited {
  invitation: Invitation;
  mailed: boolean;
  link: string | null;
}

const path_of = (...parts: string[]) => parts.map(encodeURIComponent).join('/');

/** Where the browser goes to sign in with provider `id`. */
export const sso_login_url = (id: string) => `/api/v1/auth/sso/${encodeURIComponent(id)}/login`;

const call = <T>(method: string, path: string, body?: unknown): Promise<T> =>
  use_mock ? Promise.resolve().then(() => fake(method, path, body) as T) : request<T>(method, path, body);

export const auth = {
  providers: () => call<Providers>('GET', '/auth'),
  /** `password` is the password or the 6-digit code from an invite or reset mail. A right password that needs a
   *  second step answers `{two_step}` instead of signing in. */
  sign_in: (email: string, password: string) => call<{ two_step: TwoStepChallenge } | null>('POST', '/session', { email, password }),
  /** Answers the second step; sets the session. */
  sign_in_two_step: (answer: TwoStepAnswer) => call<null>('POST', '/session/two_step', answer),
  /** Mails a code for a challenge (sign-in or password change). */
  mail_two_step_code: (challenge: string) => call<null>('POST', '/session/two_step/email', { challenge }),
  /** `two_step`: when the ecosystem asks for your second step to change a password. */
  set_password: (password: string, current_password?: string, two_step?: TwoStepAnswer) =>
    call<null>('PUT', '/me/password', { password, ...(current_password ? { current_password } : {}), ...(two_step ? { two_step } : {}) }),
  my_two_step: () => call<TwoStepView>('GET', '/me/two_step'),
  start_totp: (label: string) => call<TotpStart>('POST', '/me/two_step/totp', { label }),
  confirm_totp: (id: string, code: string) => call<Factor>('POST', '/me/two_step/totp/confirm', { id, code }),
  add_email_two_step: () => call<Factor>('POST', '/me/two_step/email'),
  start_passkey: () => call<PasskeyStart>('POST', '/me/two_step/passkey'),
  confirm_passkey: (challenge: string, credential: unknown, label: string) =>
    call<Factor>('POST', '/me/two_step/passkey/confirm', { challenge, credential, label }),
  /** The options to sign a challenge (sign-in or password change) with a passkey. */
  passkey_options: (challenge: string) =>
    call<{ options: PublicKeyCredentialRequestOptionsJSON }>('POST', '/session/two_step', { challenge, method: 'passkey' }),
  remove_two_step: (id: string) => call<null>('DELETE', `/me/two_step/${encodeURIComponent(id)}`),
  /** A challenge to confirm a password change with. */
  my_two_step_challenge: () => call<TwoStepChallenge>('POST', '/me/two_step/challenge'),
  /** `edit_members`: removes every second step of a member (they lost their phone). */
  reset_two_step: (id: string) => call<Member>('DELETE', `/members/${encodeURIComponent(id)}/two_step`),
  get_security: () => call<SecurityPolicy>('GET', '/settings/security'),
  set_security: (policy: SecurityPolicy) => call<SecurityPolicy>('PUT', '/settings/security', policy),
  get_bypass: () => call<BypassPolicy>('GET', '/settings/bypass'),
  set_bypass: (policy: Pick<BypassPolicy, 'mode' | 'member_ids'>) => call<BypassPolicy>('PUT', '/settings/bypass', policy),
  /** Deployment owners: every group with its hand-added `member_ids`, by name. 422 for a bad or taken name. */
  list_groups: () => call<MemberGroup[]>('GET', '/groups'),
  create_group: (name: string, description: string) => call<MemberGroup>('POST', '/groups', { name, description }),
  update_group: (id: string, change: Partial<Pick<MemberGroup, 'name' | 'description' | 'domains'>>) =>
    call<MemberGroup>('PATCH', `/groups/${path_of(id)}`, change),
  /** Its editor grants go with it and the terminal policy stops listing it. */
  delete_group: (id: string) => call<null>('DELETE', `/groups/${path_of(id)}`),
  add_group_member: (id: string, member: string) => call<null>('POST', `/groups/${path_of(id, 'members', member)}`),
  /** 404 unless they were added by hand. */
  remove_group_member: (id: string, member: string) => call<null>('DELETE', `/groups/${path_of(id, 'members', member)}`),
  /** The active members whose verified email is on one of `domains` now. */
  group_preview: (id: string, domains: string[]) =>
    call<Member[]>('GET', `/groups/${path_of(id, 'preview')}?${new URLSearchParams({ domains: domains.join(',') })}`),
  /** Any member: the groups they are in now. */
  my_groups: () => call<Omit<MemberGroup, 'member_ids'>[]>('GET', '/groups/mine'),
  get_terminal: () => call<TerminalPolicy>('GET', '/settings/terminal'),
  set_terminal: (policy: TerminalPolicy) => call<TerminalPolicy>('PUT', '/settings/terminal', policy),
  set_email: (email: string) => call<{ email: string }>('PUT', '/me/email', { email }),
  /** Deployment owner: an account in `workspace_id` as `role`. With `password`, no mail goes out (and SMTP isn't
   *  needed); `must_change` forces a change at first sign-in. */
  invite: (email: string, username: string, workspace_id: string, role: Role, password?: { password: string; must_change: boolean }) =>
    call<Member>('POST', '/members', { email, username, workspace_id, role, ...password }),
  /** Emails a reset code: their password is cleared and their sessions end. */
  reinvite: (id: string) => call<Member>('POST', `/members/${encodeURIComponent(id)}/invite`),
  set_member_password: (id: string, password: string, must_change: boolean) =>
    call<Member>('PUT', `/members/${encodeURIComponent(id)}/password`, { password, must_change }),
  deactivate: (id: string) => call<Member>('POST', `/members/${encodeURIComponent(id)}/deactivate`),
  activate: (id: string) => call<Member>('POST', `/members/${encodeURIComponent(id)}/activate`),
  get_smtp: () => call<SmtpView>('GET', '/smtp'),
  set_smtp: (settings: SmtpSettings & { password?: string }) => call<SmtpView>('PUT', '/smtp', settings),
  test_smtp: () => call<{ sent_to: string }>('POST', '/smtp/test'),
  list_sso: () => call<SsoView>('GET', '/settings/sso'),
  put_sso: (id: string, input: SsoInput) => call<SsoView>('PUT', `/settings/sso/${encodeURIComponent(id)}`, input),
  delete_sso: (id: string) => call<null>('DELETE', `/settings/sso/${encodeURIComponent(id)}`),
  sign_out: () => call<null>('DELETE', '/session'),
  me: () => call<Me>('GET', '/me'),
  set_view_role: (view_role: string | null) => call<{ view_role: string | null }>('PUT', '/me/view_role', { view_role }),
  list_members: () => call<Member[]>('GET', '/members'),
  set_owner: (id: string, is_owner: boolean) => call<Member>('PUT', `/members/${encodeURIComponent(id)}/owner`, { is_owner }),
  list_audit: () => call<AuditEntry[]>('GET', '/audit'),
  /** Mails a sign-up code (then `sign_in(email, code)`); 409 when the email already has an account. */
  signup: (email: string) => call<{ email: string }>('POST', '/signup', { email }),
  workspace_members: (workspace: string) => call<WorkspaceMember[]>('GET', `/workspaces/${path_of(workspace)}/members`),
  /** `edit_members`: never above your own role, nor on someone above it; 409 when it leaves no owner. */
  set_role: (workspace: string, member: string, role: Role) =>
    call<Membership>('PUT', `/workspaces/${path_of(workspace, 'members', member)}`, { role }),
  remove_member: (workspace: string, member: string) => call<null>('DELETE', `/workspaces/${path_of(workspace, 'members', member)}`),
  /** 409 for its last owner. */
  leave: (workspace: string) => call<null>('POST', `/workspaces/${path_of(workspace)}/leave`),
  /** `invite_members`: the pending ones, newest first. */
  invitations: (workspace: string) => call<Invitation[]>('GET', `/workspaces/${path_of(workspace)}/invitations`),
  /** `invite_members`, `role` at most your own; replaces a pending one to the same email. */
  invite_to: (workspace: string, email: string, role: Role) =>
    call<Invited>('POST', `/workspaces/${path_of(workspace)}/invitations`, { email, role }),
  revoke_invitation: (workspace: string, id: string) => call<null>('DELETE', `/workspaces/${path_of(workspace, 'invitations', id)}`),
  my_invitations: () => call<InvitationView[]>('GET', '/me/invitations'),
  /** 403 unless it is to your verified email; 404 once used, declined, revoked or expired. */
  accept_invitation: (id: string) => call<Membership>('POST', `/invitations/${path_of(id, 'accept')}`),
  decline_invitation: (id: string) => call<null>('POST', `/invitations/${path_of(id, 'decline')}`),
  /** No sign-in needed: the link's token is the credential. 404 when it no longer works. */
  look_up_invitation: (token: string) => call<InvitationView>('GET', `/invitations/${path_of(token)}`),
};

// ---------- in-memory fake (?mock=1) ----------

const ISSUER = 'https://sso.example.com/realms/acme';
const person = (id: string, subject: string, username: string): Member => ({
  id, issuer: ISSUER, subject, username, email: `${username}@acme.io`, is_owner: false, has_password: true, must_set_password: false,
});
const mock_params = new URLSearchParams(location.search);
// `?mock_role=<viewer|member|admin>`: sign in as someone with that role in Main instead of the deployment owner;
// `none`: someone in no workspace yet (onboarding).
const mock_role = mock_params.get('mock_role') as Role | 'none' | null;
const me_owner = !mock_role || mock_role === 'owner';
// `?mock_email=<email>`: the signed-in person's email. `?mock_team=1`: also dee@madlabs.tech (verified) and
// eli@madlabs.tech (not verified yet), for groups by email domain.
let members: Member[] = [
  { ...person('m_me', 's0', 'me'), is_owner: me_owner, email: mock_params.get('owner_email') === '0' ? null : (mock_params.get('mock_email') ?? 'me@acme.io') },
  person('m_ana', 's1', 'ana'),
  person('m_ben', 's2', 'ben'),
  person('m_cho', 's3', 'cho'),
  ...(mock_params.get('mock_team') === '1'
    ? [{ ...person('m_dee', 's4', 'dee'), email: 'dee@madlabs.tech' }, { ...person('m_eli', 's5', 'eli'), email: 'eli@madlabs.tech' }]
    : []),
];
/** Members who proved their email (everyone seeded but eli; a sign-up once its code is used). */
const verified = new Set(members.filter((m) => m.id !== 'm_eli').map((m) => m.id));
const joined = (workspace_id: string, member_id: string, role: Role): Membership => ({ workspace_id, member_id, role, created_at: '2026-10-01T09:00:00Z' });
let memberships: Membership[] = [
  ...(mock_role === 'none' ? [] : [joined(MAIN_WORKSPACE, 'm_me', me_owner ? 'owner' : (mock_role as Role))]),
  joined(MAIN_WORKSPACE, 'm_ana', 'admin'),
  joined(MAIN_WORKSPACE, 'm_ben', 'member'),
  joined(MAIN_WORKSPACE, 'm_cho', 'viewer'),
];
/** Workspace names for invitations; mock.ts registers the ones it creates. */
const workspace_names = new Map([[MAIN_WORKSPACE, 'Main']]);
/** `?invited=1`: a workspace "Design" (only in the mock) that me@acme.io is invited to as a member, and one
 *  invitation into Main for someone else's email. The link tokens are `tok_design` and `tok_other`; `tok_expired`
 *  is an expired one. */
export const DESIGN_WORKSPACE = '0000000000000000000000000D';
const MOCK_SIGNUP = mock_params.get('allow_signup') !== '0';
const MOCK_CREATION_DISABLED = mock_params.get('creation_disabled') === '1';
type MockInvitation = Invitation & { token: string };
const invite_row = (id: string, workspace_id: string, email: string, role: Role, token: string, expires_at = '2099-01-01T00:00:00Z'): MockInvitation => ({
  id, workspace_id, email, role, inviter_id: 'm_ana', created_at: '2026-10-01T09:00:00Z', expires_at, accepted_at: null, declined_at: null, token,
});
let invitations: MockInvitation[] = [invite_row('i_expired', MAIN_WORKSPACE, 'me@acme.io', 'admin', 'tok_expired', '2026-01-01T00:00:00Z')];
if (mock_params.get('invited') === '1') {
  workspace_names.set(DESIGN_WORKSPACE, 'Design');
  memberships.push(joined(DESIGN_WORKSPACE, 'm_ana', 'owner'));
  invitations.push(invite_row('i_design', DESIGN_WORKSPACE, 'me@acme.io', 'member', 'tok_design'), invite_row('i_other', MAIN_WORKSPACE, 'eve@acme.io', 'viewer', 'tok_other'));
}
let invitation_seq = 0;
/** `?smtp=1`: the mail server is already set up. */
let smtp: SmtpView = mock_params.get('smtp')
  ? { smtp: { host: 'smtp.acme.io', port: 587, username: 'sun', from: 'sun@acme.io', tls: 'starttls' }, has_password: true }
  : { smtp: null, has_password: false };
/** Sign-in providers; the mock's Keycloak has its secret saved. */
let sso: SsoProvider[] = [
  { id: 'keycloak', kind: 'oidc', name: 'Keycloak', client_id: 'sun', issuer_url: ISSUER, allowed_domains: ['acme.io'], has_secret: true },
];
const sso_view = (): SsoView => ({ providers: structuredClone(sso), redirect_url: `${location.origin}/api/v1/auth/sso/{id}/callback` });
/** `?no_bypass`: sun.yaml `runtimes.allow_bypass` is off. `?bypass=members|everyone`: the saved bypass policy. */
let bypass: Omit<BypassPolicy, 'allow_bypass'> = {
  mode: (mock_params.get('bypass') as BypassPolicy['mode'] | null) ?? 'off',
  member_ids: [],
};
const mock_allow_bypass = !mock_params.has('no_bypass');
/** The signed-in person (`m_me` at first). `?owner_email=0`: the owner hasn't given an email yet (first run).
 *  `?signed_out=1`: nobody until a sign-in. */
const me_state = {
  user: 'me',
  is_owner: me_owner,
  via: 'local' as Me['via'],
  email: members[0].email,
  must_set_password: false,
  signed_in: mock_params.get('signed_out') !== '1',
};
const me_id = () => members.find((m) => m.username === me_state.user)?.id ?? '';
/** The mock's right password (unless an admin set another) and emailed code. */
export const MOCK_PASSWORD = 'correct horse battery';
export const MOCK_CODE = '123456';
/** Passwords admins set in the mock, by member id. */
const set_passwords = new Map<string, string>();
const member_by_id = (id: string) => {
  const member = members.find((m) => m.id === decodeURIComponent(id));
  if (!member) throw new ApiFailure(404, 'not_found', 'There is no such member.');
  return member;
};
const check_password = (password: string) => {
  if (password.length < 12) throw new ApiFailure(422, 'invalid_input', 'Use a password of 12 to 1024 characters.');
};
const wrong = () => new ApiFailure(401, 'unauthorized', 'That email, password or code is not right.');
const now = () => new Date().toISOString();
function audit_member(target: string, detail: string) {
  audit.unshift({ id: `a${++audit_seq}`, actor: `user:${me_state.user}`, action: 'member_change', target, detail, at: now() });
}
const at = (hm: string) => `2026-10-07T${hm}:00Z`;
const audit: AuditEntry[] = [
  { id: 'a4', actor: 'user:ana', action: 'approval', target: 'SUN-140/review', detail: 'approved', at: at('09:12') },
  { id: 'a3', actor: 'user:me', action: 'role_change', target: 'ana', detail: 'Viewer, Member -> Viewer, Member, Admin', at: at('08:40') },
  { id: 'a2', actor: 'user:cho', action: 'login', target: 'cho', detail: 'signed in with sso:keycloak', at: at('08:31') },
  { id: 'a1', actor: 'user:me', action: 'login', target: 'me', detail: 'password', at: at('08:30') },
];
let audit_seq = audit.length;
/** Second steps by username: cho has an authenticator, and with `?cho_email_code=1` email codes too. */
const factors = new Map<string, Factor[]>([
  [
    'cho',
    [
      { id: 'f_cho', kind: 'totp', label: 'Phone', created_at: at('08:00') },
      ...(mock_params.get('cho_email_code') === '1' ? [{ id: 'f_cho_mail', kind: 'email' as const, label: 'Email', created_at: at('08:01') }] : []),
    ],
  ],
]);
/** `?require_two_step=1`: the ecosystem requires a second step. */
let policy: SecurityPolicy = { require_two_step: mock_params.get('require_two_step') === '1', two_step_for_password_change: false };
/** The sign-in waiting for its second step, and the authenticator being added. */
let pending_sign_in: Member | null = null;
let pending_totp: { id: string; label: string } | null = null;
let factor_seq = 0;
/** The key the mock's authenticator shows; any code but `MOCK_CODE` is wrong. */
export const MOCK_TOTP_SECRET = 'JBSWY3DPEHPK3PXP';
const MOCK_QR =
  'data:image/svg+xml,' +
  encodeURIComponent(
    '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 7 7"><rect width="7" height="7" fill="white"/>' +
      '<path d="M0 0h3v3H0zM4 0h3v3H4zM0 4h3v3H0zM4 4h1v1H4zM6 6h1v1H6z" fill="black"/></svg>',
  );
const factors_of = (username: string) => factors.get(username) ?? [];
/** Passkeys: each passkey factor's credential id, and the one being added. The mock checks no signature:
 *  a credential with one of your ids passes. Like the kernel, passkeys work on https or localhost only. */
const passkey_ids = new Map<string, string>();
let pending_passkey = false;
const MOCK_PASSKEY_CHALLENGE = 'c3VuLW1vY2stY2hhbGxlbmdl';
const passkey_ready = () =>
  location.protocol === 'https:' ? !/^[\d.]+$|^\[/.test(location.hostname) : location.hostname === 'localhost';
const my_passkey_ids = (username: string) => factors_of(username).flatMap((f) => passkey_ids.get(f.id) ?? []);
const two_step_right = (username: string, answer: TwoStepAnswer) =>
  answer.method === 'passkey'
    ? my_passkey_ids(username).includes((answer.credential as { id?: string } | undefined)?.id ?? '')
    : answer.code?.trim() === MOCK_CODE;
const two_step_methods = (member: Member): FactorKind[] => {
  const kinds = [...new Set(factors_of(member.username).map((f) => f.kind))];
  return kinds.length || !policy.require_two_step || !smtp.smtp ? kinds : ['email'];
};
const sign_in_as = (member: Member, by_code: boolean) =>
  Object.assign(me_state, {
    user: member.username,
    is_owner: member.is_owner,
    via: 'local',
    email: member.email,
    must_set_password: by_code || member.must_set_password,
    signed_in: true,
  });
// `?member_view=<role>`: this member already picked a role view on another device.
let view_role: string | null = new URLSearchParams(location.search).get('member_view');
function audit_entry(action: AuditAction, target: string, detail: string) {
  audit.unshift({ id: `a${++audit_seq}`, actor: `user:${me_state.user}`, action, target, detail, at: now() });
}
const not_found = () => new ApiFailure(404, 'not_found', 'Not found.');
const forbidden = (message: string) => new ApiFailure(403, 'forbidden', message);
const my_role = (workspace: string) => memberships.find((m) => m.workspace_id === workspace && m.member_id === me_id())?.role;
/** Like the kernel: a deployment owner may do everything; anyone else what their role there grants. */
const may = (permission: Permission, workspace: string) => me_state.is_owner || PERMISSIONS_OF[my_role(workspace) ?? 'viewer'].includes(permission);
/** The mock's signed-in person, for the workspace mock: a deployment owner, and the workspaces they are in. */
export const mock_me = () => ({ id: me_id(), is_owner: me_state.is_owner, workspaces: memberships.filter((m) => m.member_id === me_id()).map((m) => m.workspace_id) });
/** A workspace the mock created: its creator owns it. */
export function mock_join(workspace: string, name: string) {
  workspace_names.set(workspace, name);
  if (me_id()) memberships.push(joined(workspace, me_id(), 'owner'));
}
const owners = (workspace: string) => memberships.filter((m) => m.workspace_id === workspace && m.role === 'owner');
const LAST_OWNER = () => new ApiFailure(409, 'conflict', 'A workspace keeps at least one owner: make someone else owner first.');
/** 404 unless the caller sees `workspace` (a member, or a deployment owner). */
function seen(workspace: string) {
  if (!workspace_names.has(workspace) || !(me_state.is_owner || my_role(workspace))) throw not_found();
}
export function need(permission: Permission, workspace: string, what: string) {
  if (!may(permission, workspace))
    throw forbidden(`Your role in this workspace can't ${what}. Ask a workspace admin for a role with the \`${permission}\` permission.`);
}
const view_of = (i: MockInvitation): InvitationView => ({
  id: i.id, workspace_id: i.workspace_id, workspace_name: workspace_names.get(i.workspace_id) ?? '', email: i.email, role: i.role,
  inviter: members.find((m) => m.id === i.inviter_id)?.username ?? null, expires_at: i.expires_at,
});
const pending = (i: MockInvitation) => !i.accepted_at && !i.declined_at && i.expires_at > now();
const plain = ({ token: _, ...rest }: MockInvitation): Invitation => rest;
// Groups, the terminal policy and host shares, kept in localStorage like the mock's editor grants, so what an owner
// sets holds for `?mock_role=…` on the next page.
const GROUPS_KEY = 'sun_mock_groups';
interface GroupStore {
  groups: MemberGroup[];
  terminal: TerminalPolicy;
  /** Group ids by host id. */
  shares: Record<string, string[]>;
}
const group_store = (): GroupStore =>
  JSON.parse(localStorage.getItem(GROUPS_KEY) ?? 'null') ?? { groups: [], terminal: { mode: 'granted', groups: [] }, shares: {} };
const keep_groups = (store: GroupStore) => localStorage.setItem(GROUPS_KEY, JSON.stringify(store));
/** Like `domain_matches`: active members whose verified email's domain is one of `domains`. */
const by_domain = (domains: string[]) =>
  members.filter((m) => !m.deactivated_at && verified.has(m.id) && domains.includes(m.email?.split('@').pop() ?? ''));
/** Like `is_member`: added by hand, or by a verified email on one of its domains. */
const in_group = (g: MemberGroup, id: string) => g.member_ids.includes(id) || by_domain(g.domains).some((m) => m.id === id);
const my_groups = () => group_store().groups.filter((g) => in_group(g, me_id()));
/** The groups a host is shared with; with `groups`, saves them instead (only groups that exist). */
export function mock_host_shares(host_id: string, groups?: string[]): string[] {
  const store = group_store();
  if (groups) {
    if (groups.some((id) => !store.groups.some((g) => g.id === id))) throw not_found();
    store.shares[host_id] = [...new Set(groups)];
    keep_groups(store);
  }
  return store.shares[host_id] ?? [];
}
/** The ids of the signed-in person's groups. */
export const mock_my_groups = () => my_groups().map((g) => g.id);
/** Hosts shared with a group the signed-in person is in. */
export const mock_shared_with_me = () => {
  const mine = mock_my_groups();
  return Object.keys(group_store().shares).filter((h) => mock_host_shares(h).some((g) => mine.includes(g)));
};
/** Like `TerminalPolicy::allows`. */
export function mock_can_terminal() {
  const { terminal } = group_store();
  return me_state.is_owner || terminal.mode === 'granted' || my_groups().some((g) => terminal.groups.includes(g.id));
}
const DOMAIN = /^(?!-)[a-z0-9-]{1,63}(?<!-)(\.(?!-)[a-z0-9-]{1,63}(?<!-))+$/;
/** Like `normalize_domains`: lowercase, sorted, no duplicates; 422 names the first bad one. */
function normalize_domains(list: string[]) {
  const domains = [...new Set(list.map((d) => d.trim().toLowerCase()))].sort();
  const bad = domains.find((d) => !DOMAIN.test(d));
  if (bad !== undefined) throw new ApiFailure(422, 'invalid_input', `\`${bad}\` is not an email domain (like \`madlabs.tech\`).`);
  return domains;
}
function group_fields(store: GroupStore, input: Partial<MemberGroup>, self?: MemberGroup) {
  const name = (input.name ?? self?.name ?? '').trim();
  if (!name || name.length > 100) throw new ApiFailure(422, 'invalid_input', 'A group name is 1 to 100 characters.');
  if (store.groups.some((g) => g !== self && g.name === name)) throw new ApiFailure(422, 'invalid_input', `There is already a group named ${name}.`);
  return { name, description: (input.description ?? self?.description ?? '').trim(), domains: normalize_domains(input.domains ?? self?.domains ?? []) };
}
let group_seq = 0;
/** `/groups/**` and `/settings/terminal`, like crates/sun-api/src/groups.rs; `undefined` for any other route. */
function groups_route(method: string, path: string, body: unknown): unknown {
  const [bare, query = ''] = path.split('?');
  const parts = bare.split('/').filter(Boolean).map(decodeURIComponent);
  if (parts[0] !== 'groups' && bare !== '/settings/terminal') return undefined;
  const store = group_store();
  if (bare === '/groups/mine') return my_groups().map(({ member_ids: _, ...g }) => g);
  if (!me_state.is_owner) throw forbidden('Only a deployment owner can do this.');
  if (bare === '/settings/terminal') {
    if (method === 'PUT') {
      const policy = body as TerminalPolicy;
      if (policy.groups.some((id) => !store.groups.some((g) => g.id === id))) throw not_found();
      store.terminal = { mode: policy.mode, groups: [...new Set(policy.groups)] };
      keep_groups(store);
    }
    return { ...store.terminal };
  }
  const input = (body ?? {}) as Partial<MemberGroup>;
  if (parts.length === 1) {
    if (method === 'GET') return [...store.groups].sort((a, b) => a.name.localeCompare(b.name));
    const id = `01JGRP${String(Date.now()).slice(-8)}${String(++group_seq).padStart(12, '0')}`;
    const group: MemberGroup = { id, ...group_fields(store, input), created_by: me_id() || null, created_at: now(), member_ids: [] };
    store.groups.push(group);
    keep_groups(store);
    return group;
  }
  const group = store.groups.find((g) => g.id === parts[1]);
  if (!group) throw not_found();
  if (parts[2] === 'preview') {
    const domains = new URLSearchParams(query).get('domains');
    return by_domain(domains === null ? group.domains : normalize_domains(domains.split(',').filter(Boolean))).map((m) => ({ ...m }));
  }
  if (parts[2] === 'members') {
    const member = member_by_id(parts[3]);
    const manual = group.member_ids.includes(member.id);
    if (method === 'DELETE' && !manual) throw not_found();
    group.member_ids = method === 'DELETE' ? group.member_ids.filter((m) => m !== member.id) : manual ? group.member_ids : [...group.member_ids, member.id];
  } else if (method === 'DELETE') {
    // Its editor grants go with it, the terminal policy and host shares stop listing it.
    store.groups = store.groups.filter((g) => g !== group);
    store.terminal.groups = store.terminal.groups.filter((g) => g !== group.id);
    for (const host of Object.keys(store.shares)) store.shares[host] = store.shares[host].filter((g) => g !== group.id);
    const grants = JSON.parse(localStorage.getItem('sun_mock_editor_grants') ?? '[]') as { subject_type: string; subject_id?: string }[];
    localStorage.setItem('sun_mock_editor_grants', JSON.stringify(grants.filter((g) => !(g.subject_type === 'group' && g.subject_id === group.id))));
  } else {
    Object.assign(group, group_fields(store, input, group));
  }
  keep_groups(store);
  return method === 'PATCH' ? { ...group } : null;
}

const DEPLOYMENT_ONLY = /^\/(members|audit|smtp|settings\/(sso|security))(\/|$)/;

/** The workspace members, invitations and sign-up routes, like crates/sun-api/src/auth/{members,invitations}.rs. */
function membership_route(method: string, path: string, body: unknown): unknown {
  const route = `${method} ${path}`;
  const input = (body ?? {}) as { email?: string; role?: Role };
  if (route === 'POST /signup') {
    const email = (input.email ?? '').trim().toLowerCase();
    if (!/^[^@\s]+@[^@\s]+\.[^@\s]+$/.test(email)) throw new ApiFailure(422, 'invalid_input', 'That is not an email address.');
    if (!MOCK_SIGNUP && !invitations.some((i) => pending(i) && i.email === email)) {
      throw forbidden('Sign-up is closed here. Ask a workspace admin for an invitation.');
    }
    if (!smtp.smtp) throw new ApiFailure(503, 'unavailable', 'This deployment can’t send email yet, so nobody can sign up. Ask its owner.');
    const known = members.find((m) => m.email === email);
    if (known && (known.has_password || verified.has(known.id) || known.issuer !== 'local')) {
      throw new ApiFailure(409, 'account_exists', 'An account with this email exists. Sign in instead.');
    }
    if (!known) {
      const username = email.split('@')[0].replace(/[^A-Za-z0-9._-]/g, '') || 'member';
      members.push({ id: `m_${username}_${members.length}`, issuer: 'local', subject: email, username, email, is_owner: false, has_password: false, must_set_password: false });
    }
    return { email };
  }
  const look_up = path.match(/^\/invitations\/([^/]+)$/);
  if (method === 'GET' && look_up) {
    const found = invitations.find((i) => i.token === decodeURIComponent(look_up[1]) && pending(i));
    if (!found) throw not_found();
    return view_of(found);
  }
  const signed_in = () => {
    if (!me_state.signed_in) throw new ApiFailure(401, 'unauthorized', 'Sign in first.');
  };
  if (route === 'GET /me/invitations') signed_in();
  if (route === 'GET /me/invitations') return invitations.filter((i) => pending(i) && i.email === me_state.email).map(view_of);
  const answer = path.match(/^\/invitations\/([^/]+)\/(accept|decline)$/);
  if (method === 'POST' && answer) {
    signed_in();
    const found = invitations.find((i) => i.id === decodeURIComponent(answer[1]) && pending(i));
    if (!found) throw not_found();
    if (found.email !== me_state.email) throw forbidden('This invitation is for another email address.');
    if (answer[2] === 'decline') {
      found.declined_at = now();
      return null;
    }
    found.accepted_at = now();
    const existing = memberships.find((m) => m.workspace_id === found.workspace_id && m.member_id === me_id());
    // Accepting never lowers a role they have there.
    if (existing) existing.role = covers(existing.role, found.role) ? existing.role : found.role;
    else memberships.push({ ...joined(found.workspace_id, me_id(), found.role), created_at: now() });
    return { ...memberships.find((m) => m.workspace_id === found.workspace_id && m.member_id === me_id()) };
  }
  const scoped = path.match(/^\/workspaces\/([^/]+)\/(members|invitations|leave)(?:\/([^/]+))?$/);
  if (!scoped) return undefined;
  signed_in();
  const [workspace, what, target] = [decodeURIComponent(scoped[1]), scoped[2], scoped[3] && decodeURIComponent(scoped[3])];
  seen(workspace);
  if (route.endsWith('/leave')) {
    const mine = memberships.find((m) => m.workspace_id === workspace && m.member_id === me_id());
    if (!mine) throw new ApiFailure(422, 'invalid_input', 'You are not a member of this workspace.');
    if (mine.role === 'owner' && owners(workspace).length === 1) throw LAST_OWNER();
    memberships = memberships.filter((m) => m !== mine);
    return null;
  }
  if (what === 'members') {
    if (method === 'GET') {
      return memberships
        .filter((m) => m.workspace_id === workspace)
        .flatMap((m) => {
          const who = members.find((p) => p.id === m.member_id);
          return who ? [{ ...m, username: who.username, email: who.email }] : [];
        });
    }
    need('edit_members', workspace, 'edit members');
    const row = memberships.find((m) => m.workspace_id === workspace && m.member_id === target);
    if (!row) throw not_found();
    const mine = my_role(workspace);
    if (!me_state.is_owner && (!covers(mine, row.role) || (input.role && !covers(mine, input.role)))) {
      throw forbidden('You can’t change the role of someone above you, or give a role above your own.');
    }
    const last = row.role === 'owner' && owners(workspace).length === 1;
    if (method === 'DELETE') {
      if (last) throw LAST_OWNER();
      memberships = memberships.filter((m) => m !== row);
      return null;
    }
    if (last && input.role !== 'owner') throw LAST_OWNER();
    row.role = input.role ?? row.role;
    audit_entry('role_change', members.find((p) => p.id === row.member_id)?.username ?? '', `${workspace_names.get(workspace)}: ${row.role}`);
    return { ...row };
  }
  need('invite_members', workspace, 'invite members');
  if (method === 'GET') return invitations.filter((i) => i.workspace_id === workspace && pending(i)).reverse().map(plain);
  if (method === 'DELETE') {
    const found = invitations.find((i) => i.id === target && i.workspace_id === workspace && pending(i));
    if (!found) throw not_found();
    invitations = invitations.filter((i) => i !== found);
    return null;
  }
  const email = (input.email ?? '').trim().toLowerCase();
  const role = input.role ?? 'viewer';
  if (!/^[^@\s]+@[^@\s]+\.[^@\s]+$/.test(email)) throw new ApiFailure(422, 'invalid_input', 'That is not an email address.');
  if (!me_state.is_owner && !covers(my_role(workspace), role)) throw forbidden(`You can't add someone as ${role} in that workspace.`);
  const already = members.find((m) => m.email === email);
  if (already && memberships.some((m) => m.workspace_id === workspace && m.member_id === already.id)) {
    throw new ApiFailure(422, 'invalid_input', `${email} is already a member of ${workspace_names.get(workspace)}.`);
  }
  invitations = invitations.filter((i) => !(i.workspace_id === workspace && i.email === email && pending(i)));
  const id = `i_${++invitation_seq}`;
  const created = invite_row(id, workspace, email, role, `tok_${invitation_seq}`, new Date(Date.now() + 7 * 864e5).toISOString());
  created.inviter_id = me_id() || null;
  created.created_at = now();
  invitations.push(created);
  const mailed = !!smtp.smtp;
  return { invitation: plain(created), mailed, link: mailed ? null : `${location.origin}/#/invite/${created.token}` };
}

function fake(method: string, path: string, body: unknown): unknown {
  const route = `${method} ${path}`;
  if (route === 'GET /auth') {
    const providers = sso.filter((p) => p.has_secret).map(({ id, name, kind }) => ({ id, name, kind }));
    return { providers, local: true, allow_signup: MOCK_SIGNUP, workspace_creation_disabled: MOCK_CREATION_DISABLED };
  }
  const answered = membership_route(method, path, body);
  if (answered !== undefined) return answered;
  const grouped = groups_route(method, path, body);
  if (grouped !== undefined) return grouped;
  if (DEPLOYMENT_ONLY.test(path) && !me_state.is_owner) throw forbidden('Only a deployment owner can do this.');
  if (route === 'GET /settings/sso') return sso_view();
  const sso_path = path.match(/^\/settings\/sso\/([^/]+)$/);
  if (sso_path) {
    const id = decodeURIComponent(sso_path[1]);
    const old = sso.find((p) => p.id === id);
    if (method === 'DELETE') {
      if (!old) throw not_found();
      sso = sso.filter((p) => p !== old);
      audit_entry('settings_change', 'sso_providers', `sso ${id} ${old.kind} removed`);
      return null;
    }
    const { client_secret, ...input } = body as SsoInput;
    if (!/^[a-z][a-z0-9_]{0,31}$/.test(id)) throw new ApiFailure(422, 'invalid_input', 'A provider id is 1 to 32 of a-z, 0-9 and _, starting with a letter.');
    if (!old && !client_secret) throw new ApiFailure(422, 'invalid_input', 'Enter the client secret from the provider.');
    if (input.kind === 'oidc' && !/^https?:\/\/\S+$/.test(input.issuer_url ?? '')) {
      throw new ApiFailure(422, 'invalid_input', 'Enter the issuer URL, such as https://sso.example.com/realms/acme.');
    }
    const provider: SsoProvider = {
      ...input,
      id,
      issuer_url: input.kind === 'oidc' ? input.issuer_url : null,
      allowed_domains: input.allowed_domains.map((d) => d.trim().toLowerCase()),
      has_secret: true,
    };
    sso = old ? sso.map((p) => (p === old ? provider : p)) : [...sso, provider];
    audit_entry('settings_change', 'sso_providers', `sso ${id} ${input.kind} saved${client_secret ? ', secret set' : ''}`);
    return sso_view();
  }
  if (route === 'DELETE /session') {
    me_state.signed_in = false;
    return null;
  }
  if (route === 'GET /me') {
    if (!me_state.signed_in) throw new ApiFailure(401, 'unauthorized', 'Sign in first.');
    const must_enroll_two_step = policy.require_two_step && me_state.via === 'local' && !factors_of(me_state.user).length;
    const { signed_in: _, ...me } = me_state;
    const memberships_of_me = memberships.filter((m) => m.member_id === me_id()).map((m) => ({ ...m }));
    const may = me_state.is_owner || bypass.mode === 'everyone' || (bypass.mode === 'members' && bypass.member_ids.includes(me_id()));
    const can_bypass = mock_allow_bypass && may;
    return { ...me, memberships: memberships_of_me, email_verified: verified.has(me_id()), needs_email: !me_state.email, must_enroll_two_step, view_role, can_bypass, can_terminal: mock_can_terminal() };
  }
  if (route === 'POST /session') {
    const input = body as { email?: string; password?: string };
    const member = members.find((m) => m.email === input.email?.trim().toLowerCase());
    if (!member || member.deactivated_at) throw wrong();
    const by_password = member.has_password && input.password === (set_passwords.get(member.id) ?? MOCK_PASSWORD);
    const by_code = !by_password && input.password?.trim() === MOCK_CODE;
    if (!by_password && !by_code) throw wrong();
    const methods = two_step_methods(member);
    if (by_password && methods.length) {
      pending_sign_in = member;
      return { two_step: { challenge: 'c_mock', methods } };
    }
    if (by_code) verified.add(member.id);
    sign_in_as(member, by_code);
    return null;
  }
  if (route === 'POST /session/two_step') {
    const answer = body as TwoStepAnswer;
    // Passkey options, for a sign-in or for your own password change.
    if (answer.method === 'passkey' && !answer.credential) {
      const ids = my_passkey_ids(pending_sign_in?.username ?? me_state.user);
      if (!ids.length || !passkey_ready()) throw wrong();
      // WebAuthn's own JSON is camelCase.
      const allow = ids.map((id) => ({ type: 'public-key', id }));
      return { options: { challenge: MOCK_PASSKEY_CHALLENGE, rpId: location.hostname, allowCredentials: allow, userVerification: 'preferred', timeout: 60000 } };
    }
    if (!pending_sign_in || !two_step_right(pending_sign_in.username, answer)) throw wrong();
    sign_in_as(pending_sign_in, false);
    pending_sign_in = null;
    return null;
  }
  if (route === 'POST /session/two_step/email') return null;
  if (route === 'GET /me/two_step') {
    return { factors: structuredClone(factors_of(me_state.user)), policy: { ...policy }, email_ready: !!smtp.smtp, passkey_ready: passkey_ready() };
  }
  if (route === 'POST /me/two_step/passkey') {
    if (!passkey_ready()) throw new ApiFailure(409, 'conflict', 'Passkeys need Sun on https or localhost. Ask an admin to set its public URL.');
    pending_passkey = true;
    const options = {
      challenge: MOCK_PASSKEY_CHALLENGE,
      rp: { id: location.hostname, name: 'Sun' },
      user: { id: btoa(me_state.user).replace(/=+$/, ''), name: me_state.email ?? me_state.user, displayName: me_state.user },
      pubKeyCredParams: [-7, -8].map((alg) => ({ type: 'public-key', alg })),
      excludeCredentials: my_passkey_ids(me_state.user).map((id) => ({ type: 'public-key', id })),
      timeout: 60000,
      authenticatorSelection: { residentKey: 'preferred', userVerification: 'preferred' },
      attestation: 'none',
    };
    return { challenge: 'c_passkey', options };
  }
  if (route === 'POST /me/two_step/passkey/confirm') {
    const { challenge, credential, label } = body as { challenge: string; credential?: { id?: string }; label?: string };
    if (!pending_passkey || challenge !== 'c_passkey') throw new ApiFailure(422, 'invalid_input', 'That passkey isn’t being added. Start again.');
    if (!credential?.id) throw new ApiFailure(422, 'invalid_input', 'Sun couldn’t check that passkey. Try again.');
    const factor: Factor = { id: `f_${++factor_seq}`, kind: 'passkey', label: label?.trim() || 'Passkey', created_at: now() };
    factors.set(me_state.user, [...factors_of(me_state.user), factor]);
    passkey_ids.set(factor.id, credential.id);
    pending_passkey = false;
    audit_member(me_state.user, '2-step: passkey added');
    return { ...factor };
  }
  if (route === 'POST /me/two_step/totp') {
    pending_totp = { id: `f_${++factor_seq}`, label: (body as { label?: string }).label?.trim() || 'Authenticator' };
    const account = encodeURIComponent(me_state.email ?? me_state.user);
    return {
      id: pending_totp.id,
      secret: MOCK_TOTP_SECRET,
      otpauth_url: `otpauth://totp/Sun:${account}?secret=${MOCK_TOTP_SECRET}&issuer=Sun&algorithm=SHA1&digits=6&period=30`,
      qr_svg: MOCK_QR,
    };
  }
  if (route === 'POST /me/two_step/totp/confirm') {
    const { id, code } = body as { id: string; code: string };
    if (!pending_totp || pending_totp.id !== id) throw new ApiFailure(422, 'invalid_input', 'That authenticator isn’t being added. Start again.');
    if (code.trim() !== MOCK_CODE) throw new ApiFailure(422, 'invalid_input', 'That code is not right. Type the one your app shows now.');
    const factor: Factor = { id, kind: 'totp', label: pending_totp.label, created_at: now() };
    factors.set(me_state.user, [...factors_of(me_state.user), factor]);
    pending_totp = null;
    audit_member(me_state.user, '2-step: authenticator added');
    return { ...factor };
  }
  if (route === 'POST /me/two_step/email') {
    if (!smtp.smtp) throw new ApiFailure(409, 'conflict', 'Email codes need a mail server. Ask an admin to set up email in Settings.');
    if (factors_of(me_state.user).some((f) => f.kind === 'email')) throw new ApiFailure(422, 'invalid_input', 'Email codes are on already.');
    const factor: Factor = { id: `f_${++factor_seq}`, kind: 'email', label: 'Email', created_at: now() };
    factors.set(me_state.user, [...factors_of(me_state.user), factor]);
    audit_member(me_state.user, '2-step: email codes on');
    return { ...factor };
  }
  if (route === 'POST /me/two_step/challenge') {
    const methods = [...new Set(factors_of(me_state.user).map((f) => f.kind))];
    if (!methods.length) throw new ApiFailure(422, 'invalid_input', 'You have no 2-step method.');
    return { challenge: 'c_mock', methods };
  }
  const factor_path = path.match(/^\/me\/two_step\/([^/]+)$/);
  if (method === 'DELETE' && factor_path) {
    const mine = factors_of(me_state.user);
    if (policy.require_two_step && mine.length === 1 && mine[0].id === decodeURIComponent(factor_path[1])) {
      throw new ApiFailure(409, 'conflict', 'Add another method before removing this one.');
    }
    factors.set(me_state.user, factors_of(me_state.user).filter((f) => f.id !== decodeURIComponent(factor_path[1])));
    audit_member(me_state.user, '2-step: method removed');
    return null;
  }
  const reset_path = path.match(/^\/members\/([^/]+)\/two_step$/);
  if (method === 'DELETE' && reset_path) {
    const member = member_by_id(reset_path[1]);
    factors.delete(member.username);
    audit_member(member.username, '2-step reset by an admin');
    return { ...member, has_two_step: false };
  }
  if (route === 'GET /settings/security') return { ...policy };
  if (route === 'GET /settings/bypass') return { ...bypass, allow_bypass: mock_allow_bypass };
  if (route === 'PUT /settings/bypass') {
    const input = body as Omit<BypassPolicy, 'allow_bypass'>;
    for (const id of input.member_ids) member_by_id(id);
    bypass = { mode: input.mode, member_ids: [...new Set(input.member_ids)] };
    audit_entry('settings_change', 'bypass_policy', `bypass permissions: ${bypass.mode} (${bypass.member_ids.length} member(s) listed)`);
    return { ...bypass, allow_bypass: mock_allow_bypass };
  }
  if (route === 'PUT /settings/security') {
    policy = { ...policy, ...(body as Partial<SecurityPolicy>) };
    audit_entry('settings_change', 'security', `2-step required: ${policy.require_two_step}, for password changes: ${policy.two_step_for_password_change}`);
    return { ...policy };
  }
  if (route === 'PUT /me/password') {
    const { password, current_password, two_step } = body as { password: string; current_password?: string; two_step?: TwoStepAnswer };
    check_password(password);
    const member = members.find((m) => m.username === me_state.user);
    const current = (member && set_passwords.get(member.id)) || MOCK_PASSWORD;
    if (!me_state.must_set_password && (!member || member.has_password) && current_password !== current) {
      throw new ApiFailure(403, 'forbidden', 'Your current password is not right.');
    }
    if (!me_state.must_set_password && policy.two_step_for_password_change && factors_of(me_state.user).length) {
      if (!two_step) throw new ApiFailure(403, 'two_step_required', 'Confirm with your 2-step method to change your password.');
      if (!two_step_right(me_state.user, two_step)) throw new ApiFailure(403, 'forbidden', 'That 2-step code is not right.');
    }
    me_state.must_set_password = false;
    if (member) Object.assign(member, { has_password: true, must_set_password: false });
    if (member) set_passwords.set(member.id, password);
    return null;
  }
  if (route === 'PUT /me/email') {
    const email = (body as { email: string }).email.trim().toLowerCase();
    if (!/^[^@\s]+@[^@\s]+\.[^@\s]+$/.test(email)) throw new ApiFailure(422, 'invalid_input', 'That is not an email address.');
    me_state.email = email;
    const mine = members.find((m) => m.id === me_id());
    if (mine) mine.email = email;
    return { email };
  }
  if (route === 'GET /smtp') return structuredClone(smtp);
  if (route === 'PUT /smtp') {
    const { password, ...settings } = body as SmtpSettings & { password?: string };
    if (!settings.host.trim() || /\s/.test(settings.host)) {
      throw new ApiFailure(422, 'invalid_input', 'The SMTP host must be a host name such as smtp.example.com.');
    }
    smtp = { smtp: settings, has_password: password === undefined ? smtp.has_password : password !== '' };
    audit.unshift({ id: `a${++audit_seq}`, actor: 'user:me', action: 'settings_change', target: 'smtp', detail: `smtp ${settings.host}:${settings.port} ${settings.tls}`, at: now() });
    return structuredClone(smtp);
  }
  if (route === 'POST /smtp/test') {
    if (!smtp.smtp) throw new ApiFailure(409, 'conflict', 'Configure email first: Settings → Email (SMTP).');
    return { sent_to: me_state.email };
  }
  if (route === 'POST /members') {
    const input = body as { email: string; username: string; workspace_id?: string; role?: Role; password?: string; must_change?: boolean };
    if (input.password !== undefined) check_password(input.password);
    else if (!smtp.smtp) throw new ApiFailure(409, 'conflict', 'Configure email first: Settings → Email (SMTP).');
    const email = input.email.trim().toLowerCase();
    if (members.some((m) => m.email === email)) throw new ApiFailure(422, 'invalid_input', 'Someone with that email is already a member.');
    const [workspace, role] = [input.workspace_id ?? MAIN_WORKSPACE, input.role ?? 'viewer'];
    const with_password = input.password !== undefined;
    const member: Member = {
      id: `m_${input.username}`, issuer: 'local', subject: email, username: input.username, email, is_owner: false,
      has_password: with_password, must_set_password: with_password && input.must_change !== false, deactivated_at: null,
    };
    members.push(member);
    memberships.push({ ...joined(workspace, member.id, role), created_at: now() });
    if (input.password !== undefined) set_passwords.set(member.id, input.password);
    audit_member(member.username, `${with_password ? 'created with a password' : 'invited'} ${email} into ${workspace_names.get(workspace)} as ${role}`);
    return { ...member };
  }
  const member_path = path.match(/^\/members\/([^/]+)\/(invite|deactivate|activate|password)$/);
  if (member_path && (method === 'POST' || member_path[2] === 'password')) {
    const member = member_by_id(member_path[1]);
    const action = member_path[2];
    if (action === 'password') {
      const input = body as { password: string; must_change: boolean };
      check_password(input.password);
      set_passwords.set(member.id, input.password);
      Object.assign(member, { has_password: true, must_set_password: input.must_change });
      audit_member(member.username, `password set by an admin${input.must_change ? ' (change at next sign-in)' : ''}`);
    } else if (action === 'deactivate') {
      if (member.username === me_state.user) throw new ApiFailure(422, 'invalid_input', 'You can’t deactivate yourself.');
      member.deactivated_at = now();
      audit_member(member.username, 'deactivated');
    } else if (action === 'activate') {
      member.deactivated_at = null;
      audit_member(member.username, 'reactivated');
    } else {
      if (!smtp.smtp) throw new ApiFailure(409, 'conflict', 'Configure email first: Settings → Email (SMTP).');
      member.has_password = false;
      audit_member(member.username, `re-invited ${member.email}`);
    }
    return { ...member };
  }
  if (route === 'PUT /me/view_role') {
    view_role = (body as { view_role: string | null }).view_role;
    return { view_role };
  }
  if (route === 'GET /members') return members.map((m) => ({ ...m, has_two_step: factors_of(m.username).length > 0 }));
  if (route === 'GET /audit') return audit.map((a) => ({ ...a }));
  const owner_path = path.match(/^\/members\/([^/]+)\/owner$/);
  if (method === 'PUT' && owner_path) {
    const member = member_by_id(owner_path[1]);
    if (member.id === me_id()) throw new ApiFailure(422, 'invalid_input', 'You can’t change your own owner flag.');
    member.is_owner = (body as { is_owner: boolean }).is_owner;
    audit_entry('role_change', member.username, member.is_owner ? 'made deployment owner' : 'no longer deployment owner');
    return { ...member };
  }
  throw new ApiFailure(404, 'not_found', `No mock for ${route}`);
}
