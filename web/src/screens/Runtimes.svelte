<!-- Runtimes: one row per (host, CLI) with an honest state (TRD §6 copy: badge, one line, one action; "Check again"
     asks that host to re-probe), and, for deployment owners, the permission mode every new session of a CLI gets
     on every host (PUT /runtimes/{tool}/policy, audited). Bypass needs a second click.
     With manage_runtimes, "Add runtime" stores a BYO key runtime: a provider key (OpenAI-compatible, Anthropic
     or Google) run through OpenCode, Claude Code or Codex; its key is a secret of the workspace picked by name
     (POST /runtimes), and "Check" shows its readiness. With Sun Cloud connected it can also add a runtime paid
     with Sun credits: Claude Code or Codex for premium models, OpenCode with a gateway model. -->
<script lang="ts">
  import { Badge, Button, Dialog, Icon, Input, Radio, Select } from '@hryer/madlabs-design-system/svelte';
  import PageHeader from '../lib/ui/PageHeader.svelte';
  import Loaded from '../lib/ui/Loaded.svelte';
  import SecretPicker from '../lib/ui/SecretPicker.svelte';
  import { ApiFailure, api } from '../lib/api/client';
  import {
    BYO_KEY_ADAPTERS,
    PERMISSION_MODES,
    type ByoKeyAdapter,
    type ByoKeyKind,
    type PermissionMode,
    type Readiness,
    type Runtime,
    type RuntimeSource,
    type RuntimeState,
    type RuntimeUsage,
  } from '../lib/api/types';
  import { can, current_workspace, error_text, is_owner, toast } from '../lib/app_state.svelte';
  import { SOURCE_LABEL, clock, money, tokens, tool_label } from '../lib/format';

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
  const MODE: Record<PermissionMode, [string, string]> = {
    default: ['Ask first', 'Asks before edits and commands; unattended sessions get no answer.'],
    accept_edits: ['Edit files', 'Edits files freely; runs only the allowed commands.'],
    plan: ['Plan only', 'Reads and plans; changes nothing.'],
    bypass_permissions: ['Anything', 'Runs any command without asking.'],
  };

  const workspace = current_workspace();
  const list = () => api.list_runtimes(workspace);
  let load = $state(list());
  /** Tokens and cost per runtime, and for subscriptions within their quota window. Advisory. */
  let usage = $state<Record<string, RuntimeUsage>>({});
  api.usage_by_runtime().then(
    (rows) => (usage = Object.fromEntries(rows.map((u) => [u.runtime, u]))),
    () => {},
  );
  const used = (u: { input_tokens: number; output_tokens: number }) => `${tokens(u.input_tokens)} in · ${tokens(u.output_tokens)} out`;
  /** A bypass waiting for the second click. */
  let bypass = $state<Runtime | null>(null);
  let saving = $state(false);

  const options = (r: Runtime) =>
    PERMISSION_MODES.filter((m) => m !== 'bypass_permissions' || r.allow_bypass).map((m) => ({ value: m, label: MODE[m][0] }));

  function pick(r: Runtime, mode: PermissionMode) {
    if (mode === r.permission_mode) return;
    if (mode === 'bypass_permissions') bypass = r;
    else void save(r, mode);
  }

  async function save(r: Runtime, mode: PermissionMode) {
    saving = true;
    try {
      await api.set_runtime_policy(r.tool, mode);
      toast('success', `${tool_label(r.tool)}: ${MODE[mode][0].toLowerCase()}`, 'New sessions get it on every host; running ones keep theirs.');
    } catch (e) {
      toast('danger', 'Permission mode not changed', error_text(e));
    }
    saving = false;
    bypass = null;
    load = list();
  }

  function cancel_bypass() {
    bypass = null;
    load = list(); // puts the select back
  }

  // Add runtime. The kernel validates; a 422 message starts with the field to fix (`endpoint: …`).
  const FIELDS = ['name', 'adapter', 'endpoint', 'model', 'key_secret'] as const;
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
    source: 'byo_key' as RuntimeSource,
    name: '',
    kind: 'openai_compatible' as ByoKeyKind,
    adapter: 'opencode' as ByoKeyAdapter,
    endpoint: '',
    model: '',
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
  function pick_kind(kind: ByoKeyKind) {
    form.kind = kind;
    if (!BYO_KEY_ADAPTERS[kind].includes(form.adapter)) form.adapter = 'opencode';
  }

  async function add(e: SubmitEvent) {
    e.preventDefault();
    if (form.source === 'byo_runtime') return;
    creating = true;
    errors = {};
    const name = form.name.trim();
    try {
      await api.create_runtime(
        form.source === 'credits'
          ? { name, adapter: form.adapter, source: 'credits', model: form.adapter === 'opencode' ? form.model.trim() : null }
          : {
              name,
              adapter: form.adapter,
              source: 'byo_key',
              config: { kind: form.kind, endpoint: form.endpoint.trim(), model: form.model.trim(), key_secret: form.key_secret || null },
            },
        workspace,
      );
      adding = false;
      toast('success', `${name} added`, 'Use it in a workflow’s runtime.chain. Check shows whether it can take work.');
      load = list();
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
    if (!r.host) return;
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

  // Check: what the kernel can verify without starting a session.
  let checks = $state<Record<string, Readiness | null>>({});

  async function check(r: Runtime) {
    checks[r.id] = null;
    try {
      checks[r.id] = await api.runtime_readiness(r.id, workspace);
    } catch (err) {
      delete checks[r.id];
      toast('danger', `${r.name} not checked`, error_text(err));
    }
  }

  const check_text = (r: Runtime, ready: Readiness) =>
    ready.installed && ready.logged_in
      ? `Ready: ${ADAPTER_LABEL[r.adapter as ByoKeyAdapter] ?? r.adapter} is on a host and ${r.source === 'credits' ? 'Sun Cloud is connected' : 'the key settings are complete'}.`
      : (ready.detail ?? 'Not ready.');
  /** Stored runtimes (BYO key, credits) get a readiness check instead of a permission mode. */
  const stored = (r: Runtime) => !!r.config || r.source === 'credits';
</script>

<PageHeader title="Runtimes" subtitle="Agent CLIs Sun can dispatch to, and what their sessions may do.">
  {#if can('manage_runtimes')}<Button variant="ice" iconLeft="plus" onclick={open_add}>Add runtime</Button>{/if}
</PageHeader>

<div class="sun-page">
  <Loaded {load} empty="No runtimes yet. Install Claude Code, Codex or OpenCode on a host.">
    {#snippet row(r: Runtime)}
      {@const [tone, label] = badge(r)}
      {@const ready = checks[r.id]}
      {@const u = usage[r.name]}
      <article class="sun-row runtime" aria-label={r.host ? `${tool_label(r.tool)} on ${r.host.name}` : r.name}>
        <span class="sun-tile"><Icon name={r.config ? 'key-round' : r.source === 'credits' ? 'coins' : 'cpu'} size={18} /></span>
        <div class="sun-row__body">
          <div class="sun-row__title">
            {r.host ? tool_label(r.tool) : r.name}
            <span class="ml-mono" style="font-size:11px;color:var(--fg-3)">{r.host ? r.host.name : r.adapter}</span>
            <Badge {tone} dot live={r.state === 'busy'}>{label}</Badge>
            {#if r.state === 'idle' || r.state === 'busy'}
              {#if r.availability === 'outdated'}<Badge tone="warning" title={r.detail ?? undefined}>Untested version {r.version ?? ''}</Badge>{/if}
              {#if r.availability === 'auth_unknown'}<Badge tone="neutral">Login not checked</Badge>{/if}
            {/if}
          </div>
          {#if r.host}
            {@const host = r.host.name}
            {#if r.state === 'offline'}
              <p class="sun-row__meta runtime__meta">{host} hasn’t been seen since {r.last_seen_at ? clock(r.last_seen_at) : 'it joined'}.</p>
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
          {:else}
            <div class="sun-row__meta runtime__meta">
              {SOURCE_LABEL[r.source]} ·
              {#if r.config}
                <span class="ml-mono">{r.config.model}</span> at
                <span class="ml-mono">{r.config.endpoint || (r.config.kind === 'google' ? 'Google' : 'Anthropic')}</span> ·
                {r.config.key_secret ? 'key ' : 'no key'}{#if r.config.key_secret}<span class="ml-mono">{r.config.key_secret}</span>{/if}
              {:else if r.source === 'credits'}
                {#if r.model}<span class="ml-mono">{r.model}</span>{:else}the CLI’s own model{/if} through the Sun gateway
              {/if}
              {#if r.state !== 'idle' && r.detail} · {r.detail}{/if}
            </div>
          {/if}
          {#if u}
            <p class="runtime__usage">
              <Icon name="gauge" size={14} />
              <span>
                {r.host ? 'All hosts: ' : ''}{used(u.usage)}{u.usage.cost_usd ? ` · ${money(u.usage.cost_usd)}` : ''}
                {#if u.quota_window}<br />Quota window, last {u.quota_window.hours} h: {used(u.quota_window.usage)}{/if}
              </span>
            </p>
          {/if}
          {#if ready}
            <p class="runtime__check" class:runtime__check--ok={ready.installed && ready.logged_in} role="status">
              <Icon name={ready.installed && ready.logged_in ? 'circle-check' : 'circle-alert'} size={14} />
              <span>{check_text(r, ready)}</span>
            </p>
          {/if}
          {#if r.host && r.state === 'needs_login' && can('manage_runtimes')}
            <div class="runtime__action">
              <Button size="sm" variant="secondary" iconLeft="refresh-cw" loading={rechecking[r.id]} onclick={() => check_again(r)}>Check again</Button>
            </div>
          {:else if r.host && r.state === 'offline'}
            <div class="runtime__action">
              <Button size="sm" variant="ghost" iconLeft="circle-help" onclick={() => (restarting = r)}>How to restart the host</Button>
            </div>
          {:else if r.host && r.state === 'unsupported'}
            <div class="runtime__action">
              <Button size="sm" variant="ghost" iconLeft="external-link" href={support_url(r)} target="_blank" rel="noopener noreferrer">Request support</Button>
            </div>
          {/if}
        </div>
        {#if stored(r)}
          <Button class="runtime__mode" size="sm" variant="secondary" iconLeft="activity" loading={ready === null} onclick={() => check(r)}>
            Check
          </Button>
        {:else if is_owner()}
          <Select
            class="runtime__mode"
            size="sm"
            aria-label={r.host ? `Permissions for ${tool_label(r.tool)} on ${r.host.name}` : `Permissions for ${r.name}`}
            value={r.permission_mode ?? ''}
            placeholder="Host default"
            options={options(r)}
            hint={r.permission_mode ? `${MODE[r.permission_mode][1]} On every host.` : 'From each host’s sun.yaml.'}
            disabled={saving}
            onchange={(e: Event) => pick(r, (e.currentTarget as HTMLSelectElement).value as PermissionMode)}
          />
        {/if}
      </article>
    {/snippet}
  </Loaded>
</div>

<Dialog bind:open={adding} onclose={() => (adding = false)} title="Add a runtime">
  <form id="add-runtime" class="add" onsubmit={add} novalidate>
    <fieldset class="add__source">
      <legend class="ml-field__label">Source</legend>
      <Radio bind:group={form.source} name="source" value="byo_runtime" label="BYO runtime" description="A CLI you’re logged in to on a host." />
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
      <Input label="Name" mono placeholder="ollama-qwen" hint="Workflows pick it in runtime.chain." autocomplete="off" bind:value={form.name} error={errors.name} />
      <Select
        label="Provider"
        value={form.kind}
        options={KINDS}
        onchange={(e: Event) => pick_kind((e.currentTarget as HTMLSelectElement).value as ByoKeyKind)}
      />
      <Select
        label="Runs on"
        bind:value={form.adapter}
        options={BYO_KEY_ADAPTERS[form.kind].map((a) => ({ value: a, label: ADAPTER_LABEL[a] }))}
        hint={form.adapter === 'opencode' ? 'OpenCode takes any provider.' : `${ADAPTER_LABEL[form.adapter]} uses the key as its own login.`}
        error={errors.adapter}
      />
      <Input
        label="Endpoint"
        type="url"
        inputmode="url"
        mono
        placeholder={form.kind === 'openai_compatible' ? 'http://localhost:11434/v1' : 'Provider default'}
        hint={ENDPOINT_HINT[form.kind]}
        autocomplete="off"
        bind:value={form.endpoint}
        error={errors.endpoint}
      />
      <Input label="Model" mono placeholder={MODEL_EXAMPLE[form.kind]} autocomplete="off" bind:value={form.model} error={errors.model} />
      <SecretPicker scope={{ kind: 'workspace', id: workspace }} bind:value={form.key_secret} none_label="None (local endpoint)" error={errors.key_secret} />
    <!-- ponytail: Sun credits are off until the deployment sells them; the form stays for when they return. -->
    {:else if form.source === 'credits'}
      <Input label="Name" mono placeholder="claude-credits" hint="Workflows pick it in runtime.chain." autocomplete="off" bind:value={form.name} error={errors.name} />
      <Select
        label="Runs on"
        bind:value={form.adapter}
        options={CREDITS_ADAPTERS}
        hint={form.adapter === 'opencode' ? 'Open models, on the Pro and Business plans.' : 'Premium models, on the Business plan.'}
        error={errors.adapter}
      />
      {#if form.adapter === 'opencode'}
        <Input label="Model" mono placeholder="qwen3-coder" hint="A model from the Sun points table." autocomplete="off" bind:value={form.model} error={errors.model} />
      {/if}
      <p class="add__note">Each step costs points: the provider’s price plus Sun’s margin.</p>
    {:else}
      <p class="add__note">Install Claude Code, Codex or OpenCode on a host and log in there. It shows up here on its own.</p>
    {/if}
    {#if errors.form}<p class="add__error" role="alert">{errors.form}</p>{/if}
  </form>
  {#snippet footer()}
    <Button variant="secondary" onclick={() => (adding = false)}>Cancel</Button>
    <Button variant="ice" type="submit" form="add-runtime" loading={creating} disabled={form.source === 'byo_runtime'}>Add runtime</Button>
  {/snippet}
</Dialog>

{#if restarting?.host}
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

{#if bypass}
  {@const r = bypass}
  <Dialog open onclose={cancel_bypass} title="Let {tool_label(r.tool)} run anything?">
    <p class="runtime__warn">
      <Icon name="triangle-alert" size={16} />
      <span>Its sessions will run any command on the host without asking, on every host. Use it only when sessions run in a container or VM.</span>
    </p>
    {#snippet footer()}
      <Button variant="secondary" onclick={cancel_bypass}>Cancel</Button>
      <Button variant="danger" loading={saving} onclick={() => save(r, 'bypass_permissions')}>Allow anything</Button>
    {/snippet}
  </Dialog>
{/if}

<style>
  .runtime {
    align-items: center;
  }
  .runtime :global(.runtime__mode) {
    flex: 0 1 240px;
    min-width: 0;
  }
  /* Phones: the mode goes under the name, full width. */
  @media (max-width: 520px) {
    .runtime :global(.runtime__mode) {
      flex: 1 1 100%;
    }
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
  .runtime__check {
    display: flex;
    gap: var(--space-2);
    align-items: flex-start;
    margin: var(--space-2) 0 0;
    font-size: var(--size-xs);
    line-height: 1.5;
    color: var(--fg-2);
    overflow-wrap: anywhere;
  }
  .runtime__check :global(.ml-icon) {
    flex: none;
    margin-top: 2px;
    color: var(--warning-fg);
  }
  .runtime__check--ok :global(.ml-icon) {
    color: var(--success-fg);
  }
  .runtime__action {
    margin-top: var(--space-2);
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
  .runtime__warn {
    display: flex;
    gap: var(--space-2);
    margin: 0;
    font-size: 14px;
    line-height: 1.5;
    color: var(--fg-1);
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
