<!-- Your account, for everyone: who you are signed in as (owner or groups) with Log out, and changing your
     own password (PUT /me/password). The current password is needed when you have one, and your second step
     when the ecosystem asks for it. Below it, your 2-step methods. -->
<script lang="ts">
  import { Button, Dialog, Icon, Input } from '@hryer/madlabs-design-system/svelte';
  import { auth, type TwoStepAnswer, type TwoStepChallenge } from '../lib/api/auth';
  import { ApiFailure } from '../lib/api/client';
  import { error_text, sign_out, toast } from '../lib/app_state.svelte';
  import SignInTwoStep from './SignInTwoStep.svelte';
  import TwoStepMethods from './TwoStepMethods.svelte';

  const MIN_PASSWORD = 12;
  const me = auth.me().catch(() => null);
  let form = $state({ current: '', password: '', confirm: '' });
  let errors = $state<{ current?: string; password?: string; confirm?: string }>({});
  let saving = $state(false);
  /** The second step the ecosystem asks for to change the password. */
  let confirming = $state<TwoStepChallenge | null>(null);

  async function change(two_step?: TwoStepAnswer) {
    await auth.set_password(form.password, form.current || undefined, two_step);
    form = { current: '', password: '', confirm: '' };
    confirming = null;
    toast('success', 'Password changed', 'Use it next time you sign in.');
  }

  async function save(e: SubmitEvent) {
    e.preventDefault();
    errors = {
      password: form.password.length < MIN_PASSWORD ? `Use at least ${MIN_PASSWORD} characters.` : undefined,
      confirm: form.password !== form.confirm ? 'The two passwords don’t match.' : undefined,
    };
    if (errors.password || errors.confirm) return;
    saving = true;
    try {
      await change();
    } catch (err) {
      if (err instanceof ApiFailure && err.code === 'two_step_required') {
        confirming = await auth.my_two_step_challenge().catch((e) => ((errors = { current: error_text(e) }), null));
      } else errors = { current: error_text(err) };
    }
    saving = false;
  }
</script>

<form class="sun-row account" aria-labelledby="account-head" onsubmit={save} novalidate>
  <span class="sun-tile"><Icon name="user" size={18} /></span>
  <div class="sun-row__body">
    <div class="sun-row__title" id="account-head">Account</div>
    <div class="account__who">
      <p class="sun-row__meta account__copy">
        {#await me then m}
          {#if m}
            Signed in as <b>{m.email ?? m.user}</b> ({m.is_owner ? 'owner' : m.groups.map((g) => g.name).join(', ') || 'no groups'}).
          {/if}
        {/await}
        Change the password you sign in with.
      </p>
      <Button type="button" variant="secondary" size="sm" iconLeft="log-out" onclick={sign_out}>Log out</Button>
    </div>
    <div class="account__fields">
      <Input label="Current password" type="password" autocomplete="current-password" bind:value={form.current} error={errors.current} />
      <Input
        label="New password"
        type="password"
        autocomplete="new-password"
        bind:value={form.password}
        error={errors.password}
        hint="At least {MIN_PASSWORD} characters. A short sentence works well."
      />
      <Input label="Type it again" type="password" autocomplete="new-password" bind:value={form.confirm} error={errors.confirm} />
    </div>
    <div class="account__actions">
      <Button type="submit" variant="ice" iconLeft="key-round" loading={saving}>Change password</Button>
    </div>
  </div>
</form>

<section class="sun-row" aria-labelledby="two-step-head">
  <span class="sun-tile"><Icon name="shield-check" size={18} /></span>
  <div class="sun-row__body">
    <div class="sun-row__title" id="two-step-head">2-step verification</div>
    <p class="sun-row__meta account__copy">
      After your password, Sun asks for a code from your authenticator app or your email. Signing in with your
      company’s single sign-on skips it.
    </p>
    <div class="account__fields"><TwoStepMethods /></div>
  </div>
</section>

<Dialog open={!!confirming} onclose={() => (confirming = null)} title="Confirm it’s you">
  {#if confirming}
    <SignInTwoStep challenge={confirming} submit_label="Change password" onanswer={change} oncancel={() => (confirming = null)} />
  {/if}
</Dialog>

<style>
  .account__copy {
    margin-bottom: 0;
    line-height: 1.5;
    max-width: 60ch;
  }
  .account__who {
    display: flex;
    flex-wrap: wrap;
    align-items: flex-start;
    justify-content: space-between;
    gap: var(--space-3);
  }
  .account__fields {
    display: grid;
    gap: 14px;
    margin-top: var(--space-4);
    max-width: 480px;
  }
  .account__actions {
    display: flex;
    margin-top: var(--space-4);
  }
</style>
