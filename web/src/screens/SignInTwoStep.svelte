<!-- The second step: pick a method when there are several, then type the 6-digit code from the authenticator
     app or from the mail Sun sends (one field, paste and autofill allowed), or use a passkey (the browser's own prompt;
     hidden where the browser has none). Used after a right password at sign-in and to confirm a password change.
     `onanswer` rejects with the kernel's error, shown under the field. -->
<script lang="ts">
  import { Button, Input, Tabs } from '@hryer/madlabs-design-system/svelte';
  import { auth, type FactorKind, type TwoStepAnswer, type TwoStepChallenge } from '../lib/api/auth';
  import { error_text, toast } from '../lib/app_state.svelte';
  import { passkey_error, passkeys_supported, use_passkey } from '../lib/passkey';

  let {
    challenge,
    submit_label = 'Verify',
    onanswer,
    oncancel,
  }: {
    challenge: TwoStepChallenge;
    submit_label?: string;
    onanswer: (answer: TwoStepAnswer) => Promise<void>;
    oncancel?: () => void;
  } = $props();

  const LABELS: Partial<Record<FactorKind, [string, string]>> = {
    totp: ['Authenticator app', 'smartphone'],
    email: ['Email code', 'mail'],
    ...(passkeys_supported() ? { passkey: ['Passkey', 'key-round'] as [string, string] } : {}),
  };
  const methods = $derived(challenge.methods.filter((m) => LABELS[m]));
  let picked = $state<FactorKind | ''>('');
  const method = $derived(picked || methods[0]);
  let code = $state('');
  let error = $state('');
  let busy = $state(false);
  let sending = $state(false);
  let sent = $state(false);

  function pick(id: string) {
    picked = id as FactorKind;
    code = error = '';
  }

  async function send_code() {
    sending = true;
    error = '';
    try {
      await auth.mail_two_step_code(challenge.challenge);
      sent = true;
      toast('success', 'Code sent', 'Check your mail. The code works for 5 minutes.');
    } catch (err) {
      error = error_text(err);
    }
    sending = false;
  }

  async function answer_with_passkey() {
    const { options } = await auth.passkey_options(challenge.challenge);
    const credential = await use_passkey(options);
    await onanswer({ challenge: challenge.challenge, method: 'passkey', credential });
  }

  async function submit(e: SubmitEvent) {
    e.preventDefault();
    if (method !== 'passkey' && !/^\d{6}$/.test(code.trim())) {
      error = 'Type the 6 digits.';
      return;
    }
    busy = true;
    error = '';
    try {
      if (method === 'passkey') await answer_with_passkey();
      else await onanswer({ challenge: challenge.challenge, method, code: code.trim() });
    } catch (err) {
      error = passkey_error(err) ?? error_text(err);
      code = '';
    }
    busy = false;
  }
</script>

<form class="two-step" onsubmit={submit} novalidate>
  {#if methods.length > 1}
    <Tabs items={methods.map((m) => ({ id: m, label: LABELS[m]![0], icon: LABELS[m]![1] }))} value={method} onchange={pick} block />
  {/if}
  {#if !method}
    <p class="two-step__lead" role="alert">
      Your only 2-step method is a passkey, and this browser can’t use passkeys. Sign in with another browser, or ask an
      admin to reset your 2-step.
    </p>
  {:else if method === 'passkey'}
    <p class="two-step__lead">Use the passkey you added for Sun: your fingerprint, face, screen lock or security key.</p>
    {#if error}<p class="two-step__error" role="alert">{error}</p>{/if}
    <Button variant="ice" type="submit" block iconLeft="key-round" loading={busy}>Use passkey</Button>
  {:else}
    {#if method === 'email'}
      <p class="two-step__lead">{sent ? 'A code is on its way to your email.' : 'Sun mails you a 6-digit code.'}</p>
      <Button type="button" variant="secondary" iconLeft="mail" loading={sending} onclick={send_code}>
        {sent ? 'Send another code' : 'Email me a code'}
      </Button>
    {:else}
      <p class="two-step__lead">Open your authenticator app and type the 6-digit code it shows for Sun.</p>
    {/if}
    <!-- The field takes the focus when the step shows and on each method switch, so keyboard and screen-reader
         users land on it instead of the page top (WCAG 2.4.3). -->
    {#key method}
      <Input
        label="6-digit code"
        bind:value={code}
        autocomplete="one-time-code"
        inputmode="numeric"
        maxlength={6}
        mono
        {error}
        {@attach (input: HTMLInputElement) => input.focus()}
      />
    {/key}
    <Button variant="ice" type="submit" block loading={busy}>{submit_label}</Button>
  {/if}
  {#if oncancel}<Button type="button" variant="ghost" block onclick={oncancel}>Cancel</Button>{/if}
</form>

<style>
  .two-step {
    display: flex;
    flex-direction: column;
    gap: var(--space-4);
  }
  .two-step__lead,
  .two-step__error {
    margin: 0;
    font-size: 13px;
    line-height: 1.5;
    color: var(--fg-2);
  }
  .two-step__error {
    color: var(--danger-fg);
  }
</style>
