<!-- Settings → Members, two tabs. People: who can sign in and their groups (tags; the row's
     Groups button moves them). Groups (MembersGroups): roles admins can rename and customize.
     The settings shell shows the page with view_members or manage_groups. Each control shows only
     for its permission: invite_members / create_members, edit_members, deactivate_members,
     manage_groups (the Groups tab). Only an owner grants owner. The audit log is SettingsAudit.
     Invite only: "Invite user" mails a one-time code (needs email set up in Settings) or sets a password
     right away. Each row shows Active, Invited or Deactivated and can edit groups, email a reset code, set a
     password, and deactivate or reactivate the person. -->
<script lang="ts">
  import { Badge, Button, Checkbox, Dialog, Icon, Input, Tabs, Tag } from '@hryer/madlabs-design-system/svelte';
  import IconButton from '../lib/ui/IconButton.svelte';
  import Loaded from '../lib/ui/Loaded.svelte';
  import MembersGroups from './MembersGroups.svelte';
  import { auth, type Group, type Member } from '../lib/api/auth';
  import { can, error_text, toast } from '../lib/app_state.svelte';
  import { toggle } from '../lib/format';
  import { href } from '../lib/router.svelte';

  const TABS = $derived([
    { id: 'people', label: 'People', icon: 'user' },
    ...(can('manage_groups') ? [{ id: 'groups', label: 'Groups', icon: 'users' }] : []),
  ]);
  let tab = $state('people');
  const me = auth.me().catch(() => null);
  let is_owner = $state(false);
  me.then((m) => (is_owner = !!m?.is_owner));
  let members = $state(auth.list_members());
  let member_list = $state<Member[]>([]);
  let groups = $state<Group[]>([]);
  $effect(() => {
    members.then((list) => (member_list = list)).catch(() => null);
  });
  const load_groups = () => auth.list_groups().then((g) => (groups = g), () => null);
  load_groups();

  /** Reloads people and groups after a change. */
  function changed() {
    members = auth.list_members();
    load_groups();
  }

  /** Without manage_groups, only groups whose permissions you hold can be given or taken (the kernel enforces it). */
  const can_grant = (g: Group) => can('manage_groups') || g.permissions.every((p) => can(p));
  /** Your own password, reset code, 2-step and deactivation live in your account settings, not here. */
  const is_me = (m: Member, me_name: string | undefined) => m.username === me_name;
  /** Without manage_groups, nobody changes their own groups. */
  const own_groups_locked = (m: Member, me_name: string | undefined) => is_me(m, me_name) && !can('manage_groups');
  /** The kernel's take-over guard as far as this page can tell: only an owner touches an owner, and without
      manage_groups the person holds no permission you lack (the workspaces they see, the kernel checks). */
  const can_act_on = (m: Member) =>
    (!m.is_owner || is_owner) && (can('manage_groups') || groups.filter((g) => m.groups.includes(g.id)).every(can_grant));
  let me_name = $state<string | undefined>();
  me.then((m) => (me_name = m?.user));

  const group_names = (m: Member) => groups.filter((g) => m.groups.includes(g.id)).map((g) => g.name);

  // A person's groups (and, for an owner to set, the owner flag).
  let regrouping = $state<Member | null>(null);
  let regroup = $state<string[]>([]);
  let regroup_owner = $state(false);
  let regroup_busy = $state(false);

  function open_regroup(m: Member) {
    regrouping = m;
    regroup = [...m.groups];
    regroup_owner = m.is_owner;
  }

  async function save_regroup() {
    if (!regrouping) return;
    const m = regrouping;
    regroup_busy = true;
    try {
      await auth.set_member_groups(m.id, regroup);
      if (is_owner && regroup_owner !== m.is_owner) await auth.set_owner(m.id, regroup_owner);
      toast('success', `${m.username}’s groups saved`);
      regrouping = null;
    } catch (e) {
      toast('danger', 'Groups not changed', error_text(e));
    }
    regroup_busy = false;
    changed();
  }

  const MIN_PASSWORD = 12;
  // Emailed codes need a mail server; a password set here doesn't.
  let smtp_ready = $state<boolean | null>(null);
  auth.get_smtp().then((v) => (smtp_ready = !!v.smtp), () => (smtp_ready = false));
  // Emailing a code needs invite_members, setting a password create_members.
  const INVITE_TABS = $derived([
    ...(can('invite_members') ? [{ id: 'code', label: 'Email invite code', icon: 'mail' }] : []),
    ...(can('create_members') ? [{ id: 'password', label: 'Set password', icon: 'key-round' }] : []),
  ]);
  let invite_open = $state(false);
  let invite_how = $state('code');
  let invite_form = $state({ email: '', username: '', groups: [] as string[], password: '', must_change: true });
  let invite_errors = $state<{ email?: string; username?: string; password?: string }>({});
  let inviting = $state(false);

  function open_invite() {
    // The seeded Viewer and User groups, like the old default role.
    const picks = groups.filter((g) => (g.key === 'viewer' || g.key === 'member') && can_grant(g)).map((g) => g.id);
    invite_form = { email: '', username: '', groups: picks, password: '', must_change: true };
    invite_how = can('create_members') && (!smtp_ready || !can('invite_members')) ? 'password' : 'code';
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
      await auth.invite(email, username, invite_form.groups, password);
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

<Tabs variant="underline" bind:value={tab} items={TABS} />
{#if tab === 'groups'}
  <MembersGroups {groups} members={member_list} {changed} />
{:else}
<section aria-labelledby="members-heading" class="sun-section">
  <div class="section-head">
    <h3 id="members-heading">People</h3>
    {#if can('invite_members') || can('create_members')}
      <Button variant="ice" size="sm" iconLeft="user-plus" onclick={open_invite}>Invite user</Button>
    {/if}
  </div>
  <Loaded load={members} empty="No one here yet. Invite someone by email.">
    {#snippet row(m: Member)}
      {@const [state, tone] = status(m)}
      <article class="sun-row person" style="align-items:center">
        <span class="sun-tile"><Icon name="user" size={18} /></span>
        <div class="sun-row__body">
          <div class="sun-row__title">
            {m.username}
            {#if m.is_owner}<Badge tone="info">Owner</Badge>{/if}
            <Badge {tone} dot>{state}</Badge>
            {#if m.has_two_step}<Badge tone="info">2-step</Badge>{/if}
          </div>
          <div class="sun-row__meta">{m.email ?? 'no email yet'}</div>
          <div class="person__groups" aria-label="Groups of {m.username}">
            {#each group_names(m) as name (name)}<Tag size="sm">{name}</Tag>{:else}<span class="sun-row__meta">No groups: reads only</span>{/each}
          </div>
        </div>
        <div class="person__actions">
          {#if can('edit_members')}
          <IconButton
            icon="users"
            label="Change groups of {m.username}"
            size="sm"
            disabled={!can_act_on(m) || own_groups_locked(m, me_name)}
            onclick={() => open_regroup(m)}
          />
          {#if m.email}
            <IconButton
              icon="mail"
              label="Email a reset code to {m.username}"
              size="sm"
              disabled={!smtp_ready || !can_act_on(m) || is_me(m, me_name)}
              onclick={() => reinvite(m)}
            />
          {/if}
          <IconButton
            icon="key-round"
            label="Set password of {m.username}"
            size="sm"
            disabled={!can_act_on(m) || is_me(m, me_name)}
            onclick={() => open_set_password(m)}
          />
          {#if m.has_two_step}
            <IconButton
              icon="shield-off"
              label="Reset 2-step of {m.username}"
              size="sm"
              disabled={!can_act_on(m) || is_me(m, me_name)}
              onclick={() => (resetting = m)}
            />
          {/if}
          {/if}
          {#if can('deactivate_members')}
          {#if m.deactivated_at}
            <IconButton
              icon="user-check"
              label="Reactivate {m.username}"
              size="sm"
              disabled={active_busy || !can_act_on(m)}
              onclick={() => set_active(m, true)}
            />
          {:else}
            <IconButton
              icon="user-x"
              label="Deactivate {m.username}"
              size="sm"
              disabled={!can_act_on(m) || is_me(m, me_name)}
              onclick={() => (deactivating = m)}
            />
          {/if}
          {/if}
        </div>
      </article>
    {/snippet}
  </Loaded>
</section>
{/if}

<Dialog open={!!regrouping} onclose={() => (regrouping = null)} title="Groups of {regrouping?.username ?? ''}">
  <fieldset class="checks">
    <legend class="sun-row__meta">They get everything their groups allow.</legend>
    {#each groups as g (g.id)}
      <Checkbox
        label={g.name}
        description={can_grant(g) ? `role:${g.key}` : `role:${g.key} · has permissions you don’t`}
        disabled={!can_grant(g)}
        checked={regroup.includes(g.id)}
        onchange={(e: Event) => (regroup = toggle(regroup, g.id, (e.currentTarget as HTMLInputElement).checked))}
      />
    {/each}
    {#if is_owner}
      <Checkbox label="Owner" description="Can do everything, always" bind:checked={regroup_owner} />
    {/if}
  </fieldset>
  {#snippet footer()}
    <Button variant="secondary" onclick={() => (regrouping = null)}>Cancel</Button>
    <Button variant="ice" loading={regroup_busy} onclick={save_regroup}>Save groups</Button>
  {/snippet}
</Dialog>

<Dialog bind:open={invite_open} onclose={() => (invite_open = false)} title="Invite user">
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
    <fieldset class="checks">
      <legend class="sun-row__meta">Groups</legend>
      <!-- Only groups whose permissions you hold (all of them with manage_groups). -->
      {#each groups.filter(can_grant) as g (g.id)}
        <Checkbox
          label={g.name}
          checked={invite_form.groups.includes(g.id)}
          onchange={(e: Event) => (invite_form.groups = toggle(invite_form.groups, g.id, (e.currentTarget as HTMLInputElement).checked))}
        />
      {/each}
    </fieldset>
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
    They are signed out everywhere and can’t sign in until you reactivate them. Their groups and history stay.
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
  .person__groups {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-1);
    margin-top: var(--space-1);
  }
  .checks {
    margin: 0;
    padding: 0;
    border: 0;
    min-width: 0;
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
  }
  .checks legend {
    margin-bottom: var(--space-2);
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
