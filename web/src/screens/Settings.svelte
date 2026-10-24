<!-- Settings shell: a sub-nav ("My Account": Account and Support, then a group named after the open workspace: its General, Members
     Code, Editor access, Secrets and Plan & billing, then "Deployment": Overview and the deployment's own pages with Security / Agents sub-headings, for
     deployment owners only) and one page per entry at #/settings/<id>. Pages the signed-in person can't use are hidden;
     an unknown or hidden id opens the first visible page. Phones (< 768 px): #/settings is the list, a page has a
     back link to it. The section components own their cards; this file only picks and frames them. -->
<script lang="ts">
  import type { Component } from 'svelte';
  import { Badge, Icon } from '@hryer/madlabs-design-system/svelte';
  import PageHeader from '../lib/ui/PageHeader.svelte';
  import SettingsAccount from './SettingsAccount.svelte';
  import SettingsGeneral from './SettingsGeneral.svelte';
  import Members from './Members.svelte';
  import SettingsAudit from './SettingsAudit.svelte';
  import SettingsPlan from './SettingsPlan.svelte';
  import SettingsSupport from './SettingsSupport.svelte';
  import SettingsSignIn from './SettingsSignIn.svelte';
  import SettingsTwoStep from './SettingsTwoStep.svelte';
  import SettingsBypass from './SettingsBypass.svelte';
  import SettingsEmail from './SettingsEmail.svelte';
  import SettingsSecrets from './SettingsSecrets.svelte';
  import SettingsLimit from './SettingsLimit.svelte';
  import SettingsSpend from './SettingsSpend.svelte';
  import SettingsEmbeddings from './SettingsEmbeddings.svelte';
  import SettingsWorkspace from './SettingsWorkspace.svelte';
  import SettingsCode from './SettingsCode.svelte';
  import SettingsEditor from './SettingsEditor.svelte';
  import SettingsAccounts from './SettingsAccounts.svelte';
  import { can, current_workspace, feature_on, is_owner, workspace_of } from '../lib/app_state.svelte';
  import { href, route } from '../lib/router.svelte';

  type Group = 'account' | 'workspace' | 'deployment';
  interface SettingsPage {
    id: string;
    label: string;
    icon: string;
    group: Group;
    subgroup?: string;
    show: () => boolean;
    parts: Component[];
    /** Given to each part. */
    props?: Record<string, unknown>;
  }

  const always = () => true;
  /** The deployment's own settings: its owners only (the kernel answers 403 to anyone else). */
  const owner = is_owner;
  const workspace = $derived(workspace_of(current_workspace()));
  /** The open workspace's pages: only with a workspace, and the permission a page needs. */
  const in_workspace = (permission?: 'edit_workspace' | 'edit_members' | 'manage_secrets') => () => !!workspace && (!permission || can(permission));

  const PAGES: SettingsPage[] = [
    { id: 'account', label: 'Account', icon: 'user', group: 'account', show: always, parts: [SettingsAccount] },
    // Anyone signed in asks; Sun Cloud answers those who pay for a workspace plan.
    { id: 'support', label: 'Support', icon: 'life-buoy', group: 'account', show: always, parts: [SettingsSupport] },
    { id: 'workspace', label: 'General', icon: 'settings', group: 'workspace', show: in_workspace(), parts: [SettingsWorkspace] },
    { id: 'members', label: 'Members', icon: 'users', group: 'workspace', show: in_workspace(), parts: [Members] },
    { id: 'code', label: 'Code', icon: 'git-branch', group: 'workspace', show: in_workspace('edit_workspace'), parts: [SettingsCode] },
    { id: 'editor', label: 'Editor access', icon: 'file-code', group: 'workspace', show: in_workspace('edit_members'), parts: [SettingsEditor] },
    { id: 'secrets', label: 'Secrets', icon: 'key-round', group: 'workspace', show: in_workspace('manage_secrets'), parts: [SettingsSecrets], get props() { return { workspace: current_workspace() }; } },
    // With Sun Cloud billing the page also checks out, manages and buys seats.
    { id: 'plan', get label() { return feature_on('billing') ? 'Plan & billing' : 'Plan'; }, icon: 'shield-check', group: 'workspace', show: in_workspace(), parts: [SettingsPlan] },
    { id: 'overview', label: 'Overview', icon: 'settings', group: 'deployment', show: owner, parts: [SettingsGeneral] },
    { id: 'users', label: 'Users', icon: 'user-cog', group: 'deployment', show: owner, parts: [SettingsAccounts] },
    { id: 'audit', label: 'Audit log', icon: 'scroll-text', group: 'deployment', show: owner, parts: [SettingsAudit] },
    { id: 'sign-in', label: 'Sign-in & 2-step', icon: 'shield-check', group: 'deployment', subgroup: 'Security', show: owner, parts: [SettingsSignIn, SettingsTwoStep] },
    { id: 'bypass', label: 'Bypass permissions', icon: 'shield-off', group: 'deployment', subgroup: 'Security', show: owner, parts: [SettingsBypass] },
    { id: 'email', label: 'Email', icon: 'mail', group: 'deployment', subgroup: 'Security', show: owner, parts: [SettingsEmail] },
    { id: 'deployment-secrets', label: 'Deployment secrets', icon: 'key-round', group: 'deployment', subgroup: 'Security', show: owner, parts: [SettingsSecrets] },
    { id: 'usage', label: 'Usage limits', icon: 'gauge', group: 'deployment', subgroup: 'Agents', show: owner, parts: [SettingsLimit, SettingsSpend] },
    { id: 'embeddings', label: 'Embeddings', icon: 'sparkles', group: 'deployment', subgroup: 'Agents', show: () => feature_on('knowledge') && owner(), parts: [SettingsEmbeddings] },
  ];

  /** Nav groups and the scope badge their pages carry. */
  const GROUPS: { id: Group; label: string; scope: string; scope_icon: string }[] = $derived([
    { id: 'account', label: 'My Account', scope: 'Account', scope_icon: 'user' },
    { id: 'workspace', label: workspace?.name ?? 'Workspace', scope: 'Workspace', scope_icon: 'layers' },
    { id: 'deployment', label: 'Deployment', scope: 'Deployment', scope_icon: 'building-2' },
  ]);

  // Derived: /me (and so can()) may arrive after this page opens.
  const shown = $derived(PAGES.filter((p) => p.show()));
  const active = $derived(shown.find((p) => p.id === route.id) ?? shown[0]);
  const scope = $derived(GROUPS.find((g) => g.id === active.group)!);
</script>

<PageHeader title="Settings" />

<div class="settings" class:settings--list={!route.id}>
  <nav class="settings__nav" aria-label="Settings">
    {#each GROUPS as g (g.id)}
      {@const pages = shown.filter((p) => p.group === g.id)}
      {#if pages.length}
        <h2 class="sun-label settings__group">{g.label}</h2>
        {#each [...new Set(pages.map((p) => p.subgroup))] as sub (sub)}
          {#if sub}<h3 class="ml-eyebrow settings__sub">{sub}</h3>{/if}
          <ul class="sun-nav settings__list">
            {#each pages.filter((p) => p.subgroup === sub) as p (p.id)}
              <li>
                <a href={href('settings', p.id)} aria-current={active.id === p.id ? 'page' : undefined}>
                  <Icon name={p.icon} size={18} />
                  <span class="sun-nav__label">{p.label}</span>
                  <Icon name="chevron-right" size={18} class="settings__chevron" />
                </a>
              </li>
            {/each}
          </ul>
        {/each}
      {/if}
    {/each}
  </nav>

  <div class="settings__main">
    <div class="settings__page">
      <a class="settings__back" href={href('settings')}><Icon name="chevron-left" size={18} />Settings</a>
      <header class="settings__head">
        <h2>{active.label}</h2>
        <Badge><Icon name={scope.scope_icon} size={12} />{scope.scope}</Badge>
      </header>

      <!-- A workspace's pages start over when the workspace changes. -->
      {#key active.group === 'workspace' ? workspace?.id : ''}
        {#each active.parts as Part, i (i)}<Part {...active.props} />{/each}
      {/key}
    </div>
  </div>
</div>

<style>
  .settings {
    flex: 1;
    min-height: 0;
    display: flex;
  }
  .settings__nav {
    flex: 1;
    overflow-y: auto;
    padding: var(--space-4);
  }
  .settings__group {
    padding: 0 var(--space-3);
    margin-top: var(--space-4);
  }
  .settings__group:first-child {
    margin-top: 0;
  }
  .settings__sub {
    margin: var(--space-3) 0 var(--space-1);
    padding: 0 var(--space-3);
  }
  .settings__list {
    margin: 0;
    padding: 0;
    list-style: none;
  }
  .settings__main {
    flex: 1;
    min-width: 0;
    overflow-y: auto;
  }
  .settings__page {
    max-width: 720px;
    box-sizing: border-box;
    padding: var(--space-4);
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
  }
  .settings__back {
    display: inline-flex;
    align-items: center;
    gap: var(--space-1);
    align-self: flex-start;
    min-height: 44px;
    font-size: 14px;
    font-weight: 600;
    color: var(--fg-2);
    text-decoration: none;
  }
  .settings__head {
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: var(--space-2) var(--space-3);
    margin-bottom: var(--space-2);
  }
  .settings__head h2 {
    margin: 0;
    font-family: var(--font-heading);
    font-weight: 400;
    font-size: 24px;
    line-height: 1.1;
    color: var(--fg-1);
  }

  /* Phones: the list or one page, never both. Rows grow to touch size. */
  @media (max-width: 767px) {
    .settings--list .settings__main,
    .settings:not(.settings--list) .settings__nav {
      display: none;
    }
    .settings__list a {
      min-height: 44px;
    }
    .settings--list .settings__list a[aria-current='page'] {
      background: none;
      color: var(--fg-2);
    }
  }
  @media (min-width: 768px) {
    .settings__nav {
      flex: none;
      width: 240px;
      box-sizing: border-box;
      padding: var(--space-4) var(--space-3);
      border-right: 1.5px solid var(--border-subtle);
    }
    .settings__back,
    .settings__nav :global(.settings__chevron) {
      display: none;
    }
  }
  @media (min-width: 1024px) {
    .settings__page {
      padding: var(--space-5);
    }
  }
</style>
