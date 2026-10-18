<!-- Runtimes: one row per (host, CLI) with an honest state (TRD §6 copy: badge, one line, one action; "Check again"
     asks that host to re-probe). Permission modes are each agent's, not the runtime's.
     Credentials (TRD BLO-117 item 7): the workspace's key-backed ways to pay for a tool, which an agent's chain
     entry names. With manage_runtimes, "Add credential" stores a BYO key: a provider key (OpenAI-compatible,
     Anthropic or Google) run through OpenCode, Claude Code or Codex; its key is a workspace secret picked by name
     (POST /credentials) and never shown again, only whether one is set. "Check" re-reads its state; Delete asks
     first and is refused while an agent's chain names it. With Sun Cloud connected it can also add one paid with
     Sun credits: Claude Code or Codex for premium models, OpenCode with a gateway model. -->
<script lang="ts">
  import { Badge, Button, Dialog, Icon, Input, Radio, Select } from '@hryer/madlabs-design-system/svelte';
  import PageHeader from '../lib/ui/PageHeader.svelte';
  import Loaded from '../lib/ui/Loaded.svelte';
  import IconButton from '../lib/ui/IconButton.svelte';
  import SecretPicker from '../lib/ui/SecretPicker.svelte';
  import { ApiFailure, api } from '../lib/api/client';
  import {
    BYO_KEY_ADAPTERS,
    type ByoKeyAdapter,
    type ByoKeyKind,
    type CreateCredential,
    type Credential,
    type CredentialSource,
    type Runtime,
    type RuntimeState,
    type RuntimeUsage,
  } from '../lib/api/types';
  import { can, current_workspace, error_text, toast } from '../lib/app_state.svelte';
  import { SOURCE_LABEL, clock, money, stamp, tokens, tool_label } from '../lib/format';

  type Tone = 'success' | 'info' | 'warning' | 'neutral';
  /** The badge per state (TRD §6). Busy also shows its load. */
  const STATE: Record<RuntimeState, [Tone, string]> = {
    offline: ['neutral', 'Host offline'],
    needs_login: ['warning', 'Needs login'],
    limited: ['warning', 'Limit reached'],
    unsupported: ['neutral', 'Not supported yet'],
    busy: ['info', 'Busy'],
    idle: ['success', 'Ready'],
  };
  const badge = (r: Runtime): [Tone, string] =>
    r.state === 'busy' && r.load ? ['info', `Busy ${r.load.active}${r.load.max === null ? '' : '/' + r.load.max}`] : STATE[r.state];
  /** The command in the API's "Run `<command>` on <host>". */
  const login_command = (r: Runtime) => /`([^`]+)`/.exec(r.detail ?? '')?.[1] ?? null;
  const SUPPORT_URL = 'https://github.com/madlabs-tech/sun-ai/issues/new';
  const support_url = (r: Runtime) =>
    `${SUPPORT_URL}?title=${encodeURIComponent(`Support ${r.tool} ${r.version ?? ''}`.trim())}`;

  const workspace = current_workspace();
  const manage = $derived(can('manage_runtimes'));
  const list = () => api.list_runtimes(workspace);
  let load = $state(list());
  const list_credentials = () => api.list_credentials(workspace);
  let credentials = $state(list_credentials());
  /** Tokens and cost per runtime or credential, and for subscriptions within their quota window. Advisory. */
  let usage = $state<Record<string, RuntimeUsage>>({});
  api.usage_by_runtime().then(
    (rows) => (usage = Object.fromEntries(rows.map((u) => [u.runtime, u]))),
    () => {},
  );
  const used = (u: { input_tokens: number; output_tokens: number }) => `${tokens(u.input_tokens)} in · ${tokens(u.output_tokens)} out`;
  // Add credential. The kernel validates; a 422 message starts with the field to fix (`endpoint: …`).
  const FIELDS = ['name', 'default_tool', 'endpoint', 'default_model', 'key_secret'] as const;
  type Field = (typeof FIELDS)[number];
  const KINDS: { value: ByoKeyKind; label: string }[] = [
    { value: 'openai_compatible', label: 'OpenAI-compatible (OpenAI, OpenRouter, Ollama)' },
    { value: 'anthropic', label: 'Anthropic' },
    { value: 'google', label: 'Google' },
  ];
  const ADAPTER_LABEL: Record<ByoKeyAdapter, string> = { opencode: 'OpenCode', 'claude-code': 'Claude Code', codex: 'Codex' };
  /** On Sun credits: what each CLI runs and which plan it needs. */
  const CREDITS_ADAPTERS: { value: ByoKeyAdapter; label: string }[] = [
    { value: 'opencode', label: 'OpenCode (open models)' },
    { value: 'claude-code', label: 'Claude Code (premium)' },
    { value: 'codex', label: 'Codex (premium)' },
  ];
  /** Empty endpoint = the provider's own. */
  const MODEL_EXAMPLE: Record<ByoKeyKind, string> = {
    openai_compatible: 'qwen2.5-coder:7b',
    anthropic: 'claude-sonnet-4-5',
    google: 'gemini-2.5-pro',
  };
  const ENDPOINT_HINT: Record<ByoKeyKind, string> = {
    openai_compatible: 'OpenAI-compatible base URL, e.g. http://localhost:11434/v1 for Ollama',
    anthropic: 'Optional. Leave empty for api.anthropic.com.',
    google: 'Optional. Leave empty for Google’s Gemini API.',
  };
  const empty_form = () => ({
    source: 'byo_key' as CredentialSource,
    name: '',
    family: 'openai_compatible' as ByoKeyKind,
    default_tool: 'opencode' as ByoKeyAdapter,
    endpoint: '',
    default_model: '',
    key_secret: '',
  });
  let adding = $state(false);
  let creating = $state(false);
  let form = $state(empty_form());
  let errors = $state<Partial<Record<Field | 'form', string>>>({});

  function open_add() {
    form = empty_form();
    errors = {};
    adding = true;
  }

  /** A provider change keeps the CLI when it can run that key, else falls back to OpenCode. */
  function pick_kind(family: ByoKeyKind) {
    form.family = family;
    if (!BYO_KEY_ADAPTERS[family].includes(form.default_tool)) form.default_tool = 'opencode';
  }

  async function add(e: SubmitEvent) {
    e.preventDefault();
    creating = true;
    errors = {};
    const name = form.name.trim();
    const input: CreateCredential =
      form.source === 'credits'
        ? { name, source: 'credits', default_tool: form.default_tool, default_model: form.default_tool === 'opencode' ? form.default_model.trim() : null }
        : {
            name,
            source: 'byo_key',
            default_tool: form.default_tool,
            family: form.family,
            endpoint: form.endpoint.trim() || undefined,
            default_model: form.default_model.trim(),
            key_secret: form.key_secret || undefined,
          };
    try {
      await api.create_credential(input, workspace);
      adding = false;
      toast('success', `${name} added`, 'Pick it as who pays in an agent’s runtime chain. Check shows whether it can take work.');
      credentials = list_credentials();
    } catch (err) {
      const [, field, why] = /^(\w+): (.+)$/s.exec(err instanceof ApiFailure ? err.message : '') ?? [];
      errors = (FIELDS as readonly string[]).includes(field)
        ? { [field]: why[0].toUpperCase() + why.slice(1) }
        : { form: error_text(err) };
    }
    creating = false;
  }

  // Check again: the host re-probes its CLIs; the kernel answers once any change is stored. A host that
  // takes longer than 10 s counts as "No change" (its report still lands on the next refresh).
  const CHECK_WAIT_MS = 10_000;
  let rechecking = $state<Record<string, boolean>>({});
  const seen = (r: Runtime | undefined) => (r ? `${r.state}|${r.availability}|${r.detail}` : '');

  async function check_again(r: Runtime) {
    rechecking[r.id] = true;
    const waited = new Promise<'waited'>((done) => setTimeout(() => done('waited'), CHECK_WAIT_MS));
    try {
      await Promise.race([api.reprobe_host(workspace, r.host.id), waited]);
      const rows = await list();
      load = Promise.resolve(rows);
      const now = rows.find((x) => x.id === r.id);
      if (seen(now) === seen(r)) toast('info', 'No change', `${tool_label(r.tool)} on ${r.host.name} reports the same as before.`);
      else toast('success', `${tool_label(r.tool)} on ${r.host.name}: ${now ? STATE[now.state][1].toLowerCase() : 'gone'}`);
    } catch (err) {
      toast('danger', `${r.host.name} not checked`, error_text(err));
    }
    rechecking[r.id] = false;
  }

  /** "How to restart the host": the host whose steps are open. */
  let restarting = $state<Runtime | null>(null);

  // Check a credential: the kernel derives its state on every read, so this re-reads the list.
  let checking = $state<Record<string, boolean>>({});

  async function check(c: Credential) {
    checking[c.name] = true;
    try {
      const rows = await list_credentials();
      credentials = Promise.resolve(rows);
      const now = rows.find((x) => x.name === c.name);
      if (!now) toast('info', `${c.name} is gone`, 'Someone deleted it meanwhile.');
      else if (now.state === 'idle') toast('success', `${c.name}: ready`, `${tool_label(now.default_tool)} can run on it.`);
      else toast('warning', `${c.name}: ${STATE[now.state][1].toLowerCase()}`, now.detail ?? undefined);
    } catch (err) {
      toast('danger', `${c.name} not checked`, error_text(err));
    }
    checking[c.name] = false;
  }

  /** The credential whose delete confirm is open. */
  let deleting = $state<Credential | null>(null);
  let removing = $state(false);

  async function remove(c: Credential) {
    removing = true;
    try {
      await api.delete_credential(c.name, workspace);
      toast('success', `${c.name} deleted`, 'Its key stays in the workspace’s secrets.');
      credentials = list_credentials();
    } catch (err) {
      toast('danger', `${c.name} not deleted`, error_text(err));
    }
    removing = false;
    deleting = null;
  }
</script>

<PageHeader title="Runtimes" subtitle="Agent CLIs Sun can dispatch to, and the credentials that pay for them. Each agent sets what its sessions may do.">
  {#if manage}<Button variant="ice" iconLeft="plus" onclick={open_add}>Add credential</Button>{/if}
</PageHeader>

<div class="sun-page">
  <Loaded {load} empty="No runtimes yet. Install Claude Code, Codex or OpenCode on a host.">
    {#snippet row(r: Runtime)}
      {@const [tone, label] = badge(r)}
      {@const u = usage[r.name]}
      {@const host = r.host.name}
      <article class="sun-row runtime" aria-label="{tool_label(r.tool)} on {host}">
        <span class="sun-tile"><Icon name="cpu" size={18} /></span>
        <div class="sun-row__body">
          <div class="sun-row__title">
            {tool_label(r.tool)}
            <span class="ml-mono runtime__sub">{host}</span>
            <Badge {tone} dot live={r.state === 'busy'}>{label}</Badge>
            {#if r.state === 'idle' || r.state === 'busy'}
              {#if r.availability === 'outdated'}<Badge tone="warning" title={r.detail ?? undefined}>Untested version {r.version ?? ''}</Badge>{/if}
              {#if r.availability === 'auth_unknown'}<Badge tone="neutral">Login not checked</Badge>{/if}
            {/if}
          </div>
          {#if r.state === 'offline'}
            <p class="sun-row__meta runtime__meta">{host} hasn’t been seen since {r.last_seen_at ? stamp(r.last_seen_at) : 'it joined'}.</p>
          {:else if r.state === 'needs_login'}
            {@const command = login_command(r)}
            <p class="sun-row__meta runtime__meta">
              {tool_label(r.tool)} is installed but not signed in on {host}.
              {#if command}Run <code class="ml-mono">{command}</code> there.{:else}{r.detail}{/if}
            </p>
          {:else if r.state === 'limited'}
            <p class="sun-row__meta runtime__meta">This account hit its usage limit.{#if r.cooldown_until}{' '}Back at {clock(r.cooldown_until)}.{/if}</p>
          {:else if r.state === 'unsupported'}
            <p class="sun-row__meta runtime__meta">Sun found <code class="ml-mono">{r.tool}</code>{r.version ? ` ${r.version}` : ''} but can’t run it yet.</p>
          {/if}
          {#if u}{@render usage_line(u, 'All hosts: ')}{/if}
          {#if r.state === 'needs_login' && manage}
            <div class="runtime__action">
              <Button size="sm" variant="secondary" iconLeft="refresh-cw" loading={rechecking[r.id]} onclick={() => check_again(r)}>Check again</Button>
            </div>
          {:else if r.state === 'offline'}
            <div class="runtime__action">
              <Button size="sm" variant="ghost" iconLeft="circle-help" onclick={() => (restarting = r)}>How to restart the host</Button>
            </div>
          {:else if r.state === 'unsupported'}
            <div class="runtime__action">
              <Button size="sm" variant="ghost" iconLeft="external-link" href={support_url(r)} target="_blank" rel="noopener noreferrer">Request support</Button>
            </div>
          {/if}
        </div>
      </article>
    {/snippet}
  </Loaded>

  <section class="credentials" aria-labelledby="credentials-head">
    <h2 class="sun-label" id="credentials-head">Credentials</h2>
    <p class="sun-row__meta">API keys and Sun credits that pay for a tool. An agent’s runtime chain picks one per entry; it runs on any host with that tool.</p>
    <Loaded load={credentials} empty={manage ? 'No credentials yet. Add an API key or a local model endpoint.' : 'No credentials yet.'}>
      {#snippet row(c: Credential)}
        {@const [tone, label] = STATE[c.state]}
        {@const u = usage[c.name]}
        <article class="sun-row runtime" aria-label={c.name}>
          <span class="sun-tile"><Icon name={c.source === 'credits' ? 'coins' : 'key-round'} size={18} /></span>
          <div class="sun-row__body">
            <div class="sun-row__title">
              {c.name}
              <span class="ml-mono runtime__sub">{c.default_tool}</span>
              <Badge {tone} dot>{label}</Badge>
            </div>
            <div class="sun-row__meta runtime__meta">
              {SOURCE_LABEL[c.source]} ·
              {#if c.source === 'credits'}
                {#if c.default_model}<span class="ml-mono">{c.default_model}</span>{:else}the CLI’s own model{/if} through the Sun gateway
              {:else}
                <span class="ml-mono">{c.default_model}</span> at
                <span class="ml-mono">{c.endpoint || (c.family === 'google' ? 'Google' : 'Anthropic')}</span> ·
                {c.key_secret_set ? 'key set' : 'no key'}
              {/if}
              {#if c.state !== 'idle' && c.detail} · {c.detail}{/if}
            </div>
            {#if u}{@render usage_line(u, '')}{/if}
          </div>
          <div class="runtime__mode">
            <Button size="sm" variant="secondary" iconLeft="activity" loading={checking[c.name]} onclick={() => check(c)}>Check</Button>
            {#if manage}<IconButton icon="trash-2" label="Delete {c.name}" variant="ghost" size="sm" onclick={() => (deleting = c)} />{/if}
          </div>
        </article>
      {/snippet}
    </Loaded>
  </section>
</div>

{#snippet usage_line(u: RuntimeUsage, prefix: string)}
  <p class="runtime__usage">
    <Icon name="gauge" size={14} />
    <span>
      {prefix}{used(u.usage)}{u.usage.cost_usd ? ` · ${money(u.usage.cost_usd)}` : ''}
      {#if u.quota_window}<br />Quota window, last {u.quota_window.hours} h: {used(u.quota_window.usage)}{/if}
    </span>
  </p>
{/snippet}

<Dialog bind:open={adding} onclose={() => (adding = false)} title="Add a credential">
  <form id="add-credential" class="add" onsubmit={add} novalidate>
    <fieldset class="add__source">
      <legend class="ml-field__label">Who pays</legend>
      <Radio bind:group={form.source} name="source" value="byo_key" label="BYO key" description="Your API key or a local model, run by OpenCode, Claude Code or Codex." />
      <Radio
        bind:group={form.source}
        name="source"
        value="credits"
        label="Sun credits"
        description="Sun credits are not available on this deployment yet; bring your own key or runtime."
        disabled
      />
    </fieldset>
    {#if form.source === 'byo_key'}
      <Input label="Name" mono placeholder="ollama-qwen" hint="Agents pick it as who pays in their runtime chain." autocomplete="off" bind:value={form.name} error={errors.name} />
      <Select
        label="Provider"
        value={form.family}
        options={KINDS}
        onchange={(e: Event) => pick_kind((e.currentTarget as HTMLSelectElement).value as ByoKeyKind)}
      />
      <Select
        label="Runs on"
        bind:value={form.default_tool}
        options={BYO_KEY_ADAPTERS[form.family].map((a) => ({ value: a, label: ADAPTER_LABEL[a] }))}
        hint={form.default_tool === 'opencode' ? 'OpenCode takes any provider.' : `${ADAPTER_LABEL[form.default_tool]} uses the key as its own login.`}
        error={errors.default_tool}
      />
      <Input
        label="Endpoint"
        type="url"
        inputmode="url"
        mono
        placeholder={form.family === 'openai_compatible' ? 'http://localhost:11434/v1' : 'Provider default'}
        hint={ENDPOINT_HINT[form.family]}
        autocomplete="off"
        bind:value={form.endpoint}
        error={errors.endpoint}
      />
      <Input label="Model" mono placeholder={MODEL_EXAMPLE[form.family]} autocomplete="off" bind:value={form.default_model} error={errors.default_model} />
      <SecretPicker scope={{ kind: 'workspace', id: workspace }} bind:value={form.key_secret} none_label="None (local endpoint)" error={errors.key_secret} />
    <!-- ponytail: Sun credits are off until the deployment sells them; the form stays for when they return. -->
    {:else}
      <Input label="Name" mono placeholder="claude-credits" hint="Agents pick it as who pays in their runtime chain." autocomplete="off" bind:value={form.name} error={errors.name} />
      <Select
        label="Runs on"
        bind:value={form.default_tool}
        options={CREDITS_ADAPTERS}
        hint={form.default_tool === 'opencode' ? 'Open models, on the Pro and Business plans.' : 'Premium models, on the Business plan.'}
        error={errors.default_tool}
      />
      {#if form.default_tool === 'opencode'}
        <Input label="Model" mono placeholder="qwen3-coder" hint="A model from the Sun points table." autocomplete="off" bind:value={form.default_model} error={errors.default_model} />
      {/if}
      <p class="add__note">Each step costs points: the provider’s price plus Sun’s margin.</p>
    {/if}
    {#if errors.form}<p class="add__error" role="alert">{errors.form}</p>{/if}
  </form>
  {#snippet footer()}
    <Button variant="secondary" onclick={() => (adding = false)}>Cancel</Button>
    <Button variant="ice" type="submit" form="add-credential" loading={creating}>Add credential</Button>
  {/snippet}
</Dialog>

{#if deleting}
  {@const c = deleting}
  <Dialog open onclose={() => (deleting = null)} title="Delete {c.name}?">
    <p class="confirm">Agents can no longer pay with it. Its key stays in the workspace’s secrets. An agent whose chain names it has to drop it first.</p>
    {#snippet footer()}
      <Button variant="secondary" onclick={() => (deleting = null)}>Cancel</Button>
      <Button variant="danger" loading={removing} onclick={() => remove(c)}>Delete</Button>
    {/snippet}
  </Dialog>
{/if}

{#if restarting}
  {@const host = restarting.host.name}
  <Dialog open onclose={() => (restarting = null)} title="Restart {host}">
    <div class="restart">
      <p>On {host}, start the host service again:</p>
      <pre class="ml-mono">sun host start</pre>
      <p>Joined with <code class="ml-mono">--no-service</code>? Run it under your own supervisor:</p>
      <pre class="ml-mono">sun host run</pre>
      <p>It reconnects on its own and shows online here within a few seconds.</p>
    </div>
    {#snippet footer()}
      <Button variant="secondary" onclick={() => (restarting = null)}>Done</Button>
    {/snippet}
  </Dialog>
{/if}

<style>
  .runtime {
    align-items: center;
  }
  .runtime__sub {
    font-size: 11px;
    color: var(--fg-3);
  }
  .runtime__mode {
    display: flex;
    gap: var(--space-2);
    align-items: center;
    flex: none;
  }
  /* Phones: the actions go under the name. */
  @media (max-width: 520px) {
    .runtime__mode {
      flex: 1 1 100%;
      justify-content: flex-end;
    }
  }
  .credentials {
    display: grid;
    gap: var(--space-3);
    margin-top: var(--space-4);
  }
  .credentials h2,
  .credentials > p {
    margin: 0;
  }
  .runtime__meta {
    overflow-wrap: anywhere;
  }
  .runtime__usage {
    display: flex;
    gap: var(--space-2);
    align-items: flex-start;
    margin: var(--space-2) 0 0;
    font-size: var(--size-xs);
    line-height: 1.5;
    color: var(--fg-2);
  }
  .runtime__usage :global(.ml-icon) {
    flex: none;
    margin-top: 2px;
  }
  .runtime__action {
    margin-top: var(--space-2);
  }
  .confirm {
    margin: 0;
    font-size: 14px;
    line-height: 1.5;
    color: var(--fg-1);
  }
  .restart {
    display: grid;
    gap: var(--space-2);
    font-size: 14px;
    line-height: 1.5;
    color: var(--fg-1);
  }
  .restart p {
    margin: 0;
  }
  .restart pre {
    margin: 0;
    padding: var(--space-2) var(--space-3);
    border-radius: var(--radius-sm);
    background: var(--bg-subtle);
    overflow-x: auto;
  }
  .add {
    display: grid;
    gap: var(--space-4);
    min-width: 0;
  }
  .add__source {
    display: grid;
    gap: var(--space-3);
    margin: 0;
    padding: 0;
    border: 0;
    min-width: 0;
  }
  .add__note,
  .add__error {
    margin: 0;
    font-size: 14px;
    line-height: 1.5;
    color: var(--fg-2);
  }
  .add__error {
    color: var(--danger-fg);
  }
</style>
