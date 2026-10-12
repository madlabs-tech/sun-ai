<!-- Your second sign-in steps: add an authenticator app (scan the QR code or type the key, then confirm with
     a code it shows), turn on emailed codes, add a passkey (name it, then the browser's own prompt; offered where the
     browser has passkeys, and only usable with Sun on https or localhost), remove one. In Settings → Account, and on
     the sign-in screen when the ecosystem requires 2-step and you have none (`onchange` tells it when you have one). -->
<script lang="ts">
  import { Button, Dialog, Icon, Input } from '@hryer/madlabs-design-system/svelte';
  import IconButton from '../lib/ui/IconButton.svelte';
  import { auth, type Factor, type TotpStart, type TwoStepView } from '../lib/api/auth';
  import { error_text, toast } from '../lib/app_state.svelte';
  import { create_passkey, passkey_error, passkeys_supported } from '../lib/passkey';

  let { onchange }: { onchange?: (factors: Factor[]) => void } = $props();

  let view = $state<TwoStepView | null>(null);
  let load_error = $state('');
  async function load() {
    try {
      view = await auth.my_two_step();
      onchange?.(view.factors);
    } catch (err) {
      load_error = error_text(err);
    }
  }
  load();
  const has_email = $derived(!!view?.factors.some((f) => f.kind === 'email'));
  const name_of = (f: Factor) => (f.kind === 'email' ? 'Email code' : f.label);
  const KINDS = {
    totp: ['smartphone', 'authenticator app'],
    email: ['mail', 'to your email'],
    passkey: ['key-round', 'passkey'],
  } as const;
  const can_passkey = passkeys_supported();

  let adding = $state<TotpStart | null>(null);
  let code = $state('');
  let code_error = $state('');
  let busy = $state(false);

  async function start() {
    busy = true;
    try {
      adding = await auth.start_totp('');
      code = code_error = '';
    } catch (err) {
      toast('danger', 'Authenticator not started', error_text(err));
    }
    busy = false;
  }

  async function confirm(e: SubmitEvent) {
    e.preventDefault();
    if (!adding) return;
    if (!/^\d{6}$/.test(code.trim())) {
      code_error = 'Type the 6 digits your app shows.';
      return;
    }
    busy = true;
    try {
      await auth.confirm_totp(adding.id, code.trim());
      adding = null;
      toast('success', 'Authenticator added', 'Sun asks for a code from it when you sign in.');
      await load();
    } catch (err) {
      code_error = error_text(err);
    }
    busy = false;
  }

  async function add_email() {
    try {
      await auth.add_email_two_step();
      toast('success', 'Email codes on', 'Sun mails you a code when you sign in.');
      await load();
    } catch (err) {
      toast('danger', 'Email codes not turned on', error_text(err));
    }
  }

  let naming_passkey = $state(false);
  let passkey_label = $state('');
  let passkey_problem = $state('');
  let passkey_busy = $state(false);

  function name_passkey() {
    passkey_label = passkey_problem = '';
    naming_passkey = true;
  }

  async function add_passkey(e: SubmitEvent) {
    e.preventDefault();
    passkey_busy = true;
    passkey_problem = '';
    try {
      const { challenge, options } = await auth.start_passkey();
      const credential = await create_passkey(options);
      await auth.confirm_passkey(challenge, credential, passkey_label.trim());
      naming_passkey = false;
      toast('success', 'Passkey added', 'Sun asks for it when you sign in.');
      await load();
    } catch (err) {
      passkey_problem = passkey_error(err) ?? error_text(err);
    }
    passkey_busy = false;
  }

  let removing = $state<Factor | null>(null);
  async function remove() {
    if (!removing) return;
    const f = removing;
    try {
      await auth.remove_two_step(f.id);
      toast('success', `${name_of(f)} removed`);
      removing = null;
      await load();
    } catch (err) {
      toast('danger', 'Not removed', error_text(err));
    }
  }
</script>

<div class="methods">
  {#if view}
    <ul class="methods__list" aria-label="Your 2-step methods">
      {#each view.factors as f (f.id)}
        <li class="methods__item">
          <Icon name={KINDS[f.kind][0]} size={18} />
          <span class="methods__name">{name_of(f)}</span>
          <span class="sun-row__meta methods__kind">{KINDS[f.kind][1]}</span>
          <IconButton icon="trash-2" label="Remove {name_of(f)}" size="sm" onclick={() => (removing = f)} />
        </li>
      {:else}
        <li class="sun-row__meta">No 2-step method yet.</li>
      {/each}
    </ul>
    <div class="methods__actions">
      <Button type="button" variant="secondary" iconLeft="smartphone" loading={busy && !adding} onclick={start}>Add authenticator app</Button>
      {#if !has_email}
        <Button type="button" variant="secondary" iconLeft="mail" disabled={!view.email_ready} onclick={add_email}>Turn on email codes</Button>
      {/if}
      {#if can_passkey}
        <Button type="button" variant="secondary" iconLeft="key-round" disabled={!view.passkey_ready} onclick={name_passkey}>Add passkey</Button>
      {/if}
    </div>
    {#if !has_email && !view.email_ready}
      <p class="sun-row__meta methods__note">Email codes need a mail server, which an admin sets up in Settings.</p>
    {/if}
    {#if can_passkey && !view.passkey_ready}
      <p class="sun-row__meta methods__note">Passkeys need Sun on https or localhost.</p>
    {/if}
  {:else if load_error}
    <p class="sun-row__meta" role="alert">{load_error}</p>
  {/if}
</div>

<Dialog open={!!adding} onclose={() => (adding = null)} title="Add an authenticator app">
  {#if adding}
    <form id="totp-confirm" class="totp" onsubmit={confirm} novalidate>
      <p class="sun-row__meta totp__lead">
        Scan this code with an authenticator app (such as 1Password, Google Authenticator or Authy), then type the
        6-digit code it shows.
      </p>
      <img class="totp__qr" src={adding.qr_svg} alt="QR code to scan with your authenticator app" width="200" height="200" />
      <Input label="Or type this key" value={adding.secret} readonly mono />
      <Input label="6-digit code" bind:value={code} autocomplete="one-time-code" inputmode="numeric" maxlength={6} mono error={code_error} />
    </form>
  {/if}
  {#snippet footer()}
    <Button type="button" variant="secondary" onclick={() => (adding = null)}>Cancel</Button>
    <Button variant="ice" type="submit" form="totp-confirm" iconLeft="shield-check" loading={busy}>Turn on</Button>
  {/snippet}
</Dialog>

<Dialog open={naming_passkey} onclose={() => (naming_passkey = false)} title="Add a passkey">
  <form id="passkey-add" class="totp" onsubmit={add_passkey} novalidate>
    <p class="sun-row__meta totp__lead">
      Name it after the device or password manager that keeps it. Next, your browser asks you to confirm with your
      fingerprint, face, screen lock or security key.
    </p>
    <Input label="Name" bind:value={passkey_label} placeholder="MacBook Touch ID" maxlength={64} error={passkey_problem} />
  </form>
  {#snippet footer()}
    <Button type="button" variant="secondary" onclick={() => (naming_passkey = false)}>Cancel</Button>
    <Button variant="ice" type="submit" form="passkey-add" iconLeft="key-round" loading={passkey_busy}>Create passkey</Button>
  {/snippet}
</Dialog>

<Dialog open={!!removing} onclose={() => (removing = null)} title="Remove {removing ? name_of(removing) : ''}?">
  <p class="sun-row__meta" style="margin:0">Sun stops asking for it when you sign in. You can add it again later.</p>
  {#snippet footer()}
    <Button type="button" variant="secondary" onclick={() => (removing = null)}>Cancel</Button>
    <Button type="button" variant="danger" iconLeft="trash-2" onclick={remove}>Remove</Button>
  {/snippet}
</Dialog>

<style>
  .methods,
  .methods__list {
    display: grid;
    grid-template-columns: minmax(0, 1fr);
  }
  .methods {
    gap: var(--space-3);
  }
  .methods__list {
    margin: 0;
    padding: 0;
    list-style: none;
    gap: var(--space-2);
  }
  .methods__item {
    display: flex;
    align-items: center;
    gap: var(--space-3);
    min-height: 44px;
  }
  .methods__name {
    color: var(--fg-1);
    font-weight: 500;
  }
  .methods__kind {
    margin: 0;
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .methods__actions {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-3);
  }
  .methods__note,
  .totp__lead {
    margin: 0;
  }
  .totp {
    display: grid;
    gap: var(--space-4);
  }
  .totp__qr {
    justify-self: center;
    width: 200px;
    height: 200px;
    border-radius: var(--radius-md);
    background: var(--slate-0);
  }
</style>
