<!-- The one sign-in screen, shown on any 401, on #/login and while the person must do something first.
     Email + password only (the emailed 6-digit code from an invite or reset goes in the password box), plus "Sign in
     with <provider>" when the kernel has OIDC. POST /session sets an HttpOnly cookie that fetch and EventSource send.
     After a code sign-in (or with an admin-set password marked "change at next sign-in") the person chooses a password
     before anything else; the first-run owner (admin@sun.local) and a member from before invites give their email first.
     A right password that needs a second step shows SignInTwoStep; when the ecosystem requires 2-step and the
     person has none, they add one (TwoStepMethods) before going on.
     #/login/failed, /denied, /invite and /seats explain a failed provider round trip. -->
<script lang="ts">
  import { Button, Card, Input } from '@hryer/madlabs-design-system/svelte';
  import { auth, sso_login_url, type TwoStepAnswer, type TwoStepChallenge } from '../lib/api/auth';
  import { app, error_text, recheck_gate, sign_out, toast } from '../lib/app_state.svelte';
  import { go, route } from '../lib/router.svelte';
  import SignInTwoStep from './SignInTwoStep.svelte';
  import TwoStepMethods from './TwoStepMethods.svelte';

  const REASONS: Record<string, string> = {
    failed: 'Sign-in with your provider didn’t finish. Try again, or use your email.',
    denied: 'Your account is deactivated, or your email domain isn’t allowed on this ecosystem. Ask an admin.',
    invite: 'Ask an admin for an invite. Only invited people can sign in, with the email the invite went to.',
    seats: 'Every seat on this ecosystem’s plan is taken. Ask an admin to free one or add seats.',
  };
  const MIN_PASSWORD = 12;
  const reason = $derived(route.page === 'login' && route.id ? REASONS[route.id] : undefined);
  /** Come back to the page that asked for sign-in (a notification's deep link), not the task list. */
  const return_to = $derived.by(() => {
    void [route.page, route.id, route.sub];
    const hash = location.hash;
    return hash.startsWith('#/') && !hash.startsWith('#/login') ? `?return_to=${encodeURIComponent(hash)}` : '';
  });
  const providers = auth.providers().catch(() => ({ providers: [], local: true }));

  let email = $state('');
  let password = $state('');
  let confirm = $state('');
  let errors = $state<{ email?: string; password?: string; confirm?: string }>({});
  let busy = $state(false);
  /** The second step a right password still needs. */
  let pending = $state<TwoStepChallenge | null>(null);
  /** The person added a second step (the required-2-step gate). */
  let enrolled = $state(false);

  async function signed_in() {
    errors = {};
    password = '';
    pending = null;
    if (route.page === 'login') go('tasks');
    await recheck_gate();
  }

  async function submit(e: SubmitEvent) {
    e.preventDefault();
    errors = {
      email: email.trim() ? undefined : 'Enter your email.',
      password: password ? undefined : 'Enter your password or the code from your mail.',
    };
    if (errors.email || errors.password) return;
    busy = true;
    try {
      const answer = await auth.sign_in(email.trim(), password);
      if (answer?.two_step) {
        pending = answer.two_step;
        password = '';
      } else await signed_in();
    } catch (err) {
      errors = { password: error_text(err) };
    }
    busy = false;
  }

  async function save_password(e: SubmitEvent) {
    e.preventDefault();
    errors = {
      password: password.length < MIN_PASSWORD ? `Use at least ${MIN_PASSWORD} characters.` : undefined,
      confirm: password !== confirm ? 'The two passwords don’t match.' : undefined,
    };
    if (errors.password || errors.confirm) return;
    busy = true;
    try {
      await auth.set_password(password);
      password = confirm = '';
      toast('success', 'Password saved', 'Sign in with your email and this password from now on.');
      await recheck_gate();
    } catch (err) {
      errors = { password: error_text(err) };
    }
    busy = false;
  }

  async function save_email(e: SubmitEvent) {
    e.preventDefault();
    errors = { email: /^[^@\s]+@[^@\s]+\.[^@\s]+$/.test(email.trim()) ? undefined : 'Enter an email such as you@company.com.' };
    if (errors.email) return;
    busy = true;
    try {
      await auth.set_email(email.trim());
      toast('success', 'Email saved');
      await recheck_gate();
    } catch (err) {
      errors = { email: error_text(err) };
    }
    busy = false;
  }

  async function second_step(answer: TwoStepAnswer) {
    await auth.sign_in_two_step(answer);
    await signed_in();
  }
</script>

<main class="sign-in">
  {#if app.gate === 'password'}
    <Card as="form" class="sign-in__card" onsubmit={save_password} aria-labelledby="sign-in-title">
      <div class="sun-wordmark"><span class="sun-wordmark__sun">SUN</span><span class="sun-wordmark__by">by <b>MADLABS</b></span></div>
      <h1 id="sign-in-title" class="sign-in__title">Choose a password</h1>
      <p class="sign-in__lead">Pick your own password before you go on. Use it for your next sign-ins.</p>
      <Input label="New password" type="password" autocomplete="new-password" bind:value={password} error={errors.password} hint="At least {MIN_PASSWORD} characters. A short sentence works well." />
      <Input label="Type it again" type="password" autocomplete="new-password" bind:value={confirm} error={errors.confirm} />
      <Button variant="ice" type="submit" block loading={busy}>Save password</Button>
      <Button variant="ghost" block onclick={sign_out}>Log out</Button>
    </Card>
  {:else if app.gate === 'email'}
    <Card as="form" class="sign-in__card" onsubmit={save_email} aria-labelledby="sign-in-title">
      <div class="sun-wordmark"><span class="sun-wordmark__sun">SUN</span><span class="sun-wordmark__by">by <b>MADLABS</b></span></div>
      <h1 id="sign-in-title" class="sign-in__title">Add your email</h1>
      <p class="sign-in__lead">You sign in with it from now on. Sun sends sign-in codes and test mails to it.</p>
      <Input label="Email" type="email" autocomplete="email" inputmode="email" bind:value={email} error={errors.email} />
      <Button variant="ice" type="submit" block loading={busy}>Save email</Button>
    </Card>
  {:else if app.gate === 'two_step'}
    <Card class="sign-in__card" aria-labelledby="sign-in-title">
      <div class="sun-wordmark"><span class="sun-wordmark__sun">SUN</span><span class="sun-wordmark__by">by <b>MADLABS</b></span></div>
      <h1 id="sign-in-title" class="sign-in__title">Turn on 2-step verification</h1>
      <p class="sign-in__lead">This ecosystem asks for a second step at sign-in. Add one to go on.</p>
      <TwoStepMethods onchange={(factors) => (enrolled = factors.length > 0)} />
      <Button variant="ice" block disabled={!enrolled} onclick={recheck_gate}>Continue</Button>
      <Button variant="ghost" block onclick={sign_out}>Log out</Button>
    </Card>
  {:else if pending}
    <Card class="sign-in__card" aria-labelledby="sign-in-title">
      <div class="sun-wordmark"><span class="sun-wordmark__sun">SUN</span><span class="sun-wordmark__by">by <b>MADLABS</b></span></div>
      <h1 id="sign-in-title" class="sign-in__title">Verify it’s you</h1>
      <SignInTwoStep challenge={pending} onanswer={second_step} oncancel={() => (pending = null)} />
    </Card>
  {:else}
    <Card as="form" class="sign-in__card" onsubmit={submit} aria-labelledby="sign-in-title">
      <div class="sun-wordmark"><span class="sun-wordmark__sun">SUN</span><span class="sun-wordmark__by">by <b>MADLABS</b></span></div>
      <h1 id="sign-in-title" class="sign-in__title">Sign in to this kernel</h1>

      {#if reason}
        <p class="sign-in__alert" role="alert">{reason}</p>
      {/if}

      {#await providers then p}
        {#each p.providers as provider (provider.id)}
          <Button variant="secondary" block class="sign-in__provider" iconLeft="log-in" href={sso_login_url(provider.id) + return_to}>Continue with {provider.name}</Button>
        {/each}
        {#if p.providers.length}<div class="sign-in__or"><span>or</span></div>{/if}
      {/await}

      <Input label="Email" type="email" autocomplete="username" inputmode="email" bind:value={email} error={errors.email} />
      <Input
        label="Password"
        type="password"
        autocomplete="current-password"
        bind:value={password}
        error={errors.password}
        hint="Got a 6-digit code by mail? Type it here."
      />
      <Button variant="ice" type="submit" block loading={busy}>Sign in</Button>
      <p class="sign-in__lead">Forgot it? Ask an admin to email you a reset code.</p>
    </Card>
  {/if}
</main>

<style>
  .sign-in {
    min-height: 100dvh;
    display: grid;
    place-items: center;
    padding: var(--space-5) var(--space-4);
    box-sizing: border-box;
  }
  .sign-in > :global(.sign-in__card) {
    width: 100%;
    max-width: 400px;
    display: flex;
    flex-direction: column;
    gap: var(--space-4);
    box-sizing: border-box;
  }
  .sign-in :global(.sun-wordmark) {
    padding: 0;
  }
  .sign-in__title {
    margin: 0;
    font-family: var(--font-heading);
    font-weight: 400;
    font-size: 26px;
    line-height: 1.1;
    color: var(--fg-1);
  }
  .sign-in__lead {
    margin: 0;
    font-size: 13px;
    line-height: 1.5;
    color: var(--fg-2);
  }
  .sign-in__alert {
    margin: 0;
    padding: var(--space-3);
    border-radius: var(--radius-md);
    border: 1.5px solid var(--red-400);
    font-size: 13px;
    color: var(--fg-1);
  }
  /* An admin names the provider: a long name wraps inside the 360 px card instead of widening it. */
  .sign-in :global(.sign-in__provider) {
    height: auto;
    min-height: var(--control-md);
    padding-block: var(--space-2);
    white-space: normal;
    overflow-wrap: anywhere;
    text-align: center;
  }
  .sign-in__or {
    display: flex;
    align-items: center;
    gap: var(--space-3);
    font-size: var(--size-xs);
    color: var(--fg-2);
  }
  .sign-in__or::before,
  .sign-in__or::after {
    content: '';
    flex: 1;
    border-top: 1.5px solid var(--border-subtle);
  }
</style>
