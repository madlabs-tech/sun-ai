<!-- Sign-in providers, for manage_settings: Google and GitHub presets plus any OIDC provider. Each gets a
     "Continue with {name}" button on the login page. Only invited people get in (their verified email must match the
     invite), and Sun's 2-step check is skipped for them: the provider's own MFA applies. Client secrets are sealed as
     ecosystem secrets and never come back. The dialog shows the callback URL to register at the provider. -->
<script lang="ts">
  import { Badge, Button, Dialog, Icon, Input, Select } from '@hryer/madlabs-design-system/svelte';
  import IconButton from '../lib/ui/IconButton.svelte';
  import { auth, type SsoKind, type SsoProvider, type SsoView } from '../lib/api/auth';
  import { copy_text, error_text, toast } from '../lib/app_state.svelte';

  const PRESETS: { value: SsoKind; label: string }[] = [
    { value: 'google', label: 'Google' },
    { value: 'github', label: 'GitHub' },
    { value: 'oidc', label: 'Other OIDC provider' },
  ];
  const KIND_NAME: Record<SsoKind, string> = { google: 'Google', github: 'GitHub', oidc: 'OIDC' };
  const SETUP: Record<SsoKind, string> = {
    google: 'Create an OAuth client ID (web application) in the Google Cloud console, under APIs & Services → Credentials.',
    github: 'Create an OAuth App in GitHub, under Settings → Developer settings → OAuth Apps.',
    oidc: 'Create a confidential web client with the authorization code flow in your provider’s admin console.',
  };

  let view = $state<SsoView | null>(null);
  let hidden = $state(false);
  /** The provider being edited; `null` = adding a new one. `undefined` = dialog closed. */
  let editing = $state<SsoProvider | null | undefined>(undefined);
  let form = $state({ kind: 'google' as SsoKind, name: 'Google', client_id: '', client_secret: '', issuer_url: '', domains: '' });
  let errors = $state<{ name?: string; client_id?: string; client_secret?: string; issuer_url?: string }>({});
  let saving = $state(false);
  let removing = $state<SsoProvider | null>(null);
  let remove_busy = $state(false);

  auth.list_sso().then((v) => (view = v), () => (hidden = true));

  /** A new provider's id: its name as a slug, unique among the others. */
  const new_id = $derived.by(() => {
    const base = form.name.trim().toLowerCase().replace(/[^a-z0-9]+/g, '_').replace(/^[^a-z]+|_+$/g, '').slice(0, 28) || form.kind;
    const taken = new Set(view?.providers.map((p) => p.id));
    let id = base;
    for (let n = 2; taken.has(id); n++) id = `${base}_${n}`;
    return id;
  });
  const id = $derived(editing ? editing.id : new_id);
  const callback = $derived(view ? view.redirect_url.replace('{id}', id) : '');

  function open_add() {
    form = { kind: 'google', name: 'Google', client_id: '', client_secret: '', issuer_url: '', domains: '' };
    errors = {};
    editing = null;
  }

  function open_edit(p: SsoProvider) {
    form = { kind: p.kind, name: p.name, client_id: p.client_id, client_secret: '', issuer_url: p.issuer_url ?? '', domains: p.allowed_domains.join(', ') };
    errors = {};
    editing = p;
  }

  /** Picking a preset names it, unless the admin already typed their own name. */
  function pick(kind: SsoKind) {
    if (!form.name.trim() || Object.values(KIND_NAME).includes(form.name)) form.name = kind === 'oidc' ? '' : KIND_NAME[kind];
    form.kind = kind;
  }

  async function save(e: SubmitEvent) {
    e.preventDefault();
    errors = {
      name: form.name.trim() ? undefined : 'Name it: people see “Continue with {name}”.',
      client_id: form.client_id.trim() ? undefined : 'Enter the client id from the provider.',
      client_secret: editing || form.client_secret.trim() ? undefined : 'Enter the client secret from the provider.',
      issuer_url: form.kind !== 'oidc' || /^https?:\/\/\S+$/.test(form.issuer_url.trim()) ? undefined : 'Enter the issuer URL, such as https://sso.example.com/realms/acme.',
    };
    if (Object.values(errors).some(Boolean)) return;
    saving = true;
    try {
      view = await auth.put_sso(id, {
        kind: form.kind,
        name: form.name.trim(),
        client_id: form.client_id.trim(),
        issuer_url: form.kind === 'oidc' ? form.issuer_url.trim() : null,
        allowed_domains: form.domains.split(/[\s,]+/).filter(Boolean),
        ...(form.client_secret.trim() ? { client_secret: form.client_secret.trim() } : {}),
      });
      toast('success', 'Sign-in provider saved', `The login page shows “Continue with ${form.name.trim()}”.`);
      editing = undefined;
    } catch (err) {
      toast('danger', 'Sign-in provider not saved', error_text(err));
    }
    saving = false;
  }

  async function remove(p: SsoProvider) {
    remove_busy = true;
    try {
      await auth.delete_sso(p.id);
      view = await auth.list_sso();
      toast('success', `${p.name} removed`, 'People it signed in keep their accounts and can use a password.');
    } catch (err) {
      toast('danger', 'Provider not removed', error_text(err));
    }
    remove_busy = false;
    removing = null;
  }

  const copy = () => copy_text(callback, 'Callback URL copied');
</script>

{#if !hidden}
  <section class="sun-row" aria-labelledby="sso-head">
    <span class="sun-tile"><Icon name="log-in" size={18} /></span>
    <div class="sun-row__body">
      <div class="sun-row__title" id="sso-head">Sign-in</div>
      <p class="sun-row__meta sso__copy">
        Let invited people continue with Google, GitHub or your own OIDC provider. Their verified email there must match the
        invite. Sun’s 2-step check is skipped for them: the provider’s own MFA applies.
      </p>
      {#if view}
        {#if view.providers.length}
          <ul class="sso__list" aria-label="Sign-in providers">
            {#each view.providers as p (p.id)}
              <li class="sso__item">
                <div class="sso__text">
                  <span class="sso__name">{p.name} <Badge tone="neutral">{KIND_NAME[p.kind]}</Badge></span>
                  <span class="sun-row__meta ml-mono sso__meta">{p.client_id}</span>
                  <span class="sun-row__meta sso__meta">
                    {p.allowed_domains.length ? `Only ${p.allowed_domains.join(', ')}` : 'Any invited email'}
                  </span>
                </div>
                {#if !p.has_secret}<Badge tone="warning" dot>No secret</Badge>{/if}
                <IconButton icon="pencil" label="Edit {p.name}" variant="ghost" size="sm" onclick={() => open_edit(p)} />
                <IconButton icon="trash-2" label="Remove {p.name}" variant="ghost" size="sm" onclick={() => (removing = p)} />
              </li>
            {/each}
          </ul>
        {:else}
          <div class="sun-empty" role="status">No providers yet: people sign in with their email and password.</div>
        {/if}
        <div class="sso__actions"><Button variant="secondary" iconLeft="plus" onclick={open_add}>Add provider</Button></div>
      {:else}
        <div class="sun-empty" role="status">Loading…</div>
      {/if}
    </div>
  </section>
{/if}

<Dialog open={editing !== undefined} onclose={() => (editing = undefined)} title={editing ? `Edit ${editing.name}` : 'Add sign-in provider'}>
  <form id="sso-form" class="sso__form" onsubmit={save} novalidate>
    <Select label="Provider" value={form.kind} options={PRESETS} onchange={(e: Event) => pick((e.currentTarget as HTMLSelectElement).value as SsoKind)} />
    <p class="sun-row__meta sso__hint">{SETUP[form.kind]}</p>
    <Input label="Button name" hint="People see “Continue with {form.name.trim() || '…'}”." autocomplete="off" bind:value={form.name} error={errors.name} />
    {#if form.kind === 'oidc'}
      <Input label="Issuer URL" mono autocomplete="off" placeholder="https://sso.example.com/realms/acme" bind:value={form.issuer_url} error={errors.issuer_url} />
    {/if}
    <Input label="Client id" mono autocomplete="off" bind:value={form.client_id} error={errors.client_id} />
    <Input
      label="Client secret"
      type="password"
      autocomplete="new-password"
      placeholder={editing?.has_secret ? 'Leave empty to keep the saved secret' : ''}
      hint="Stored as an ecosystem secret. It is never shown again."
      bind:value={form.client_secret}
      error={errors.client_secret}
    />
    <Input
      label="Allowed email domains (optional)"
      autocomplete="off"
      placeholder="acme.com, acme.io"
      hint="Only these domains get in. Empty: anyone you invited."
      bind:value={form.domains}
    />
    <Input label="Callback URL" mono readonly value={callback} hint="Register it at the provider as the redirect URL.">
      {#snippet trailing()}<IconButton icon="copy" label="Copy callback URL" variant="ghost" size="sm" onclick={copy} />{/snippet}
    </Input>
  </form>
  {#snippet footer()}
    <Button variant="secondary" onclick={() => (editing = undefined)}>Cancel</Button>
    <Button variant="ice" type="submit" form="sso-form" loading={saving}>Save provider</Button>
  {/snippet}
</Dialog>

{#if removing}
  {@const p = removing}
  <Dialog open onclose={() => (removing = null)} title="Remove {p.name}?">
    <p class="sun-row__meta sso__hint">
      Its button leaves the login page and its secret is deleted. People it signed in keep their accounts and can sign in
      with a password.
    </p>
    {#snippet footer()}
      <Button variant="secondary" onclick={() => (removing = null)}>Keep it</Button>
      <Button variant="danger" loading={remove_busy} onclick={() => remove(p)}>Remove provider</Button>
    {/snippet}
  </Dialog>
{/if}

<style>
  .sso__copy {
    margin-bottom: 0;
    line-height: 1.5;
    max-width: 60ch;
  }
  .sso__list {
    margin: var(--space-3) 0 0;
    padding: 0;
    list-style: none;
    max-width: 560px;
  }
  .sso__item {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    min-height: 44px;
    padding: var(--space-2) 0;
    border-bottom: 1px solid var(--border-subtle);
  }
  .sso__text {
    display: flex;
    flex-direction: column;
    flex: 1;
    min-width: 0;
  }
  .sso__name {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    font-size: 14px;
    font-weight: 600;
    color: var(--fg-1);
  }
  .sso__meta {
    margin: 0;
    overflow-wrap: anywhere;
  }
  .sso__actions {
    margin-top: var(--space-4);
  }
  .sso__form {
    display: grid;
    gap: 14px;
  }
  .sso__hint {
    margin: 0;
    line-height: 1.5;
  }
</style>
