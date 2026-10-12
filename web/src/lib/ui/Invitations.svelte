<!-- The signed-in person's pending invitations (`app.invitations`): where to, as what, from whom, with Join and
     Decline. Joining opens that workspace. Onboarding and the workspace menu's dialog show it. -->
<script lang="ts">
  import { Button, Icon } from '@hryer/madlabs-design-system/svelte';
  import { auth, type InvitationView } from '../api/auth';
  import { app, error_text, load_memberships, load_workspace, toast } from '../app_state.svelte';
  import { ROLE_LABEL } from '../role';
  import { show_workspace } from '../router.svelte';

  let { ondone }: { ondone?: () => void } = $props();
  let busy = $state('');

  async function answer(invitation: InvitationView, join: boolean) {
    busy = invitation.id;
    try {
      if (join) await auth.accept_invitation(invitation.id);
      else await auth.decline_invitation(invitation.id);
      await load_memberships();
      await load_workspace();
      const joined = app.workspaces.find((w) => w.id === invitation.workspace_id);
      if (join && joined) show_workspace(joined.slug);
      toast('success', join ? `You joined ${invitation.workspace_name}` : 'Invitation declined');
      ondone?.();
    } catch (e) {
      toast('danger', 'That didn’t go through', error_text(e));
      await load_memberships().catch(() => {});
    }
    busy = '';
  }
</script>

<ul class="invitations" aria-label="Your invitations">
  {#each app.invitations as i (i.id)}
    <li class="sun-row invitation">
      <span class="sun-tile"><Icon name="mail" size={18} /></span>
      <div class="sun-row__body">
        <div class="sun-row__title">{i.workspace_name}</div>
        <div class="sun-row__meta">As {ROLE_LABEL[i.role]}{i.inviter ? `, from ${i.inviter}` : ''}</div>
      </div>
      <div class="invitation__actions">
        <Button variant="ghost" size="sm" disabled={!!busy} onclick={() => answer(i, false)} aria-label="Decline {i.workspace_name}">Decline</Button>
        <Button variant="ice" size="sm" loading={busy === i.id} disabled={!!busy} onclick={() => answer(i, true)} aria-label="Join {i.workspace_name}">Join</Button>
      </div>
    </li>
  {/each}
</ul>

<style>
  .invitations {
    margin: 0;
    padding: 0;
    list-style: none;
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
  }
  .invitation {
    flex-wrap: wrap;
    align-items: center;
  }
  .invitation__actions {
    display: flex;
    gap: var(--space-2);
    margin-left: auto;
  }
</style>
