<!-- "Add MCP server" in `workspace` (manage_plugins there): a URL with an optional bearer secret, or a command Sun
     runs sandboxed; its secrets are the workspace's. The kernel connects once before saving; then the dialog shows the
     tools it found. -->
<script lang="ts">
  import { Badge, Button, Dialog, Input, Tabs } from '@hryer/madlabs-design-system/svelte';
  import { api } from '../api/client';
  import type { McpServer, McpServerAdded } from '../api/types';
  import { error_text } from '../app_state.svelte';
  import SecretPicker from './SecretPicker.svelte';

  let { workspace, onclose, onadded }: { workspace: string; onclose: () => void; onadded: (added: McpServerAdded) => void } = $props();

  let name = $state('');
  let how = $state('url');
  let url = $state('');
  let bearer_secret = $state('');
  let command = $state('');
  let args = $state('');
  let network = $state('');
  let secrets = $state('');
  let errors = $state<Record<string, string>>({});
  let busy = $state(false);
  let added = $state<McpServerAdded | null>(null);

  // ponytail: args split on spaces; an argument with a space needs a wrapper script.
  const words = (text: string) => text.split(/\s+/).filter(Boolean);
  const list = (text: string) => text.split(/[\s,]+/).filter(Boolean);

  function server(): McpServer {
    return {
      name: name.trim(),
      transport:
        how === 'url'
          ? { type: 'url', url: url.trim(), ...(bearer_secret ? { bearer_secret } : {}) }
          : { type: 'command', command: command.trim(), args: words(args), network: list(network), secrets: list(secrets) },
    };
  }

  /** Same rules as the kernel, so most mistakes show before a round trip. */
  function check(s: McpServer): Record<string, string> {
    const found: Record<string, string> = {};
    if (!/^[a-z][a-z0-9_-]{0,63}$/.test(s.name)) found.name = 'Lowercase letters, digits, - or _, starting with a letter.';
    else if (s.name === 'sun') found.name = '“sun” is Sun’s own server. Pick another name.';
    const t = s.transport;
    if (t.type === 'url' && !/^https?:\/\/\S+$/.test(t.url)) found.url = 'Use an http:// or https:// address.';
    if (t.type === 'command' && !t.command) found.command = 'Give the program to run.';
    else if (t.type === 'command' && t.command.includes('/') && !t.command.startsWith('/')) found.command = 'Use a bare name (npx) or an absolute path.';
    return found;
  }

  /** Puts the kernel's message under the field it is about. */
  function field_of(message: string): string {
    if (/secret/i.test(message)) return how === 'url' ? 'bearer_secret' : 'secrets';
    if (/named|name /i.test(message)) return 'name';
    if (/host/i.test(message) && how === 'command') return 'network';
    return how === 'url' ? 'url' : 'command';
  }

  async function submit(e: SubmitEvent) {
    e.preventDefault();
    const s = server();
    errors = check(s);
    if (Object.keys(errors).length) return;
    busy = true;
    try {
      added = await api.add_mcp_server(s, workspace);
      onadded(added);
    } catch (err) {
      const message = error_text(err);
      errors = { [field_of(message)]: message };
    }
    busy = false;
  }
</script>

<Dialog open {onclose} title={added ? `${added.server.name} added` : 'Add MCP server'} size="md">
  {#if added}
    <p class="hint">Sun connected and found {added.tools.length} tool{added.tools.length === 1 ? '' : 's'}. Turn the server on per task; only read-only tools are on by default.</p>
    <ul class="tools" aria-label="Tools of {added.server.name}">
      {#each added.tools as t (t.name)}
        <li>
          <span class="ml-mono">{t.name}</span>
          {#if t.read_only}<Badge tone="info">read-only</Badge>{/if}
          {#if t.description}<span class="dim">{t.description}</span>{/if}
        </li>
      {/each}
    </ul>
  {:else}
    <form id="mcp-server-form" class="form" onsubmit={submit} novalidate>
      <Input label="Name" mono placeholder="docs" bind:value={name} error={errors.name} hint={errors.name ? undefined : 'How tasks and agents refer to it.'} />
      <Tabs
        block
        bind:value={how}
        items={[
          { id: 'url', label: 'URL', icon: 'globe' },
          { id: 'command', label: 'Command', icon: 'terminal' },
        ]}
      />
      {#if how === 'url'}
        <Input label="URL" mono type="url" placeholder="https://mcp.example.com/mcp" bind:value={url} error={errors.url} hint={errors.url ? undefined : 'Streamable HTTP endpoint.'} />
        <SecretPicker scope={{ kind: 'workspace', id: workspace }} label="Bearer token" none_label="No auth" hint="Sent as Authorization: Bearer. Sun keeps only its name." error={errors.bearer_secret} bind:value={bearer_secret} />
      {:else}
        <Input label="Command" mono placeholder="npx" bind:value={command} error={errors.command} hint={errors.command ? undefined : 'Runs in Sun’s sandbox: no network except the hosts below.'} />
        <Input label="Arguments" mono placeholder="-y @acme/docs-mcp" bind:value={args} hint="Separated by spaces." />
        <Input label="Network hosts" mono placeholder="registry.npmjs.org" bind:value={network} error={errors.network} hint={errors.network ? undefined : 'Hosts it may reach, separated by commas.'} />
        <Input label="Secrets" mono placeholder="DOCS_TOKEN" bind:value={secrets} error={errors.secrets} hint={errors.secrets ? undefined : 'This workspace’s secret names, passed as env vars of the same name.'} />
      {/if}
    </form>
  {/if}
  {#snippet footer()}
    {#if added}
      <Button variant="ice" onclick={onclose}>Done</Button>
    {:else}
      <Button variant="secondary" onclick={onclose}>Cancel</Button>
      <Button variant="ice" type="submit" form="mcp-server-form" loading={busy}>Connect and add</Button>
    {/if}
  {/snippet}
</Dialog>

<style>
  .form {
    display: grid;
    gap: var(--space-3);
  }
  .hint {
    margin: 0 0 var(--space-3);
    font-size: 14px;
    color: var(--fg-1);
  }
  .tools {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    margin: 0;
    padding: 0;
    list-style: none;
  }
  .tools li {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: var(--space-2);
    font-size: 13px;
    min-width: 0;
  }
  .dim {
    color: var(--fg-3);
    overflow-wrap: anywhere;
  }
</style>
