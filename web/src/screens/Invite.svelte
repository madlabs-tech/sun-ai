<!-- An invitation link, #/invite/<token>: where to, as what, from whom and until when (read without signing in).
     Signed out, the sign-in card sits below it with the invited email filled in (an invitation admits sign-up even
     when the deployment's is closed); once signed in, the same page offers Join and Decline. The API says when
     it's for another email. A used, declined, revoked or expired link reads as one that doesn't work. -->
<script lang="ts">
  import { Button, Card } from '@hryer/madlabs-design-system/svelte';
  import { auth, type InvitationView } from '../lib/api/auth';
  import { app, error_text, load_memberships, load_workspace, sign_out, toast } from '../lib/app_state.svelte';
  import { stamp } from '../lib/format';
  import { ROLE_LABEL } from '../lib/role';
  import SignIn from './SignIn.svelte';

  let { token }: { token: string } = $props();

  // svelte-ignore state_referenced_locally
  const invitation = auth.look_up_invitation(token);
  const signed_in = $derived(app.loaded && !app.signed_out && !app.gate && !!app.me);
  let error = $state('');
  let busy = $state<'' | 'join' | 'decline'>('');

  async function answer(i: InvitationView, join: boolean) {
    busy = join ? 'join' : 'decline';
    error = '';
    try {
      if (join) await auth.accept_invitation(i.id);
      else await auth.decline_invitation(i.id);
      await load_memberships();
      await load_workspace();
      const joined = app.workspaces.find((w) => w.id === i.workspace_id);
      location.hash = join && joined ? `#/tasks?workspace=${encodeURIComponent(joined.slug)}` : '#/tasks';
      toast('success', join ? `You joined ${i.workspace_name}` : 'Invitation declined');
    } catch (e) {
      error = error_text(e);
    }
    busy = '';
  }
</script>

<main class="invite">
  {#await invitation}
    <Card class="invite__card"><p class="invite__lead" role="status">Opening the invitation…</p></Card>
  {:then i}
    <Card class="invite__card" aria-labelledby="invite-title">
      <div class="sun-wordmark"><span class="sun-wordmark__sun">SUN</span><span class="sun-wordmark__by">by <b>MADLABS</b></span></div>
      <h1 id="invite-title" class="invite__title">Join {i.workspace_name}</h1>
      <p class="invite__lead">
        {i.inviter ?? 'A workspace admin'} invited <b>{i.email}</b> to join as {ROLE_LABEL[i.role]}. The link works until {stamp(i.expires_at)}.
      </p>
      {#if signed_in}
        {#if error}<p class="invite__alert" role="alert">{error}</p>{/if}
        <p class="invite__lead">Signed in as <b>{app.me?.email ?? app.me?.user}</b>.</p>
        <div class="invite__actions">
          <Button variant="ghost" disabled={!!busy} loading={busy === 'decline'} onclick={() => answer(i, false)}>Decline</Button>
          <Button variant="ice" disabled={!!busy} loading={busy === 'join'} onclick={() => answer(i, true)}>Join {i.workspace_name}</Button>
        </div>
        {#if error}<Button variant="secondary" block iconLeft="log-out" onclick={sign_out}>Sign in with another account</Button>{/if}
      {:else if app.loaded}
        <p class="invite__lead">Sign in, or create an account with {i.email}, to answer it.</p>
      {/if}
    </Card>
    {#if app.loaded && !signed_in}<SignIn embedded email={i.email} invited />{/if}
  {:catch}
    <Card class="invite__card" aria-labelledby="invite-title">
      <div class="sun-wordmark"><span class="sun-wordmark__sun">SUN</span><span class="sun-wordmark__by">by <b>MADLABS</b></span></div>
      <h1 id="invite-title" class="invite__title">This invitation doesn’t work</h1>
      <p class="invite__lead">It was used, declined or withdrawn, or it expired. Ask whoever invited you for a new link.</p>
      <Button variant="secondary" block href="#/">Open Sun</Button>
    </Card>
  {/await}
</main>

<style>
  .invite {
    min-height: 100dvh;
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: var(--space-4);
    padding: var(--space-5) var(--space-4);
    box-sizing: border-box;
  }
  .invite > :global(.invite__card) {
    width: 100%;
    max-width: 400px;
    display: flex;
    flex-direction: column;
    gap: var(--space-4);
    box-sizing: border-box;
  }
  .invite > :global(.sign-in) {
    width: 100%;
    max-width: 400px;
  }
  .invite :global(.sun-wordmark) {
    padding: 0;
  }
  .invite__title {
    margin: 0;
    font-family: var(--font-heading);
    font-weight: 400;
    font-size: 26px;
    line-height: 1.1;
    color: var(--fg-1);
    overflow-wrap: anywhere;
  }
  .invite__lead {
    margin: 0;
    font-size: 14px;
    line-height: 1.5;
    color: var(--fg-2);
    overflow-wrap: anywhere;
  }
  .invite__alert {
    margin: 0;
    padding: var(--space-3);
    border-radius: var(--radius-md);
    border: 1.5px solid var(--red-400);
    font-size: 13px;
    color: var(--fg-1);
  }
  .invite__actions {
    display: flex;
    flex-wrap: wrap;
    justify-content: flex-end;
    gap: var(--space-2);
  }
</style>
