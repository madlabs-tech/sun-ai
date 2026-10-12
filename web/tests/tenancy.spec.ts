// Abuse cases of the multi-tenant UI on the in-memory API: a viewer finds no control that writes anywhere in the
// workspace; switching workspaces never shows the last one's agents, members or tasks; a brand-new person signs up
// from an invitation link while the deployment's sign-up is closed; and the Members and Secrets pages at 360 px.
import { expect, test, type Page } from '@playwright/test';

const open = (page: Page, route = '', query = '') => page.goto(`/?mock=1${query}#/${route}`);
const sidebar = (page: Page) => page.getByRole('complementary', { name: 'Main' });
const main = (page: Page) => page.getByRole('main');
const switcher = (page: Page) => sidebar(page).getByRole('button', { name: /^Workspace: / });
const no_overflow = async (page: Page) =>
  expect(await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth)).toBeLessThanOrEqual(0);
const loaded = async (page: Page) => expect(page.getByText('Loading…')).toHaveCount(0);

test.describe('a viewer', () => {
  test.use({ viewport: { width: 1280, height: 800 } });

  test('finds no write control on the AI team, settings and members pages', async ({ page }) => {
    const absent: [route: string, heading: string, names: RegExp[]][] = [
      ['workflows', 'Workflows', [/^New workflow$/]],
      ['agents', 'Agents', [/^New agent$/, /^Edit /, /^Archive /]],
      ['runtimes', 'Runtimes', [/^Add runtime$/]],
      ['memory', 'Memory', [/^Approve$/, /^Reject$/, /^Retire$/]],
    ];
    for (const [route, heading, names] of absent) {
      await open(page, route, '&mock_role=viewer');
      await expect(page.getByRole('heading', { name: heading, level: 1 })).toBeVisible();
      await loaded(page);
      for (const name of names) await expect(main(page).getByRole('button', { name }), `${route}: ${name}`).toHaveCount(0);
    }

    await open(page, 'settings/members', '&mock_role=viewer');
    await expect(page.getByRole('region', { name: 'People in Main' }).getByRole('article')).toHaveCount(4);
    await expect(page.getByRole('combobox')).toHaveCount(0);
    await expect(page.getByRole('button', { name: /^Remove / })).toHaveCount(0);
    await expect(page.getByRole('region', { name: 'Invite people' })).toHaveCount(0);
    await expect(page.getByRole('region', { name: 'Pending invitations' })).toHaveCount(0);
    // Workspace settings read only; no Code or Secrets page, no deployment pages.
    const nav = page.getByRole('navigation', { name: 'Settings' });
    for (const name of ['Code', 'Secrets', 'Users', 'Audit log', 'Email', 'Deployment secrets'])
      await expect(nav.getByRole('link', { name, exact: true }), name).toHaveCount(0);
    await open(page, 'settings/workspace', '&mock_role=viewer');
    await expect(page.getByText('You can see these settings. Changing them needs “Edit workspaces”.')).toBeVisible();
    await expect(page.getByRole('button', { name: 'Change prefix…' })).toHaveCount(0);
    await expect(page.getByRole('button', { name: /Delete workspace/ })).toHaveCount(0);
    await expect(main(page).getByRole('textbox', { name: 'Name' })).toHaveAttribute('readonly', '');
  });

  test('reads a task without editing its title, description or comments', async ({ page }) => {
    await open(page, 'tasks/SUN-147', '&mock_role=viewer');
    await expect(main(page).getByRole('heading', { name: 'October campaign: channel drafts' })).toBeVisible();
    await expect(main(page).locator('[contenteditable]')).toHaveCount(0);
    await expect(main(page).getByRole('textbox', { name: 'Description' })).toHaveCount(0);
    await expect(main(page).getByRole('textbox', { name: 'Comment' })).toHaveCount(0);
    await expect(main(page).getByRole('button', { name: 'Stop' })).toHaveCount(0);
  });

  // These would only be refused by the API (403): a viewer doesn't get them.
  test('finds no New task', async ({ page }) => {
    await open(page, 'tasks', '&mock_role=viewer');
    await expect(page.getByRole('heading', { name: 'Tasks', level: 1 })).toBeVisible();
    await expect(page.getByRole('button', { name: 'New task' })).toHaveCount(0, { timeout: 1_000 });
  });

  for (const name of ['Status', 'Priority', 'Assignee', 'Project']) {
    test(`finds no ${name} menu on a task`, async ({ page }) => {
        await open(page, 'tasks/SUN-147', '&mock_role=viewer');
      const details = page.getByRole('complementary', { name: 'Task details' });
      await expect(details).toBeVisible();
      await expect(details.getByRole('button', { name: new RegExp(`^${name}`) })).toHaveCount(0, { timeout: 1_000 });
    });
  }

  test('finds no New project', async ({ page }) => {
    await open(page, 'projects', '&mock_role=viewer');
    await expect(page.getByRole('heading', { name: 'Projects', level: 1 })).toBeVisible();
    await expect(page.getByRole('button', { name: 'New project' })).toHaveCount(0, { timeout: 1_000 });
  });

  test('finds no knowledge source write', async ({ page }) => {
    await open(page, 'knowledge', '&mock_role=viewer');
    await expect(page.getByRole('heading', { name: 'Knowledge', level: 1 })).toBeVisible();
    await loaded(page);
    await expect(main(page).getByRole('button', { name: /^(Add source|Upload|Re-index)$|^Remove |^Delete / })).toHaveCount(0, {
      timeout: 1_000,
    });
  });

  test('finds no host, catalog or MCP server write', async ({ page }) => {
    await open(page, 'hosts', '&mock_role=viewer');
    await expect(main(page).getByRole('article', { name: 'hery-mbp' })).toContainText('Serves Main');
    for (const name of [/^Add host$/, /^Detach /, /^Attach /, /^Kill session/])
      await expect(main(page).getByRole('button', { name }), String(name)).toHaveCount(0);
    await expect(page.getByRole('region', { name: 'Hosts you can attach' })).toHaveCount(0);

    await open(page, 'catalog', '&mock_role=viewer');
    for (const [tab, item] of [[/Installed/, 'tdd'], [/Suggested/, 'diagnosing-bugs'], ['MCP servers', 'github']] as const) {
      await page.getByRole('tab', { name: tab }).click();
      const article = main(page).getByRole('article').filter({ hasText: item });
      await expect(article.first(), item).toBeVisible();
      await expect(main(page).getByRole('article').getByRole('button'), item).toHaveCount(0);
    }
    await expect(main(page).getByRole('button', { name: 'Add MCP server' })).toHaveCount(0);
    await expect(page.getByLabel('Suggest for a repo')).toHaveCount(0);
  });
});

test.describe('switching workspaces', () => {
  test.use({ viewport: { width: 1280, height: 800 } });

  test('never shows the last workspace’s tasks, agents or members', async ({ page }) => {
    await open(page, 'tasks', '&invited=1');
    await expect(main(page).getByRole('link', { name: /SUN-147/ })).toBeVisible();
    await switcher(page).click();
    await page.getByRole('menu', { name: 'Account and workspaces' }).getByRole('menuitem', { name: /Invitations/ }).click();
    await page.getByRole('dialog').filter({ hasText: 'Invitations' }).getByRole('button', { name: 'Join Design' }).click();
    await expect(switcher(page)).toHaveAccessibleName('Workspace: Design');

    await page.evaluate(() => (location.hash = '#/tasks?workspace=design'));
    await expect(page.getByRole('heading', { name: 'Tasks', level: 1 })).toBeVisible();
    await loaded(page);
    await expect(main(page).getByRole('link', { name: /SUN-\d+/ })).toHaveCount(0);
    await page.evaluate(() => (location.hash = '#/agents?workspace=design'));
    await expect(page.getByRole('heading', { name: 'Agents', level: 1 })).toBeVisible();
    await loaded(page);
    for (const name of ['Ada', 'Night Owl']) await expect(main(page).getByText(name, { exact: true })).toHaveCount(0);
    await page.evaluate(() => (location.hash = '#/settings/members?workspace=design'));
    const people = page.getByRole('region', { name: 'People in Design' });
    await expect(people.getByRole('article', { name: 'ana' })).toBeVisible();
    for (const name of ['ben', 'cho']) await expect(people.getByRole('article', { name })).toHaveCount(0);
    // Hosts, catalog installs and MCP servers are Main's, not Design's.
    await page.evaluate(() => (location.hash = '#/hosts?workspace=design'));
    await expect(page.getByText('No hosts serve this workspace yet. Add one, or attach one below.')).toBeVisible();
    await page.evaluate(() => (location.hash = '#/catalog?workspace=design'));
    await expect(page.getByText('Nothing installed yet. Pick one under Suggested.')).toBeVisible();
    await page.getByRole('tab', { name: 'MCP servers' }).click();
    await loaded(page);
    await expect(main(page).getByRole('article').filter({ hasText: 'github' })).toHaveCount(0);

    // And back: Main's again, nothing of Design's left over.
    await page.evaluate(() => (location.hash = '#/settings/members?workspace=main'));
    await expect(page.getByRole('region', { name: 'People in Main' }).getByRole('article', { name: 'cho' })).toBeVisible();
    await page.evaluate(() => (location.hash = '#/hosts?workspace=main'));
    await expect(main(page).getByRole('article', { name: 'hery-mbp' })).toContainText('Serves Main');
    await page.evaluate(() => (location.hash = '#/catalog?workspace=main'));
    await page.getByRole('tab', { name: 'MCP servers' }).click();
    await expect(main(page).getByRole('article').filter({ hasText: 'github' })).toBeVisible();
    await page.evaluate(() => (location.hash = '#/tasks?workspace=main'));
    await expect(main(page).getByRole('link', { name: /SUN-147/ })).toBeVisible();
    await expect(switcher(page)).toHaveAccessibleName('Workspace: Main');
  });
});

test.describe('360 px', () => {
  test.use({ viewport: { width: 360, height: 740 } });

  test('a brand-new person signs up from an invitation link while sign-up is closed', async ({ page }) => {
    await open(page, 'invite/tok_other', '&invited=1&signed_out=1&allow_signup=0&smtp=1');
    await expect(page.getByRole('heading', { name: 'Join Main' })).toBeVisible();
    await expect(page.getByLabel('Email')).toHaveValue('eve@acme.io');
    await no_overflow(page);
    await page.getByRole('button', { name: 'Create account' }).click();
    await expect(page.getByLabel('Email')).toHaveValue('eve@acme.io');
    await page.getByRole('button', { name: 'Email me a code' }).click();
    await expect(page.getByRole('heading', { name: 'Check your email' })).toBeVisible();
    await page.getByLabel('Code').fill('123456');
    await page.getByRole('button', { name: 'Continue' }).click();
    await page.getByLabel('New password').fill('eve’s brand new password');
    await page.getByLabel('Type it again').fill('eve’s brand new password');
    await page.getByRole('button', { name: 'Save password' }).click();
    await page.getByRole('button', { name: 'Join Main' }).click();
    await expect(page).toHaveURL(/#\/tasks\?workspace=main$/);
    await expect(page.getByText('You joined Main')).toBeVisible();
    await no_overflow(page);
  });

  test('Members for an admin: role pickers, invite and pending invitations fit', async ({ page }) => {
    await open(page, 'settings/members', '&mock_role=admin&invited=1');
    const people = page.getByRole('region', { name: 'People in Main' });
    await expect(people.getByRole('combobox', { name: 'Role of cho' })).toBeVisible();
    await expect(page.getByRole('region', { name: 'Pending invitations' }).getByRole('article', { name: 'eve@acme.io' })).toBeVisible();
    await no_overflow(page);
    const invite = page.getByRole('region', { name: 'Invite people' });
    await invite.getByLabel('Email').fill('a.very.long.address.for.a.phone@subsidiary.acme-industries.io');
    await invite.getByRole('button', { name: 'Invite' }).click();
    await expect(page.getByRole('dialog').filter({ hasText: 'Send the link yourself' })).toBeVisible();
    await no_overflow(page);
  });

  test('a workspace’s Secrets: set one, it lists by name only, and the page fits', async ({ page }) => {
    await open(page, 'settings/secrets');
    const secrets = page.getByRole('main');
    const form = secrets.getByRole('form', { name: 'Add or replace a secret' });
    await expect(form).toBeVisible();
    await loaded(page);
    await no_overflow(page);
    await form.getByLabel('Name').fill('OPENROUTER_KEY');
    await form.getByLabel('Value').fill('sk-or-0123456789abcdef');
    await form.getByRole('button', { name: 'Save secret' }).click();
    await expect(secrets.getByText('OPENROUTER_KEY', { exact: true })).toBeVisible();
    await expect(page.getByText('sk-or-0123456789abcdef')).toHaveCount(0);
    await no_overflow(page);
  });
});
