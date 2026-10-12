// Settings → <workspace> → Code against the in-memory API, multica-style: the git host rows (GitHub App
// install and GitLab OAuth in a new tab, disabled with the operator copy until the apps are configured),
// the connect landing, repositories added by URL or picked from a connection, and the PR rules.
import { expect, test, type Page } from '@playwright/test';

const MAIN = '00000000000000000000000001';
/** `configured`: the operator set the GitHub App and GitLab OAuth secrets (`mock_git_apps=1`). */
const open = (page: Page, route = 'settings/code', query = '&mock_git_apps=1') => page.goto(`/?mock=1${query}#/${route}`);
const hosts = (page: Page) => page.getByRole('list', { name: 'Git hosts' });
const host = (page: Page, name: string) => hosts(page).getByRole('listitem').filter({ hasText: name });
const repos = (page: Page) => page.getByRole('region', { name: 'Repositories' });
const repo_row = (page: Page, url: string) => repos(page).getByRole('listitem').filter({ hasText: url });
const rules = (page: Page) => page.getByRole('region', { name: 'Pull requests & issues' });
const settings_link = (page: Page, name: string) => page.getByRole('navigation', { name: 'Settings' }).getByRole('link', { name, exact: true });
const no_overflow = async (page: Page) =>
  expect(await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth)).toBeLessThanOrEqual(0);

test.use({ viewport: { width: 1280, height: 800 } });

/** Connects a host: its page opens in a new tab (the mock connects at once and lands back on Code). */
async function connect(page: Page, provider: 'GitHub' | 'GitLab') {
  const [tab] = await Promise.all([page.waitForEvent('popup'), host(page, provider).getByRole('button', { name: `Connect ${provider}` }).click()]);
  await expect(tab.getByText(`${provider} connected`, { exact: true })).toBeVisible();
  await expect(tab).toHaveURL(/#\/settings\/code\?workspace=main$/);
  await tab.close();
  // This tab catches up when it gets focus back.
  await page.evaluate(() => window.dispatchEvent(new Event('focus')));
  await expect(host(page, provider)).toContainText('Connected to madlabs');
}

test('not configured: both Connect buttons are disabled and the rows say what the operator must set', async ({ page }) => {
  await open(page, 'settings/code', '');
  const github = host(page, 'GitHub');
  await expect(github).toContainText('GitHub integration is not configured for this deployment.');
  await expect(github).toContainText('GITHUB_APP_SLUG');
  await expect(github).toContainText('scripts/load-git-apps.sh');
  await expect(github.getByRole('button', { name: 'Connect GitHub' })).toBeDisabled();
  const gitlab = host(page, 'GitLab');
  await expect(gitlab).toContainText('GitLab integration is not configured for this deployment.');
  await expect(gitlab).toContainText('GITLAB_OAUTH_CLIENT_ID and GITLAB_OAUTH_CLIENT_SECRET (and GITLAB_OAUTH_URL for a self-hosted instance)');
  await expect(gitlab.getByRole('button', { name: 'Connect GitLab' })).toBeDisabled();

  // The repositories are the workspace's own list; the picker needs a connection first.
  await expect(repos(page)).toContainText('Agents can use these repositories to work on code.');
  await expect(repos(page).getByRole('listitem')).toHaveCount(3);
  await repos(page).getByRole('button', { name: 'Add repository' }).click();
  const menu = page.getByRole('menu', { name: 'Repositories' });
  await expect(menu.getByRole('menuitem')).toHaveText([/Connect GitHub/, /Connect GitLab/, /Enter a URL…/]);
  await expect(menu.getByRole('menuitem', { name: /^Connect GitHub/ })).toHaveAttribute('aria-disabled', 'true');
  await expect(menu.getByRole('menuitem', { name: /^Connect GitHub/ })).toContainText('Not configured for this deployment');
});

test('GitHub: Connect opens the install page in a new tab, lands back connected, then ⋯ → Disconnect', async ({ page }) => {
  await open(page);
  const github = host(page, 'GitHub');
  await expect(github).toContainText('Not connected');
  await expect(github).not.toContainText('not configured');
  await connect(page, 'GitHub');
  await expect(github.getByText('Connected', { exact: true })).toBeVisible();
  await expect(github).toContainText('Connected to madlabs · Connected by You');

  await github.getByRole('button', { name: 'GitHub actions' }).click();
  await expect(page.getByRole('menu', { name: 'GitHub actions' }).getByRole('menuitem')).toHaveText(['Disconnect']);
  await page.getByRole('menuitem', { name: 'Disconnect' }).click();
  const dialog = page.getByRole('dialog').filter({ hasText: 'Disconnect GitHub App' });
  await expect(dialog).toContainText('Existing PR mirrors and links are kept');
  await dialog.getByRole('button', { name: 'Disconnect' }).click();
  await expect(page.getByText('GitHub disconnected')).toBeVisible();
  await expect(github).toContainText('Not connected');
});

test('connect landing: each outcome is said in plain words and taken out of the URL', async ({ page }) => {
  await page.goto(`/?mock=1#/settings/code?workspace=${MAIN}&github_error=requested`);
  await expect(page.getByText('Waiting for approval')).toBeVisible();
  await expect(page.getByText('An owner of the GitHub organization must approve the install.')).toBeVisible();
  await expect(page).toHaveURL(/#\/settings\/code\?workspace=main$/);
  await page.evaluate(() => (location.hash = '#/settings/code?workspace=main&github_error=invalid_state'));
  await expect(page.getByText('That connect link expired or was opened in another browser.')).toBeVisible();
  await expect(page).toHaveURL(/#\/settings\/code\?workspace=main$/);
  await page.evaluate(() => (location.hash = '#/settings/code?workspace=main&gitlab_error=persist_failed'));
  await expect(page.getByText('Sun couldn’t save the connection.')).toBeVisible();
  await page.evaluate(() => (location.hash = '#/settings/code?workspace=main&gitlab_error=refused'));
  await expect(page.getByText('GitLab refused the sign-in. Check that GITLAB_OAUTH_CLIENT_ID and GITLAB_OAUTH_CLIENT_SECRET match the OAuth app')).toBeVisible();
  await page.evaluate(() => (location.hash = '#/settings/code?workspace=main&gitlab_error=unreachable'));
  await expect(page.getByText('Sun couldn’t reach the git host.')).toBeVisible();
  await page.evaluate(() => (location.hash = '#/settings/code?workspace=main&github_error=something_new'));
  await expect(page.getByText('Something went wrong while connecting.')).toBeVisible();
});

test('repositories: Add repository → Connect GitHub lands in the picker; ticked repos are added together', async ({ page }) => {
  await open(page);
  await repos(page).getByRole('button', { name: 'Add repository' }).click();
  const [tab] = await Promise.all([page.waitForEvent('popup'), page.getByRole('menuitem', { name: 'Connect GitHub' }).click()]);
  // The landing (`github_connected=1&return_to=repositories`) opens the picker and leaves only the slug in the URL.
  const dialog = tab.getByRole('dialog').filter({ hasText: 'Choose GitHub repositories' });
  await expect(dialog).toContainText('Existing manual entries are unchanged.');
  await expect(dialog).toContainText('GitHub account: madlabs');
  await expect(tab).toHaveURL(/#\/settings\/code\?workspace=main$/);
  const picker = dialog.getByRole('list', { name: 'Repositories you can add' });
  await expect(picker.getByRole('listitem')).toHaveCount(4);
  // Main's three repos are already there: ticked and disabled, tagged Added.
  const web = picker.getByRole('listitem').filter({ hasText: 'madlabs/web' });
  await expect(web.getByRole('checkbox')).toBeChecked();
  await expect(web.getByRole('checkbox')).toBeDisabled();
  await expect(web).toContainText('Added');
  await expect(web).toContainText('Private');
  await expect(dialog.getByRole('status')).toHaveText('0 selected');
  await expect(dialog.getByRole('button', { name: 'Add repositories' })).toBeDisabled();

  await dialog.getByLabel('Search loaded repositories').fill('docs');
  await expect(picker.getByRole('listitem')).toHaveCount(1);
  await picker.getByRole('listitem').filter({ hasText: 'madlabs/docs' }).locator('label').click();
  await expect(dialog.getByRole('status')).toHaveText('1 selected');
  await dialog.getByRole('button', { name: 'Add repositories' }).click();
  await expect(dialog).toBeHidden();
  await expect(tab.getByText('1 repository added')).toBeVisible();
  const docs = repo_row(tab, 'https://github.com/madlabs/docs.git');
  await expect(docs).toBeVisible();
  await expect(docs).toContainText('Public docs site');
  // Reopened from the list: docs is ticked, disabled and tagged Added.
  await repos(tab).getByRole('button', { name: 'Add repository' }).click();
  await tab.getByRole('menuitem', { name: 'Choose from GitHub' }).click();
  const again = tab.getByRole('dialog').filter({ hasText: 'Choose GitHub repositories' });
  await expect(again.getByRole('list', { name: 'Repositories you can add' }).getByRole('listitem').filter({ hasText: 'madlabs/docs' })).toContainText('Added');
  await again.getByRole('button', { name: 'Cancel' }).click();
  await expect(again).toBeHidden();
  await tab.close();

  // This tab catches up with the connection when it gets focus back (the repos it added live in that tab's mock).
  await page.evaluate(() => window.dispatchEvent(new Event('focus')));
  await expect(host(page, 'GitHub')).toContainText('Connected to madlabs');
  await repos(page).getByRole('button', { name: 'Add repository' }).click();
  await expect(page.getByRole('menuitem', { name: 'Choose from GitHub' })).toBeVisible();
});

test('repositories by URL: duplicates are refused, SSH URLs are fine, rows edit and delete', async ({ page }) => {
  await open(page, 'settings/code', '');
  await repos(page).getByRole('button', { name: 'Add repository' }).click();
  await page.getByRole('menuitem', { name: 'Enter a URL…' }).click();
  let dialog = page.getByRole('dialog').filter({ hasText: 'Add repository' });
  await expect(dialog.getByLabel('Repository URL')).toHaveAttribute('placeholder', 'https://git.example.com/org/repo.git or git@git.example.com:org/repo.git');
  await expect(dialog.getByRole('button', { name: 'Save' })).toBeDisabled();
  // The same repository in its SSH form is a duplicate, said next to the field before saving.
  await dialog.getByLabel('Repository URL').fill('git@github.com:madlabs/web.git');
  await expect(dialog.getByText('This repository is already added.')).toBeVisible();
  await expect(dialog.getByRole('button', { name: 'Save' })).toBeDisabled();

  await dialog.getByLabel('Repository URL').fill('git@git.example.com:acme/site.git');
  await dialog.getByLabel('Description').fill('Marketing site, Astro');
  await dialog.getByRole('button', { name: 'Save' }).click();
  await expect(dialog).toBeHidden();
  const row = repo_row(page, 'git@git.example.com:acme/site.git');
  await expect(row).toContainText('Marketing site, Astro');
  await expect(repos(page).getByRole('listitem')).toHaveCount(4);

  // ⋯ → Edit changes the description; the URL stays.
  await row.getByRole('button', { name: /^Actions for/ }).click();
  await page.getByRole('menuitem', { name: 'Edit' }).click();
  dialog = page.getByRole('dialog').filter({ hasText: 'Edit repository' });
  await expect(dialog.getByLabel('Repository URL')).toBeDisabled();
  await dialog.getByLabel('Description').fill('Marketing site (Astro, Tailwind)');
  await dialog.getByRole('button', { name: 'Save' }).click();
  await expect(dialog).toBeHidden();
  await expect(row).toContainText('Marketing site (Astro, Tailwind)');

  // ⋯ → Delete repository, with a confirm.
  await row.getByRole('button', { name: /^Actions for/ }).click();
  await page.getByRole('menuitem', { name: 'Delete repository' }).click();
  const confirm = page.getByRole('dialog').filter({ hasText: 'Delete repository?' });
  await expect(confirm).toContainText('This repository will be removed from the workspace immediately.');
  await confirm.getByRole('button', { name: 'Delete repository' }).click();
  await expect(row).toHaveCount(0);
  await expect(repos(page).getByRole('listitem')).toHaveCount(3);
});

test('a new workspace starts with no connection and no repositories', async ({ page }) => {
  await open(page, 'tasks');
  const sidebar = page.getByRole('complementary', { name: 'Main' });
  await sidebar.getByRole('button', { name: /^Workspace: / }).click();
  await page.getByRole('menuitem', { name: 'Create workspace' }).click();
  await page.getByRole('dialog').getByLabel('Name').fill('Ops');
  await page.getByRole('dialog').getByRole('button', { name: 'Create workspace' }).click();
  await expect(page).toHaveURL(/workspace=ops$/);
  await page.goto('/?mock=1&mock_git_apps=1#/settings/code?workspace=ops');
  await expect(host(page, 'GitHub')).toContainText('Not connected');
  await expect(repos(page).locator('.sun-empty')).toHaveText('No repositories yet.');
  await expect(repos(page).getByRole('list')).toHaveCount(0);
});

test('GitLab: OAuth connect, the picker, Rotate webhook, and the row menu by keyboard', async ({ page }) => {
  await open(page);
  await connect(page, 'GitLab');
  await expect(host(page, 'GitLab')).toContainText('Connected to madlabs on gitlab.com · Connected by You');

  await repos(page).getByRole('button', { name: 'Add repository' }).click();
  await page.getByRole('menuitem', { name: 'Choose from GitLab' }).click();
  const dialog = page.getByRole('dialog').filter({ hasText: 'Choose GitLab repositories' });
  const picker = dialog.getByRole('list', { name: 'Repositories you can add' });
  await expect(picker.getByRole('listitem')).toHaveCount(2);
  await picker.getByRole('listitem').filter({ hasText: 'madlabs/platform/infra' }).locator('label').click();
  await picker.getByRole('listitem').filter({ hasText: 'madlabs/platform/app' }).locator('label').click();
  await expect(dialog.getByRole('status')).toHaveText('2 selected');
  await dialog.getByRole('button', { name: 'Add repositories' }).click();
  await expect(page.getByText('2 repositories added')).toBeVisible();
  await expect(repos(page).getByRole('listitem')).toHaveCount(5);

  const trigger = host(page, 'GitLab').getByRole('button', { name: /^GitLab .* actions$/ });
  await trigger.focus();
  await page.keyboard.press('ArrowDown');
  const menu = page.getByRole('menu', { name: /^GitLab .* actions$/ });
  await expect(menu.getByRole('menuitem', { name: 'Rotate webhook' })).toBeFocused();
  await page.keyboard.press('ArrowDown');
  await expect(menu.getByRole('menuitem', { name: 'Disconnect' })).toBeFocused();
  await page.keyboard.press('Escape');
  await expect(menu).toBeHidden();
  await expect(trigger).toBeFocused();

  await trigger.click();
  await page.getByRole('menuitem', { name: 'Rotate webhook' }).click();
  await expect(page.getByText('Webhook secret rotated')).toBeVisible();
  await expect(page.getByText('2 webhooks updated.')).toBeVisible();
});

test('pull request rules: each switch saves on the workspace and survives a reload', async ({ page }) => {
  await open(page);
  const card = rules(page);
  await expect(card.getByRole('switch')).toHaveCount(3);
  for (const name of ['Pull Request sidebar', 'Auto-link issues and PRs', 'Move tasks when their PRs merge']) await expect(card.getByRole('switch', { name })).toBeChecked();
  await expect(card).toContainText('Links a PR to the task whose ID, e.g. SUN-123, is in its title or branch name');
  await expect(card).toContainText('Workflow tasks move to Done, agent tasks to Needs you');

  const move = card.getByRole('switch', { name: 'Move tasks when their PRs merge' });
  await move.locator('xpath=..').click();
  await expect(move).not.toBeChecked();
  await settings_link(page, 'Access').click();
  await settings_link(page, 'Code').click();
  await expect(rules(page).getByRole('switch', { name: 'Move tasks when their PRs merge' })).not.toBeChecked();
  await expect(rules(page).getByRole('switch', { name: 'Pull Request sidebar' })).toBeChecked();
});

test('without edit_workspace there is no Code page', async ({ page }) => {
  await open(page, 'settings/code', '&mock_role=member');
  await expect(page.getByRole('heading', { name: 'Account', exact: true })).toBeVisible();
  await expect(settings_link(page, 'Code')).toHaveCount(0);
});

test('360 px: the Code page fits, with and without the apps configured', async ({ page }) => {
  await page.setViewportSize({ width: 360, height: 740 });
  await open(page, 'settings/code', '');
  await expect(host(page, 'GitHub')).toContainText('not configured');
  await no_overflow(page);
  await open(page);
  await connect(page, 'GitLab');
  await no_overflow(page);
  await repos(page).getByRole('button', { name: 'Add repository' }).click();
  await page.getByRole('menuitem', { name: 'Enter a URL…' }).click();
  await expect(page.getByRole('dialog').filter({ hasText: 'Add repository' })).toBeVisible();
  await no_overflow(page);
});
