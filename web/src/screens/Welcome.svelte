<!-- Onboarding for someone in no workspace yet (a new sign-up, or after leaving their last one): join a workspace
     they're invited to, or create their first one (they become its owner). When the deployment lets only its
     owners create workspaces, only the invitations show, or a note to ask an admin for one. -->
<script lang="ts">
  import { Button, Card } from '@hryer/madlabs-design-system/svelte';
  import Invitations from '../lib/ui/Invitations.svelte';
  import WorkspaceForm from '../lib/ui/WorkspaceForm.svelte';
  import { app, can_create_workspace, sign_out } from '../lib/app_state.svelte';

  let busy = $state(false);
</script>

<main class="welcome">
  <Card class="welcome__card" aria-labelledby="welcome-title">
    <div class="sun-wordmark"><span class="sun-wordmark__sun">SUN</span><span class="sun-wordmark__by">by <b>MADLABS</b></span></div>
    <h1 id="welcome-title" class="welcome__title">{can_create_workspace() ? 'Create your first workspace' : 'Join a workspace'}</h1>

    {#if app.invitations.length}
      <section class="welcome__section" aria-labelledby="welcome-invites">
        <h2 id="welcome-invites" class="sun-label">You’re invited</h2>
        <Invitations />
      </section>
    {/if}

    {#if can_create_workspace()}
      <section class="welcome__section" aria-labelledby="welcome-create">
        <h2 id="welcome-create" class="sun-label">{app.invitations.length ? 'Or start your own' : 'Start your own'}</h2>
        <p class="welcome__lead">A workspace holds your team’s tasks, workflows, repos and knowledge. You’ll be its owner and can invite people.</p>
        <WorkspaceForm id="first-workspace" bind:busy />
        <Button variant="ice" type="submit" form="first-workspace" block loading={busy}>Create workspace</Button>
      </section>
    {:else if !app.invitations.length}
      <p class="welcome__lead" role="status">
        You’re not in a workspace yet, and only this deployment’s owners create them. Ask a workspace admin to invite
        {app.me?.email ?? 'you'}.
      </p>
    {/if}

    <Button variant="ghost" block onclick={sign_out}>Log out</Button>
  </Card>
</main>

<style>
  .welcome {
    min-height: 100dvh;
    display: grid;
    place-items: center;
    padding: var(--space-5) var(--space-4);
    box-sizing: border-box;
  }
  .welcome > :global(.welcome__card) {
    width: 100%;
    max-width: 480px;
    display: flex;
    flex-direction: column;
    gap: var(--space-4);
    box-sizing: border-box;
  }
  .welcome :global(.sun-wordmark) {
    padding: 0;
  }
  .welcome__title {
    margin: 0;
    font-family: var(--font-heading);
    font-weight: 400;
    font-size: 26px;
    line-height: 1.1;
    color: var(--fg-1);
  }
  .welcome__section {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
  }
  .welcome__section h2 {
    margin: 0;
  }
  .welcome__lead {
    margin: 0;
    font-size: 14px;
    line-height: 1.5;
    color: var(--fg-2);
  }
</style>
