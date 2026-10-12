<!-- Settings → <workspace> → Members: who is in the open workspace and their role (every member sees the list).
     With `edit_members` a row's role picker offers only roles at or below yours (only an owner makes an owner), and
     Remove; nobody changes someone above them. Your own row has Leave. With `invite_members`: invite by email with
     a role, and the pending invitations with Revoke, plus Copy link while this page holds the link the API gave back
     (when the deployment can't send mail). Errors (the last owner, a role above yours) show the API's words. -->
<script lang="ts">
  import { Badge, Button, Dialog, Icon, Input, Select } from '@hryer/madlabs-design-system/svelte';
  import IconButton from '../lib/ui/IconButton.svelte';
  import Loaded from '../lib/ui/Loaded.svelte';
  import { auth, type Invitation, type WorkspaceMember } from '../lib/api/auth';
  import { app, can, copy_text, current_workspace, error_text, load_memberships, load_workspace, toast, workspace_of } from '../lib/app_state.svelte';
  import { stamp } from '../lib/format';
  import { ROLES, ROLE_LABEL, covers, type Role } from '../lib/role';

  const workspace = current_workspace();
  const name = workspace_of(workspace)?.name ?? 'this workspace';
  /** Your role here; a deployment owner may act as an owner. */
  const mine = $derived<Role | undefined>(app.me?.is_owner ? 'owner' : app.memberships.find((m) => m.workspace_id === workspace)?.role);
  const grantable = $derived(ROLES.filter((r) => covers(mine, r)).map((r) => ({ value: r, label: ROLE_LABEL[r] })));
  const is_me = (m: WorkspaceMember) => m.username === app.me?.user;
  const may_change = (m: WorkspaceMember) => can('edit_members', workspace) && covers(mine, m.role) && !is_me(m);

  let people = $state(auth.workspace_members(workspace));
  let pending = $state<Promise<Invitation[]>>(can('invite_members', workspace) ? auth.invitations(workspace) : Promise.resolve([]));
  /** Links the API returned this visit, by invitation id (it keeps only their hash). */
  let links = $state<Record<string, string>>({});
  const reload = () => {
    people = auth.workspace_members(workspace);
    if (can('invite_members', workspace)) pending = auth.invitations(workspace);
  };

  async function set_role(m: WorkspaceMember, role: Role) {
    if (role === m.role) return;
    try {
      await auth.set_role(workspace, m.member_id, role);
      toast('success', `${m.username} is ${ROLE_LABEL[role].toLowerCase()} now`);
    } catch (e) {
      toast('danger', 'Role not changed', error_text(e));
    }
    reload();
  }

  let removing = $state<WorkspaceMember | null>(null);
  let leaving = $state(false);
  let busy = $state(false);

  async function remove(m: WorkspaceMember) {
    busy = true;
    try {
      await auth.remove_member(workspace, m.member_id);
      toast('success', `${m.username} removed from ${name}`);
    } catch (e) {
      toast('danger', 'Not removed', error_text(e));
    }
    removing = null;
    busy = false;
    reload();
  }

  async function leave() {
    busy = true;
    try {
      await auth.leave(workspace);
      toast('success', `You left ${name}`);
      await load_memberships();
      location.hash = '#/tasks';
      await load_workspace();
    } catch (e) {
      toast('danger', 'You’re still in it', error_text(e));
    }
    leaving = false;
    busy = false;
  }

  let invite = $state({ email: '', role: 'member' as Role });
  let invite_error = $state('');
  let inviting = $state(false);
  /** The link to pass on when the deployment couldn't mail it. */
  let fresh_link = $state<{ email: string; link: string } | null>(null);

  async function send_invite(e: SubmitEvent) {
    e.preventDefault();
    const email = invite.email.trim();
    invite_error = /^[^@\s]+@[^@\s]+\.[^@\s]+$/.test(email) ? '' : 'Enter an email such as ana@company.com.';
    if (invite_error) return;
    inviting = true;
    try {
      const sent = await auth.invite_to(workspace, email, invite.role);
      if (sent.link) {
        links[sent.invitation.id] = sent.link;
        fresh_link = { email: sent.invitation.email, link: sent.link };
      } else toast('success', `Invitation sent to ${sent.invitation.email}`, 'The link works once, for 7 days.');
      invite = { email: '', role: invite.role };
      reload();
    } catch (err) {
      invite_error = error_text(err);
    }
    inviting = false;
  }

  async function revoke(i: Invitation) {
    try {
      await auth.revoke_invitation(workspace, i.id);
      toast('success', `Invitation to ${i.email} withdrawn`, 'Its link no longer works.');
    } catch (e) {
      toast('danger', 'Not withdrawn', error_text(e));
    }
    reload();
  }
</script>

<section aria-labelledby="people-heading" class="sun-section">
  <h3 id="people-heading">People in {name}</h3>
  <Loaded load={people} empty="No one is in this workspace.">
    {#snippet row(m: WorkspaceMember)}
      <article class="sun-row person" aria-label={m.username}>
        <span class="sun-tile"><Icon name="user" size={18} /></span>
        <div class="sun-row__body">
          <div class="sun-row__title">
            {m.username}
            {#if is_me(m)}<Badge tone="info">You</Badge>{/if}
          </div>
          <div class="sun-row__meta">{m.email ?? 'no email yet'}</div>
        </div>
        <div class="person__actions">
          {#if may_change(m)}
            <Select
              aria-label="Role of {m.username}"
              size="sm"
              options={grantable}
              value={m.role}
              onchange={(e: Event) => set_role(m, (e.currentTarget as HTMLSelectElement).value as Role)}
            />
            <IconButton icon="user-x" label="Remove {m.username}" size="sm" onclick={() => (removing = m)} />
          {:else}
            <Badge>{ROLE_LABEL[m.role]}</Badge>
          {/if}
          {#if is_me(m)}
            <Button variant="secondary" size="sm" iconLeft="log-out" onclick={() => (leaving = true)}>Leave</Button>
          {/if}
        </div>
      </article>
    {/snippet}
  </Loaded>
</section>

{#if can('invite_members', workspace)}
  <section aria-labelledby="invite-heading" class="sun-section">
    <h3 id="invite-heading">Invite people</h3>
    <form class="invite" onsubmit={send_invite} novalidate>
      <Input label="Email" type="email" autocomplete="off" bind:value={invite.email} error={invite_error} />
      <Select label="Role" options={grantable} bind:value={invite.role} />
      <Button variant="ice" type="submit" iconLeft="send" loading={inviting}>Invite</Button>
    </form>
  </section>

  <section aria-labelledby="pending-heading" class="sun-section">
    <h3 id="pending-heading">Pending invitations</h3>
    <Loaded load={pending} empty="No invitations waiting.">
      {#snippet row(i: Invitation)}
        <article class="sun-row person" aria-label={i.email}>
          <span class="sun-tile"><Icon name="mail" size={18} /></span>
          <div class="sun-row__body">
            <div class="sun-row__title">{i.email} <Badge>{ROLE_LABEL[i.role]}</Badge></div>
            <div class="sun-row__meta">Works until {stamp(i.expires_at)}</div>
          </div>
          <div class="person__actions">
            {#if links[i.id]}
              <IconButton icon="link" label="Copy link for {i.email}" size="sm" onclick={() => copy_text(links[i.id], 'Link copied')} />
            {/if}
            <IconButton icon="x" label="Withdraw invitation to {i.email}" size="sm" onclick={() => revoke(i)} />
          </div>
        </article>
      {/snippet}
    </Loaded>
  </section>
{/if}

<Dialog open={!!fresh_link} onclose={() => (fresh_link = null)} title="Send the link yourself">
  <p class="sun-row__meta" style="margin:0">
    This deployment can’t send email, so {fresh_link?.email} didn’t get a mail. Send them this link: it works once, for
    7 days.
  </p>
  <Input label="Invitation link" mono readonly value={fresh_link?.link ?? ''} />
  {#snippet footer()}
    <Button variant="secondary" onclick={() => (fresh_link = null)}>Done</Button>
    <Button variant="ice" iconLeft="copy" onclick={() => fresh_link && copy_text(fresh_link.link, 'Link copied')}>Copy link</Button>
  {/snippet}
</Dialog>

<Dialog open={!!removing} onclose={() => (removing = null)} title="Remove {removing?.username ?? ''} from {name}?">
  <p class="sun-row__meta" style="margin:0">They lose access to its tasks, workflows and knowledge. Their account and history stay.</p>
  {#snippet footer()}
    <Button variant="secondary" onclick={() => (removing = null)}>Cancel</Button>
    <Button variant="danger" iconLeft="user-x" loading={busy} onclick={() => removing && remove(removing)}>Remove</Button>
  {/snippet}
</Dialog>

<Dialog open={leaving} onclose={() => (leaving = false)} title="Leave {name}?">
  <p class="sun-row__meta" style="margin:0">You lose access to it until someone invites you again.</p>
  {#snippet footer()}
    <Button variant="secondary" onclick={() => (leaving = false)}>Cancel</Button>
    <Button variant="danger" iconLeft="log-out" loading={busy} onclick={leave}>Leave</Button>
  {/snippet}
</Dialog>

<style>
  .person {
    flex-wrap: wrap;
    align-items: center;
  }
  .person__actions {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: var(--space-2);
    margin-left: auto;
  }
  .invite {
    display: flex;
    flex-direction: column;
    gap: 14px;
  }
  .invite > :global(.ml-btn) {
    align-self: flex-start;
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
