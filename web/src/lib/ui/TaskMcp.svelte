<!-- "MCP tools" of a task: one switch per registered server. On = its read-only tools
     (the default grant); ticking tools in the list makes the grant exactly those tools.
     Every change saves at once (PUT /tasks/:id/mcp); the task's next agent session gets it. -->
<script lang="ts">
  import { Badge, Button, Checkbox, Switch } from '@hryer/madlabs-design-system/svelte';
  import { api } from '../api/client';
  import type { McpGrant, McpServer, McpTool } from '../api/types';
  import { error_text, toast } from '../app_state.svelte';

  let { task_id }: { task_id: string } = $props();

  let servers = $state<McpServer[]>([]);
  let grants = $state<McpGrant[]>([]);
  let open = $state<Record<string, boolean>>({});
  let tools = $state<Record<string, McpTool[] | string>>({});
  let status = $state('');

  $effect(() => {
    const id = task_id;
    servers = [];
    // Advisory, like the context section: a kernel without the routes shows nothing.
    Promise.all([api.list_mcp_servers(), api.task_mcp(id)]).then(([all, saved]) => {
      if (id !== task_id) return;
      [servers, grants, status] = [all, saved, ''];
    }, () => {});
  });

  const grant_of = (name: string) => grants.find((g) => g.server === name);

  /** Whether `tool` is on under the server's current grant. */
  const allowed = (grant: McpGrant | undefined, tool: McpTool) =>
    !!grant && (grant.tools ? grant.tools.includes(tool.name) : tool.read_only);

  async function save(next: McpGrant[]) {
    const before = grants;
    grants = next;
    status = 'Saving…';
    try {
      grants = await api.set_task_mcp(task_id, next);
      status = 'Saved. The next agent session gets these tools.';
    } catch (e) {
      grants = before;
      status = '';
      toast('danger', 'MCP tools not saved', error_text(e));
    }
  }

  const without = (name: string) => grants.filter((g) => g.server !== name);

  function switch_server(name: string, on: boolean) {
    save(on ? [...without(name), { server: name }] : without(name));
  }

  function tick(name: string, tool: McpTool, on: boolean) {
    const list = tools[name];
    if (typeof list === 'string') return;
    const grant = grant_of(name);
    const current = list.filter((t) => allowed(grant, t)).map((t) => t.name);
    const next = on ? [...current, tool.name] : current.filter((t) => t !== tool.name);
    save([...without(name), { server: name, tools: next }]);
  }

  async function expand(name: string) {
    open[name] = !open[name];
    if (!open[name] || Array.isArray(tools[name])) return;
    tools[name] = 'Connecting…';
    try {
      tools[name] = await api.mcp_tools(name);
    } catch (e) {
      tools[name] = error_text(e);
    }
  }
</script>

{#if servers.length}
  <section aria-labelledby="mcp-head">
    <h3 class="sun-label" id="mcp-head">MCP tools</h3>
    <p class="hint">Servers this task’s agents may use. On gives the read-only tools; pick tools to choose exactly.</p>
    <ul class="servers">
      {#each servers as s (s.name)}
        {@const grant = grant_of(s.name)}
        {@const list = tools[s.name]}
        <li>
          <div class="server">
            <Switch label={s.name} checked={!!grant} onchange={(e: Event) => switch_server(s.name, (e.currentTarget as HTMLInputElement).checked)} />
            {#if grant}<Badge tone={grant.tools ? 'warning' : 'info'}>{grant.tools ? `${grant.tools.length} picked` : 'Read-only'}</Badge>{/if}
          </div>
          <Button variant="ghost" size="sm" class="sun-disclosure" iconLeft={open[s.name] ? 'chevron-down' : 'chevron-right'} aria-expanded={!!open[s.name]} onclick={() => expand(s.name)}>
            Tools of {s.name}
          </Button>
          {#if open[s.name]}
            {#if typeof list === 'string' || !list}
              <p class="hint" role="status">{list ?? ''}</p>
            {:else}
              <div class="tools" role="group" aria-label="Tools of {s.name}">
                {#each list as t (t.name)}
                  <div class="tool">
                    <Checkbox label={t.name} description={t.description || undefined} checked={allowed(grant, t)} onchange={(e: Event) => tick(s.name, t, (e.currentTarget as HTMLInputElement).checked)} />
                    {#if t.read_only}<Badge tone="info">read-only</Badge>{/if}
                  </div>
                {:else}
                  <p class="hint">It offers no tools.</p>
                {/each}
              </div>
            {/if}
          {/if}
        </li>
      {/each}
    </ul>
    {#if status}<p class="hint" role="status" aria-live="polite">{status}</p>{/if}
  </section>
{/if}

<style>
  .hint {
    margin: 4px 0 var(--space-2);
    font-size: 12px;
    color: var(--fg-3);
  }
  .servers {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    margin: 0;
    padding: 0;
    list-style: none;
  }
  .server {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    flex-wrap: wrap;
    min-height: 40px;
  }
  .tools {
    display: flex;
    flex-direction: column;
    gap: 2px;
    padding-left: var(--space-3);
  }
  .tool {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    flex-wrap: wrap;
    min-height: 40px;
    min-width: 0;
  }
</style>
