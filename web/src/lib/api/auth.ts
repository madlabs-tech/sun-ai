// Sign-in, 2-step, members, groups and the audit log. Mirrors crates/sun-api/src/auth.
// With ?mock=1 an in-memory fake answers instead of the kernel.
import { ApiFailure, request, use_mock } from './client';

/** What a group may grant, per verb. An owner always has all of them. */
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
  | 'manage_groups'
  | 'create_workspace'
  | 'edit_workspace'
  | 'delete_workspace'
  | 'create_workflow'
  | 'edit_workflow'
  | 'access_all_workspaces';
/** Every permission, in the kernel's order. */
export const PERMISSIONS: Permission[] = [
  'run',
  'approve',
  'manage_plugins',
  'manage_extensions',
  'manage_secrets',
  'manage_runtimes',
  'manage_settings',
  'invite_members',
  'create_members',
  'view_members',
  'edit_members',
  'deactivate_members',
  'manage_groups',
  'create_workspace',
  'edit_workspace',
  'delete_workspace',
  'create_workflow',
  'edit_workflow',
  'access_all_workspaces',
];

/** A role admins can rename and customize. `key` is what `who: role:<key>` matches; it never changes. */
export interface Group {
  id: string;
  key: string;
  name: string;
  permissions: Permission[];
}

export interface Member {
  id: string;
  issuer: string;
  subject: string;
  username: string;
  /** Lowercase; null only for a member from before invites who hasn't given one yet. */
  email: string | null;
  /** Owners have every permission. */
  is_owner: boolean;
  /** Ids of the groups they are in. */
  groups: string[];
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
  | 'agent_change';

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
  is_owner: boolean;
  groups: Group[];
  /** The union of the groups' permissions; all of them for an owner. */
  permissions: Permission[];
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
}

/** How a sign-in provider works: the Google and GitHub presets, or any OIDC provider. */
export type SsoKind = 'google' | 'github' | 'oidc';

export interface Providers {
  /** One "Continue with {name}" each. */
  providers: { id: string; name: string; kind: SsoKind }[];
  /** Members sign in with email + password (or an emailed code in the password box). */
  local?: boolean;
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
  set_email: (email: string) => call<{ email: string }>('PUT', '/me/email', { email }),
  /** With `password`, no mail goes out (and SMTP isn't needed); `must_change` forces a change at first sign-in. */
  invite: (email: string, username: string, group_ids: string[], password?: { password: string; must_change: boolean }) =>
    call<Member>('POST', '/members', { email, username, group_ids, ...password }),
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
  set_member_groups: (id: string, group_ids: string[]) =>
    call<Member>('PUT', `/members/${encodeURIComponent(id)}/groups`, { group_ids }),
  set_owner: (id: string, is_owner: boolean) => call<Member>('PUT', `/members/${encodeURIComponent(id)}/owner`, { is_owner }),
  list_groups: () => call<Group[]>('GET', '/groups'),
  create_group: (name: string, permissions: Permission[]) => call<Group>('POST', '/groups', { name, permissions }),
  update_group: (id: string, change: { name?: string; permissions?: Permission[] }) =>
    call<Group>('PATCH', `/groups/${encodeURIComponent(id)}`, change),
  delete_group: (id: string) => call<null>('DELETE', `/groups/${encodeURIComponent(id)}`),
  set_group_members: (id: string, member_ids: string[]) =>
    call<Member[]>('PUT', `/groups/${encodeURIComponent(id)}/members`, { member_ids }),
  list_audit: () => call<AuditEntry[]>('GET', '/audit'),
};

// ---------- in-memory fake (?mock=1) ----------

const ISSUER = 'https://sso.example.com/realms/acme';
/** The seeded groups' ids, as the kernel's migration makes them. */
export const VIEWER = '00000000000000000000000001';
export const MEMBER = '00000000000000000000000002';
export const ADMIN = '00000000000000000000000003';
let groups: Group[] = [
  // `?viewer_edits_members=1`: Viewer may see, edit and deactivate members (but not manage groups).
  {
    id: VIEWER,
    key: 'viewer',
    name: 'Viewer',
    permissions:
      new URLSearchParams(location.search).get('viewer_edits_members') === '1' ? ['view_members', 'edit_members', 'deactivate_members'] : [],
  },
  { id: MEMBER, key: 'member', name: 'User', permissions: ['run', 'approve', 'create_workflow', 'edit_workflow'] },
  { id: ADMIN, key: 'admin', name: 'Admin', permissions: [...PERMISSIONS] },
];
/** The groups an old role became: each member is in every group their role covered. */
const ROLE_GROUPS: Record<string, string[]> = { viewer: [VIEWER], member: [VIEWER, MEMBER], admin: [VIEWER, MEMBER, ADMIN] };
const person = (id: string, subject: string, username: string, role: string): Member => ({
  id, issuer: ISSUER, subject, username, email: `${username}@acme.io`, is_owner: false, groups: [...ROLE_GROUPS[role]], has_password: true, must_set_password: false,
});
let members: Member[] = [person('m_ana', 's1', 'ana', 'admin'), person('m_ben', 's2', 'ben', 'member'), person('m_cho', 's3', 'cho', 'viewer')];
const mock_params = new URLSearchParams(location.search);
// `?mock_role=<viewer|member|admin>`: sign in as someone in that old role's groups instead of the owner.
const mock_role = mock_params.get('mock_role');
/** `?smtp=1`: the mail server is already set up. */
let smtp: SmtpView = mock_params.get('smtp')
  ? { smtp: { host: 'smtp.acme.io', port: 587, username: 'sun', from: 'sun@acme.io', tls: 'starttls' }, has_password: true }
  : { smtp: null, has_password: false };
/** Sign-in providers; the mock's Keycloak has its secret saved. */
let sso: SsoProvider[] = [
  { id: 'keycloak', kind: 'oidc', name: 'Keycloak', client_id: 'sun', issuer_url: ISSUER, allowed_domains: ['acme.io'], has_secret: true },
];
const sso_view = (): SsoView => ({ providers: structuredClone(sso), redirect_url: `${location.origin}/api/v1/auth/sso/{id}/callback` });
/** The signed-in person. `?owner_email=0`: the owner hasn't given an email yet (first run). */
const me_state = {
  user: 'me',
  is_owner: !mock_role || mock_role === 'owner',
  groups: ROLE_GROUPS[mock_role ?? ''] ?? ([] as string[]),
  via: 'local' as Me['via'],
  email: mock_params.get('owner_email') === '0' ? null : ('me@acme.io' as string | null),
  must_set_password: false,
};
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
    groups: [...member.groups],
    via: 'local',
    email: member.email,
    must_set_password: by_code || member.must_set_password,
  });
// `?member_view=<role>`: this member already picked a role view on another device.
let view_role: string | null = new URLSearchParams(location.search).get('member_view');
let group_seq = 0;
function audit_entry(action: AuditAction, target: string, detail: string) {
  audit.unshift({ id: `a${++audit_seq}`, actor: `user:${me_state.user}`, action, target, detail, at: now() });
}
const group_names = (ids: string[]) => groups.filter((g) => ids.includes(g.id)).map((g) => g.name).join(', ') || 'no groups';
const not_found = (what: string) => new ApiFailure(404, 'not_found', `not found: ${what}`);
const slug = (name: string) => name.trim().toLowerCase().replace(/[^a-z0-9]/g, '_').replace(/^_+|_+$/g, '') || 'group';
function check_name(name: string, except?: string): string {
  const clean = name.trim();
  if (!clean || clean.length > 64) throw new ApiFailure(422, 'invalid_input', 'Give the group a name of 1 to 64 characters.');
  if (groups.some((g) => g.id !== except && g.name.toLowerCase() === clean.toLowerCase())) {
    throw new ApiFailure(422, 'invalid_input', `A group named \`${clean}\` already exists.`);
  }
  return clean;
}
/** Groups in their `PERMISSIONS` order, once each, like the kernel. */
const ordered = (permissions: Permission[]) => PERMISSIONS.filter((p) => permissions.includes(p));
const me_groups = () => groups.filter((g) => me_state.groups.includes(g.id));
const my_permissions = () => (me_state.is_owner ? [...PERMISSIONS] : PERMISSIONS.filter((p) => me_groups().some((g) => g.permissions.includes(p))));
/** The mock's signed-in person, for the workspace mock: their group ids and permissions. */
export const mock_me = () => ({ groups: [...me_state.groups], permissions: my_permissions() });

function fake(method: string, path: string, body: unknown): unknown {
  const route = `${method} ${path}`;
  if (route === 'GET /auth') return { providers: sso.filter((p) => p.has_secret).map(({ id, name, kind }) => ({ id, name, kind })), local: true };
  if (route === 'GET /settings/sso') return sso_view();
  const sso_path = path.match(/^\/settings\/sso\/([^/]+)$/);
  if (sso_path) {
    const id = decodeURIComponent(sso_path[1]);
    const old = sso.find((p) => p.id === id);
    if (method === 'DELETE') {
      if (!old) throw not_found(`sign-in provider ${id}`);
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
  if (route === 'DELETE /session') return null;
  if (route === 'GET /me') {
    const must_enroll_two_step = policy.require_two_step && me_state.via === 'local' && !factors_of(me_state.user).length;
    return { ...me_state, groups: structuredClone(me_groups()), permissions: my_permissions(), needs_email: !me_state.email, must_enroll_two_step, view_role };
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
    const input = body as { email: string; username: string; group_ids?: string[]; password?: string; must_change?: boolean };
    if (input.password !== undefined) check_password(input.password);
    else if (!smtp.smtp) throw new ApiFailure(409, 'conflict', 'Configure email first: Settings → Email (SMTP).');
    const email = input.email.trim().toLowerCase();
    if (members.some((m) => m.email === email)) throw new ApiFailure(422, 'invalid_input', 'Someone with that email is already a member.');
    const ids = input.group_ids ?? [];
    const with_password = input.password !== undefined;
    const member: Member = {
      id: `m_${input.username}`, issuer: 'local', subject: email, username: input.username, email, is_owner: false, groups: ids,
      has_password: with_password, must_set_password: with_password && input.must_change !== false, deactivated_at: null,
    };
    members.push(member);
    if (input.password !== undefined) set_passwords.set(member.id, input.password);
    audit_member(member.username, `${with_password ? 'created with a password' : 'invited'} ${email} into [${group_names(ids)}]`);
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
  if (route === 'GET /groups') return structuredClone(groups);
  if (route === 'POST /groups') {
    const input = body as { name: string; permissions?: Permission[] };
    const name = check_name(input.name);
    let key = slug(name);
    for (let n = 2; groups.some((g) => g.key === key) || key === 'owner'; n++) key = `${slug(name)}_${n}`;
    const group: Group = { id: `g_${++group_seq}`, key, name, permissions: ordered(input.permissions ?? []) };
    groups.push(group);
    audit_entry('group_change', name, `created as role:${key}; permissions: ${group.permissions.join(', ')}`);
    return structuredClone(group);
  }
  const group_path = path.match(/^\/groups\/([^/]+)(\/members)?$/);
  if (group_path) {
    const group = groups.find((g) => g.id === decodeURIComponent(group_path[1]));
    if (!group) throw not_found('group');
    if (group_path[2]) {
      const ids = (body as { member_ids: string[] }).member_ids;
      for (const m of members) m.groups = m.groups.filter((g) => g !== group.id).concat(ids.includes(m.id) ? [group.id] : []);
      const now_in = members.filter((m) => m.groups.includes(group.id));
      audit_entry('group_change', group.name, `members: ${now_in.map((m) => m.username).join(', ')}`);
      return now_in.map((m) => ({ ...m }));
    }
    if (method === 'DELETE') {
      groups = groups.filter((g) => g !== group);
      for (const m of members) m.groups = m.groups.filter((g) => g !== group.id);
      me_state.groups = me_state.groups.filter((g) => g !== group.id);
      audit_entry('group_change', group.name, `deleted (was role:${group.key})`);
      return null;
    }
    const change = body as { name?: string; permissions?: Permission[] };
    if (change.name !== undefined) {
      const name = check_name(change.name, group.id);
      audit_entry('group_change', name, `renamed from ${group.name}`);
      group.name = name;
    }
    if (change.permissions) {
      group.permissions = ordered(change.permissions);
      audit_entry('group_change', group.name, `permissions: ${group.permissions.join(', ')}`);
    }
    return structuredClone(group);
  }
  if (route === 'GET /members') return members.map((m) => ({ ...m, has_two_step: factors_of(m.username).length > 0 }));
  if (route === 'GET /audit') return audit.map((a) => ({ ...a }));
  const access_path = path.match(/^\/members\/([^/]+)\/(groups|owner)$/);
  if (method === 'PUT' && access_path) {
    const member = members.find((m) => m.id === decodeURIComponent(access_path[1]));
    if (!member) throw not_found('member');
    if (access_path[2] === 'owner') {
      member.is_owner = (body as { is_owner: boolean }).is_owner;
      audit_entry('role_change', member.username, member.is_owner ? 'made owner' : 'no longer owner');
      return { ...member };
    }
    const ids = (body as { group_ids: string[] }).group_ids;
    audit_entry('role_change', member.username, `${group_names(member.groups)} -> ${group_names(ids)}`);
    member.groups = groups.filter((g) => ids.includes(g.id)).map((g) => g.id);
    return { ...member };
  }
  throw new ApiFailure(404, 'not_found', `No mock for ${route}`);
}
