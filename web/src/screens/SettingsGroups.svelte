<!-- Settings → Deployment → Groups (deployment owners): deployment-wide sets of people, by hand or by verified email
     domain. A group grants nothing by itself; editor grants, host shares and the terminal policy name it. The list
     shows each group's size now and its domains; #/settings/groups/<id> opens one (SettingsGroup). Creating one asks
     for its name, then opens it to say who's in it. -->
<script lang="ts">
  import { Button, Dialog, Icon, Input, Tag } from '@hryer/madlabs-design-system/svelte';
  import Loaded from '../lib/ui/Loaded.svelte';
  import TextArea from '../lib/ui/TextArea.svelte';
  import SettingsGroup from './SettingsGroup.svelte';
  import { auth } from '../lib/api/auth';
  import type { MemberGroup } from '../lib/api/types';
  import { error_text } from '../lib/app_state.svelte';
  import { go, href, route } from '../lib/router.svelte';

  type Listed = MemberGroup & { size: number };
  /** Each group with how many people are in it now: the ones added by hand and its domains' verified ones. */
  async function load_groups(): Promise<Listed[]> {
    const groups = await auth.list_groups();
    return Promise.all(
      groups.map(async (g) => {
        const by_domain = g.domains.length ? await auth.group_preview(g.id, g.domains) : [];
        return { ...g, size: new Set([...g.member_ids, ...by_domain.map((m) => m.id)]).size };
      }),
    );
  }
  let load = $state<Promise<Listed[]>>(new Promise(() => {}));
  // Again on every return from a group: it may have changed there.
  $effect(() => {
    if (!route.sub) load = load_groups();
  });

  let creating = $state(false);
  let name = $state('');
  let description = $state('');
  let form_error = $state('');
  let saving = $state(false);

  function open_create() {
    [name, description, form_error, creating] = ['', '', '', true];
  }

  async function create(e: SubmitEvent) {
    e.preventDefault();
    saving = true;
    try {
      const group = await auth.create_group(name, description);
      creating = false;
      go('settings', 'groups', group.id);
    } catch (err) {
      form_error = error_text(err);
    }
    saving = false;
  }
</script>

{#if route.sub}
  {#key route.sub}<SettingsGroup id={route.sub} />{/key}
{:else}
  <section aria-labelledby="groups-heading" class="groups">
    <div class="groups__head">
      <p class="groups__lede" id="groups-heading">
        People across workspaces, picked by hand or by their email domain. Use a group in Editor access, Terminal and
        host sharing.
      </p>
      <Button variant="ice" size="sm" iconLeft="plus" onclick={open_create}>Create group</Button>
    </div>
    <Loaded {load} empty="Create a group for your team, for example everyone at @yourcompany.com">
      {#snippet row(g: Listed)}
        <article class="sun-row" aria-label={g.name}>
          <span class="sun-tile"><Icon name="users" size={18} /></span>
          <div class="sun-row__body">
            <a class="sun-row__title groups__link" href={href('settings', 'groups', g.id)}>{g.name}</a>
            <div class="sun-row__meta">{g.size} {g.size === 1 ? 'member' : 'members'}{g.description ? ` · ${g.description}` : ''}</div>
            {#if g.domains.length}
              <ul class="sun-chips groups__domains" aria-label="Domains of {g.name}">
                {#each g.domains as d (d)}<li><Tag size="sm"><span class="ml-mono">@{d}</span></Tag></li>{/each}
              </ul>
            {/if}
          </div>
          <Icon name="chevron-right" size={18} />
        </article>
      {/snippet}
    </Loaded>
  </section>
{/if}

{#if creating}
  <Dialog open onclose={() => (creating = false)} title="Create a group">
    <form id="create-group" class="groups__form" onsubmit={create} novalidate>
      <Input label="Name" bind:value={name} placeholder="Madlabs team" maxlength={100} error={form_error || undefined} />
      <TextArea label="Description" bind:value={description} rows={2} maxlength={1000} hint="Optional. Shown with the group." />
      <p class="groups__note">Next you say who’s in it: people by hand, or anyone with a verified email at a domain.</p>
    </form>
    {#snippet footer()}
      <Button variant="secondary" onclick={() => (creating = false)}>Cancel</Button>
      <Button type="submit" form="create-group" variant="ice" loading={saving}>Create group</Button>
    {/snippet}
  </Dialog>
{/if}

<style>
  .groups {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
  }
  .groups__head {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    justify-content: space-between;
    gap: var(--space-3);
  }
  .groups__lede,
  .groups__note {
    margin: 0;
    max-width: 60ch;
    font-size: var(--size-sm);
    line-height: 1.5;
    color: var(--fg-2);
  }
  .groups__link {
    color: var(--fg-1);
    text-decoration: none;
  }
  .groups__link:hover {
    text-decoration: underline;
  }
  .groups__domains {
    list-style: none;
    margin: var(--space-2) 0 0;
    padding: 0;
  }
  .groups__form {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
  }
</style>
