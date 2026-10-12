<!-- Email (SMTP), admins only: the mail server invites go through. The password is write-only: it is sealed as a
     ecosystem secret and never comes back. "Send test mail" mails the signed-in admin. Hidden for anyone the kernel
     refuses (403). -->
<script lang="ts">
  import { Badge, Button, Icon, Input, Select } from '@hryer/madlabs-design-system/svelte';
  import { auth, type SmtpTls, type SmtpView } from '../lib/api/auth';
  import { error_text, toast } from '../lib/app_state.svelte';

  const TLS: { value: SmtpTls; label: string }[] = [
    { value: 'starttls', label: 'STARTTLS (usually port 587)' },
    { value: 'tls', label: 'TLS from the start (usually port 465)' },
  ];

  let saved = $state<SmtpView | null>(null);
  let hidden = $state(false);
  let form = $state({ host: '', port: '587', username: '', from: '', tls: 'starttls' as SmtpTls, password: '' });
  let errors = $state<{ host?: string; port?: string; from?: string }>({});
  let saving = $state(false);
  let testing = $state(false);

  function fill(view: SmtpView) {
    saved = view;
    const s = view.smtp;
    if (s) form = { host: s.host, port: String(s.port), username: s.username, from: s.from, tls: s.tls, password: '' };
  }

  auth.get_smtp().then(fill, () => (hidden = true));

  async function save(e: SubmitEvent) {
    e.preventDefault();
    const port = Number(form.port);
    errors = {
      host: /^[A-Za-z0-9.-]+$/.test(form.host.trim()) ? undefined : 'Enter a host name such as smtp.example.com.',
      port: Number.isInteger(port) && port > 0 && port < 65536 ? undefined : 'Use a port from 1 to 65535.',
      from: /^[^@\s]+@[^@\s]+\.[^@\s]+$/.test(form.from.trim()) ? undefined : 'Enter the address mails come from.',
    };
    if (errors.host || errors.port || errors.from) return;
    saving = true;
    try {
      fill(
        await auth.set_smtp({
          host: form.host.trim(),
          port,
          username: form.username.trim(),
          from: form.from.trim(),
          tls: form.tls,
          ...(form.password ? { password: form.password } : {}),
        }),
      );
      toast('success', 'Email saved', 'You can invite people now.');
    } catch (err) {
      toast('danger', 'Email not saved', error_text(err));
    }
    saving = false;
  }

  async function test() {
    testing = true;
    try {
      const { sent_to } = await auth.test_smtp();
      toast('success', 'Test mail sent', `Check the inbox of ${sent_to}.`);
    } catch (err) {
      toast('danger', 'Test mail not sent', error_text(err));
    }
    testing = false;
  }
</script>

{#if !hidden}
  <form class="sun-row smtp" aria-labelledby="smtp-head" onsubmit={save} novalidate>
    <span class="sun-tile"><Icon name="mail" size={18} /></span>
    <div class="sun-row__body">
      <div class="sun-row__title" id="smtp-head">
        Email (SMTP)
        {#if saved}<Badge tone={saved.smtp ? 'success' : 'neutral'} dot>{saved.smtp ? 'Set up' : 'Not set up'}</Badge>{/if}
      </div>
      <p class="sun-row__meta smtp__copy">Invites with their one-time sign-in code go out through this mail server.</p>
      {#if saved}
        <div class="smtp__fields">
          <Input label="Host" mono placeholder="smtp.example.com" bind:value={form.host} error={errors.host} />
          <Input label="Port" type="number" inputmode="numeric" min="1" max="65535" bind:value={form.port} error={errors.port} />
          <Select label="Security" bind:value={form.tls} options={TLS} />
          <Input label="Username (optional)" autocomplete="off" bind:value={form.username} />
          <Input
            label="Password"
            type="password"
            autocomplete="new-password"
            placeholder={saved.has_password ? 'Leave empty to keep the saved password' : ''}
            hint="Stored as an ecosystem secret. It is never shown again."
            bind:value={form.password}
          />
          <Input label="From address" type="email" placeholder="sun@example.com" bind:value={form.from} error={errors.from} />
        </div>
        <div class="smtp__actions">
          <Button type="submit" variant="ice" loading={saving}>Save</Button>
          <Button variant="secondary" iconLeft="send" disabled={!saved.smtp} loading={testing} onclick={test}>Send test mail</Button>
        </div>
      {:else}
        <div class="sun-empty" role="status">Loading…</div>
      {/if}
    </div>
  </form>
{/if}

<style>
  .smtp__copy {
    margin-bottom: 0;
    line-height: 1.5;
    max-width: 60ch;
  }
  .smtp__fields {
    display: grid;
    gap: 14px;
    margin-top: var(--space-4);
    max-width: 480px;
  }
  .smtp__actions {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-3);
    margin-top: var(--space-4);
  }
</style>
