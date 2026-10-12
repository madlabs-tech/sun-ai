<!-- Settings → Deployment → Users (deployment owners): everyone who can sign in to this deployment. Each row
     shows Active, Invited or Deactivated and can email a reset code, set a password, reset 2-step, deactivate or
     reactivate them, and make them a deployment owner or not. "Add user" puts someone straight into a workspace
     with a role: with a one-time code by email (needs email set up) or a password set right away (no mail, for a
     deployment without SMTP). Who is in which workspace, and invitations, live on each workspace's Members page. -->
<script lang="ts">
  import { Badge, Button, Checkbox, Dialog, Icon, Input, Select, Switch, Tabs } from '@hryer/madlabs-design-system/svelte';
  import IconButton from '../lib/ui/IconButton.svelte';
  import Loaded from '../lib/ui/Loaded.svelte';
  import { auth, type Member } from '../lib/api/auth';
  import { app, current_workspace, error_text, toast } from '../lib/app_state.svelte';
  import { ROLES, ROLE_LABEL, type Role } from '../lib/role';
  import { href } from '../lib/router.svelte';

  let members = $state(auth.list_members());
  const changed = () => (members = auth.list_members());
  /** Your own password, reset code, 2-step, deactivation and owner flag live in your account settings, not here. */
  const is_me = (m: Member) => m.username === app.me?.user;

  // The deployment owner flag: they see and may do everything, everywhere.
  let crowning = $state<Member | null>(null);
  let crown = $state(false);
  let crown_busy = $state(false);

  async function save_owner() {
    if (!crowning) return;
    const m = crowning;
    crown_busy = true;
    try {
      if (crown !== m.is_owner) await auth.set_owner(m.id, crown);
      toast('success', crown ? `${m.username} is a deployment owner` : `${m.username} is no longer a deployment owner`);
      crowning = null;
    } catch (e) {
      toast('danger', 'Not changed', error_text(e));
    }
    crown_busy = false;
    changed();
  }

  const MIN_PASSWORD = 12;
  // Emailed codes need a mail server; a password set here doesn't.
  let smtp_ready = $state<boolean | null>(null);
  auth.get_smtp().then((v) => (smtp_ready = !!v.smtp), () => (smtp_ready = false));
  const INVITE_TABS = [
    { id: 'code', label: 'Email a code', icon: 'mail' },
    { id: 'password', label: 'Set password', icon: 'key-round' },
  ];
  const ROLE_OPTIONS = ROLES.map((r) => ({ value: r, label: ROLE_LABEL[r] }));
  let invite_open = $state(false);
  let invite_how = $state('code');
  let invite_form = $state({ email: '', username: '', workspace: '', role: 'member' as Role, password: '', must_change: true });
  let invite_errors = $state<{ email?: string; username?: string; password?: string }>({});
  let inviting = $state(false);

  function open_invite() {
    invite_form = { email: '', username: '', workspace: current_workspace() || app.workspaces[0]?.id || '', role: 'member', password: '', must_change: true };
    invite_how = smtp_ready ? 'code' : 'password';
    invite_errors = {};
    invite_open = true;
  }

  async function invite(e: SubmitEvent) {
    e.preventDefault();
    const email = invite_form.email.trim();
    const username = invite_form.username.trim();
    const by_password = invite_how === 'password';
    if (!by_password && !smtp_ready) return;
    invite_errors = {
      email: /^[^@\s]+@[^@\s]+\.[^@\s]+$/.test(email) ? undefined : 'Enter an email such as ana@company.com.',
      username: /^[A-Za-z0-9._-]{1,64}$/.test(username) ? undefined : 'Use 1–64 letters, digits, dots, dashes or underscores.',
      password: by_password && invite_form.password.length < MIN_PASSWORD ? `Use at least ${MIN_PASSWORD} characters.` : undefined,
    };
    if (invite_errors.email || invite_errors.username || invite_errors.password) return;
    inviting = true;
    try {
      const password = by_password ? { password: invite_form.password, must_change: invite_form.must_change } : undefined;
      await auth.invite(email, username, invite_form.workspace, invite_form.role, password);
      invite_open = false;
      if (by_password) toast('success', `${username} added`, 'Share the password with them yourself; Sun sends no mail.');
      else toast('success', `Invite sent to ${email}`, 'They sign in with the code, then choose a password.');
      changed();
    } catch (err) {
      invite_errors = { email: error_text(err) };
    }
    inviting = false;
  }

  async function reinvite(m: Member) {
    try {
      await auth.reinvite(m.id);
      toast('success', `Reset code sent to ${m.email}`, 'Their old password and sessions no longer work.');
    } catch (err) {
      toast('danger', 'Code not sent', error_text(err));
    }
    changed();
  }

  // An admin sets someone's password; by default they must change it at their next sign-in.
  let setting = $state<Member | null>(null);
  let set_form = $state({ password: '', must_change: true });
  let set_error = $state('');
  let set_busy = $state(false);

  function open_set_password(m: Member) {
    setting = m;
    set_form = { password: '', must_change: true };
    set_error = '';
  }

  async function save_password(e: SubmitEvent) {
    e.preventDefault();
    if (!setting) return;
    const m = setting;
    set_error = set_form.password.length < MIN_PASSWORD ? `Use at least ${MIN_PASSWORD} characters.` : '';
    if (set_error) return;
    set_busy = true;
    try {
      await auth.set_member_password(m.id, set_form.password, set_form.must_change);
      toast('success', `${m.username}’s password set`, 'Their sessions ended. Share the password with them yourself.');
      setting = null;
    } catch (err) {
      set_error = error_text(err);
    }
    set_busy = false;
    changed();
  }

  // An admin removes someone's second steps (they lost their phone); they sign in with the password alone.
  let resetting = $state<Member | null>(null);
  let reset_busy = $state(false);

  async function reset_two_step(m: Member) {
    reset_busy = true;
    try {
      await auth.reset_two_step(m.id);
      toast('success', `${m.username}’s 2-step reset`, 'They sign in with their password alone until they add a method.');
      resetting = null;
    } catch (err) {
      toast('danger', '2-step not reset', error_text(err));
    }
    reset_busy = false;
    changed();
  }

  let deactivating = $state<Member | null>(null);
  let active_busy = $state(false);

  async function set_active(m: Member, active: boolean) {
    active_busy = true;
    try {
      await (active ? auth.activate(m.id) : auth.deactivate(m.id));
      toast('success', active ? `${m.username} reactivated` : `${m.username} deactivated`, active ? 'They can sign in again.' : 'Their sessions ended.');
      deactivating = null;
    } catch (err) {
      toast('danger', active ? 'Not reactivated' : 'Not deactivated', error_text(err));
    }
    active_busy = false;
    changed();
  }

  /** Active, Invited (no password yet) or Deactivated, with its badge tone. */
  const status = (m: Member): [string, 'success' | 'warning' | 'neutral'] =>
    m.deactivated_at ? ['Deactivated', 'neutral'] : m.issuer === 'local' && !m.has_password ? ['Invited', 'warning'] : ['Active', 'success'];
</script>

<section aria-labelledby="members-heading" class="sun-section">
  <div class="section-head">
    <h3 id="members-heading">Everyone on this deployment</h3>
    <Button variant="ice" size="sm" iconLeft="user-plus" onclick={open_invite}>Add user</Button>
  </div>
  <Loaded load={members} empty="No one here yet. Add someone, or invite them from a workspace’s Members page.">
    {#snippet row(m: Member)}
      {@const [state, tone] = status(m)}
      <article class="sun-row person" style="align-items:center">
        <span class="sun-tile"><Icon name="user" size={18} /></span>
        <div class="sun-row__body">
          <div class="sun-row__title">
            {m.username}
            {#if m.is_owner}<Badge tone="info">Deployment owner</Badge>{/if}
            <Badge {tone} dot>{state}</Badge>
            {#if m.has_two_step}<Badge tone="info">2-step</Badge>{/if}
          </div>
          <div class="sun-row__meta">{m.email ?? 'no email yet'}</div>
        </div>
        <div class="person__actions">
          <IconButton
            icon="shield"
            label="Deployment owner: {m.username}"
            size="sm"
            disabled={is_me(m)}
            onclick={() => ((crowning = m), (crown = m.is_owner))}
          />
          {#if m.email}
            <IconButton
              icon="mail"
              label="Email a reset code to {m.username}"
              size="sm"
              disabled={!smtp_ready || is_me(m)}
              onclick={() => reinvite(m)}
            />
          {/if}
          <IconButton
            icon="key-round"
            label="Set password of {m.username}"
            size="sm"
            disabled={is_me(m)}
            onclick={() => open_set_password(m)}
          />
          {#if m.has_two_step}
            <IconButton
              icon="shield-off"
              label="Reset 2-step of {m.username}"
              size="sm"
              disabled={is_me(m)}
              onclick={() => (resetting = m)}
            />
          {/if}
          {#if m.deactivated_at}
            <IconButton
              icon="user-check"
              label="Reactivate {m.username}"
              size="sm"
              disabled={active_busy}
              onclick={() => set_active(m, true)}
            />
          {:else}
            <IconButton
              icon="user-x"
              label="Deactivate {m.username}"
              size="sm"
              disabled={is_me(m)}
              onclick={() => (deactivating = m)}
            />
          {/if}
        </div>
      </article>
    {/snippet}
  </Loaded>
</section>

<Dialog open={!!crowning} onclose={() => (crowning = null)} title="Deployment owner: {crowning?.username ?? ''}">
  <p class="sun-row__meta" style="margin:0">
    A deployment owner sees every workspace, may do everything in each, and alone manages this deployment: accounts,
    sign-in, email, security, hosts, mods and runtimes.
  </p>
  <Switch label="Deployment owner" bind:checked={crown} />
  {#snippet footer()}
    <Button variant="secondary" onclick={() => (crowning = null)}>Cancel</Button>
    <Button variant="ice" loading={crown_busy} onclick={save_owner}>Save</Button>
  {/snippet}
</Dialog>

<Dialog bind:open={invite_open} onclose={() => (invite_open = false)} title="Add user">
  <form id="invite-user" class="invite-form" onsubmit={invite} novalidate>
    <Tabs items={INVITE_TABS} bind:value={invite_how} block />
    {#if invite_how === 'code'}
      {#if smtp_ready}
        <p class="sun-row__meta" style="margin:0">They get a one-time code by email, sign in with it and choose a password.</p>
      {:else}
        <p class="sun-row__meta" style="margin:0" role="status">
          Email invites need a mail server. <a href={href('settings', 'email')}>Set up email in Settings</a>, or set a password instead.
        </p>
      {/if}
    {/if}
    <Input label="Email" type="email" autocomplete="off" bind:value={invite_form.email} error={invite_errors.email} />
    <Input label="Username" hint="Used by approvals’ who: user:&lt;name&gt;." autocomplete="off" bind:value={invite_form.username} error={invite_errors.username} />
    {#if invite_how === 'password'}
      <Input
        label="Password"
        type="password"
        autocomplete="new-password"
        bind:value={invite_form.password}
        error={invite_errors.password}
        hint="At least {MIN_PASSWORD} characters. Sun sends no mail: share it with them yourself."
      />
      <Checkbox label="Ask them to change it at first sign-in" bind:checked={invite_form.must_change} />
    {/if}
    <Select label="Workspace" options={app.workspaces.map((w) => ({ value: w.id, label: w.name }))} bind:value={invite_form.workspace} />
    <Select label="Role there" options={ROLE_OPTIONS} bind:value={invite_form.role} />
  </form>
  {#snippet footer()}
    <Button variant="secondary" onclick={() => (invite_open = false)}>Cancel</Button>
    {#if invite_how === 'password'}
      <Button variant="ice" type="submit" form="invite-user" iconLeft="user-plus" loading={inviting}>Add user</Button>
    {:else}
      <Button variant="ice" type="submit" form="invite-user" iconLeft="send" disabled={!smtp_ready} loading={inviting}>Send invite</Button>
    {/if}
  {/snippet}
</Dialog>

<Dialog open={!!setting} onclose={() => (setting = null)} title="Set password of {setting?.username ?? ''}">
  <form id="set-password" class="invite-form" onsubmit={save_password} novalidate>
    <p class="sun-row__meta" style="margin:0">Their sessions end. Sun sends no mail: share the password with them yourself.</p>
    <Input
      label="New password"
      type="password"
      autocomplete="new-password"
      bind:value={set_form.password}
      error={set_error}
      hint="At least {MIN_PASSWORD} characters."
    />
    <Checkbox label="Ask them to change it at next sign-in" bind:checked={set_form.must_change} />
  </form>
  {#snippet footer()}
    <Button variant="secondary" onclick={() => (setting = null)}>Cancel</Button>
    <Button variant="ice" type="submit" form="set-password" iconLeft="key-round" loading={set_busy}>Set password</Button>
  {/snippet}
</Dialog>

<Dialog open={!!resetting} onclose={() => (resetting = null)} title="Reset 2-step of {resetting?.username ?? ''}?">
  <p class="sun-row__meta" style="margin:0">
    Their authenticator and email codes are removed. They sign in with their password alone, then add a method again.
  </p>
  {#snippet footer()}
    <Button variant="secondary" onclick={() => (resetting = null)}>Cancel</Button>
    <Button variant="danger" iconLeft="shield-off" loading={reset_busy} onclick={() => resetting && reset_two_step(resetting)}>Reset 2-step</Button>
  {/snippet}
</Dialog>

<Dialog open={!!deactivating} onclose={() => (deactivating = null)} title="Deactivate {deactivating?.username ?? ''}?">
  <p class="sun-row__meta" style="margin:0">
    They are signed out everywhere and can’t sign in until you reactivate them. Their workspaces, roles and history stay.
  </p>
  {#snippet footer()}
    <Button variant="secondary" onclick={() => (deactivating = null)}>Cancel</Button>
    <Button variant="danger" iconLeft="user-x" loading={active_busy} onclick={() => deactivating && set_active(deactivating, false)}>Deactivate</Button>
  {/snippet}
</Dialog>

<style>
  .section-head {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    justify-content: space-between;
    gap: var(--space-3);
  }
  .person {
    flex-wrap: wrap;
  }
  .person__actions {
    display: flex;
    gap: var(--space-1);
  }
  .invite-form {
    display: flex;
    flex-direction: column;
    gap: 14px;
  }
  .sun-section {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
  }
  h3 {
    margin: var(--space-2) 0 0;
    font-family: var(--font-heading);
    font-weight: 400;
    font-size: 20px;
    color: var(--fg-1);
  }
</style>
