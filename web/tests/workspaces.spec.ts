// Workspaces against the in-memory API: each workspace has its own workflows and tasks, the
// current workspace lives in the URL (by slug; id links redirect) and is remembered, and anyone creates one
// (they own it). Rename, members and delete live in Settings → <workspace>.
import { expect, test, type Locator, type Page } from '@playwright/test';

const MAIN = '00000000000000000000000001';

/** The account/workspace menu's trigger in `scope` (the sidebar, or More on a phone). */
const switcher = (scope: Locator) => scope.getByRole('button', { name: /^Workspace: / });
/** Opens the menu in `scope` and picks the item called `name` (a workspace or an action). */
async function pick(scope: Locator, name: string) {
  await switcher(scope).click();
  const menu = scope.getByRole('menu');
  await menu.getByRole('menuitemradio', { name, exact: true }).or(menu.getByRole('menuitem', { name, exact: true })).click();
}
/** Opens Settings → <workspace> → `page_name` in-app (the mock's data lives in the page, so no reload). */
async function workspace_settings(page: Page, page_name: 'General' | 'Members' | 'Code') {
  await page.getByRole('complementary', { name: 'Main' }).getByRole('link', { name: 'Settings' }).click();
  await page.getByRole('navigation', { name: 'Settings' }).getByRole('link', { name: page_name, exact: true }).click();
}
/** The menu's items in `scope`, opening it first: checks what it offers, then closes it. */
async function menu_items(scope: Locator) {
  await switcher(scope).click();
  const names = await scope.getByRole('menu').getByRole('menuitem').allTextContents();
  await scope.page().keyboard.press('Escape');
  return names.map((n) => n.trim());
}

test('workspaces: switch, create, rename, delete; workflows and tasks stay on their workspace', async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 800 });
  await page.goto('/?mock=1#/tasks');

  // The workspace's slug goes into the URL: Main to start with, holding the seed tasks.
  await expect(page).toHaveURL(/workspace=main$/);
  const sidebar = page.getByRole('complementary', { name: 'Main' });
  await expect(switcher(sidebar)).toHaveAccessibleName('Workspace: Main');
  await expect(page.locator('.task-card').first()).toBeVisible();

  // Create a workspace: names are unique ignoring case.
  await pick(sidebar, 'New workspace');
  let dialog = page.getByRole('dialog');
  await dialog.getByLabel('Name').fill('main');
  await dialog.getByRole('button', { name: 'Create workspace' }).click();
  await expect(dialog).toContainText('already exists');
  await dialog.getByLabel('Name').fill('  Marketing ');
  await dialog.getByRole('button', { name: 'Create workspace' }).click();
  await expect(dialog).toBeHidden();
  await expect(switcher(sidebar)).toHaveAccessibleName('Workspace: Marketing');
  await expect(page).toHaveURL(/workspace=marketing$/);
  const marketing = 'marketing';
  await expect(page.locator('.task-card')).toHaveCount(0);

  // Workflows: the new workspace starts empty; a template installs onto it.
  await sidebar.getByRole('link', { name: 'Workflows' }).click();
  await expect(page).toHaveURL(new RegExp(`#/workflows\\?workspace=${marketing}`));
  await expect(page.getByText('No workflows yet.')).toBeVisible();
  await page.getByRole('button', { name: /^Use / }).first().click();
  await expect(page).toHaveURL(new RegExp(`#/workflows/[^?]+\\?workspace=${marketing}`));
  await sidebar.getByRole('link', { name: 'Workflows' }).click();
  await expect(page.locator('a.sun-row')).toHaveCount(1);

  // Main still has its own workflows, not the new one.
  await pick(sidebar, 'Main');
  await expect(page).toHaveURL(/workspace=main$/);
  await expect(page.locator('a.sun-row')).not.toHaveCount(1);
  await expect(page.locator('a.sun-row').first()).toBeVisible();

  // Remembered: a link without a workspace opens the one shown last.
  await pick(sidebar, 'Marketing');
  await expect.poll(() => page.evaluate(() => localStorage.getItem('sun_workspace'))).not.toBe(MAIN);
  await page.evaluate(() => (location.hash = '#/workflows'));
  await expect(page).toHaveURL(new RegExp(`workspace=${marketing}`));
  await expect(page.locator('a.sun-row')).toHaveCount(1);

  // The new-task dialog offers only this workspace's workflows and agents: its one workflow (no agents yet), then None.
  await sidebar.getByRole('button', { name: 'New task' }).click();
  dialog = page.getByRole('dialog');
  await dialog.getByRole('button', { name: /^Assignee:/ }).click();
  await expect(page.getByRole('menu', { name: 'Assignee' }).getByRole('menuitemradio')).toHaveCount(2);
  await page.keyboard.press('Escape');
  await dialog.getByRole('button', { name: 'Close' }).click();

  // Rename, in Settings → <workspace> → General: saved when the field loses focus.
  await workspace_settings(page, 'General');
  const settings_nav = page.getByRole('navigation', { name: 'Settings' });
  await expect(settings_nav.getByRole('heading', { name: 'Marketing', exact: true })).toBeVisible();
  await page.getByLabel('Name', { exact: true }).fill('Growth');
  await page.getByLabel('Name', { exact: true }).blur();
  await expect(page.getByRole('status').filter({ hasText: 'Saved' })).toBeVisible();
  await expect(switcher(sidebar)).toHaveAccessibleName('Workspace: Growth');
  await expect(settings_nav.getByRole('heading', { name: 'Growth', exact: true })).toBeVisible();
  // The slug stays: it is part of every link.
  await expect(page).toHaveURL(/workspace=marketing$/);

  // A workspace with workflows can't be deleted (the API says why); an empty one can, then Main shows.
  const danger = page.getByRole('region', { name: 'Danger zone' });
  await danger.getByRole('button', { name: 'Delete workspace' }).click();
  await page.getByRole('dialog').getByRole('button', { name: 'Delete workspace' }).click();
  await expect(page.getByText('The workspace still has workflows')).toBeVisible();
  await pick(sidebar, 'New workspace');
  dialog = page.getByRole('dialog');
  await dialog.getByLabel('Name').fill('Scratch');
  await dialog.getByRole('button', { name: 'Create workspace' }).click();
  await expect(switcher(sidebar)).toHaveAccessibleName('Workspace: Scratch');
  await expect(page).toHaveURL(/#\/settings\/workspace\?workspace=scratch$/);
  await danger.getByRole('button', { name: 'Delete workspace' }).click();
  await page.getByRole('dialog').getByRole('button', { name: 'Delete workspace' }).click();
  await expect(switcher(sidebar)).toHaveAccessibleName('Workspace: Main');
  // Main can't be deleted, and the menu no longer offers settings or delete.
  await expect(danger.getByRole('button', { name: 'Delete workspace' })).toBeDisabled();
  await expect(danger).toContainText("Main is the default workspace and can't be deleted.");
  await switcher(sidebar).click();
  await expect(sidebar.getByRole('menuitemradio')).toHaveText([/Main/, /Growth/]);
  await expect(sidebar.getByRole('menuitem')).toHaveText(['New workspace', 'Log out']);
});

test('workspaces: the URL names the slug; an id link redirects to it', async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 800 });
  await page.goto(`/?mock=1#/workflows?workspace=${MAIN}`);
  await expect(page).toHaveURL(/#\/workflows\?workspace=main$/);
  await expect(page.locator('a.sun-row').first()).toBeVisible();
  // An unknown slug falls back to the one shown last.
  await page.evaluate(() => (location.hash = '#/workflows?workspace=nope'));
  await expect(page).toHaveURL(/#\/workflows\?workspace=main$/);
});

test('workspaces: create picks the slug from the name until edited, and previews the task prefix', async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 800 });
  await page.goto('/?mock=1#/tasks');
  const sidebar = page.getByRole('complementary', { name: 'Main' });
  await pick(sidebar, 'New workspace');
  const dialog = page.getByRole('dialog');
  const slug = dialog.getByLabel('Workspace URL');
  await dialog.getByLabel('Name').fill('Growth & Ads');
  await expect(slug).toHaveValue('growth-ads');
  await expect(dialog).toContainText('Task keys: GROW-1');
  await slug.fill('Paid--Ads');
  await dialog.getByLabel('Name').fill('Paid ads');
  await expect(slug).toHaveValue('paid--ads');
  await dialog.getByRole('button', { name: 'Create workspace' }).click();
  await expect(dialog.getByText('Use lowercase letters and digits, in words joined by single hyphens.')).toBeVisible();
  await slug.fill('main');
  await dialog.getByRole('button', { name: 'Create workspace' }).click();
  await expect(dialog.getByText('A workspace with the slug `main` already exists.')).toBeVisible();
  await slug.fill('paid');
  await expect(dialog).toContainText('Task keys: PAID-1');
  await dialog.getByRole('button', { name: 'Create workspace' }).click();
  await expect(dialog).toBeHidden();
  await expect(page).toHaveURL(/workspace=paid$/);
  await expect(switcher(sidebar)).toHaveAccessibleName('Workspace: Paid ads');
});

test('workspaces: a member creates a workspace and owns it', async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 800 });
  await page.goto('/?mock=1&mock_role=member#/tasks');
  const sidebar = page.getByRole('complementary', { name: 'Main' });
  await expect(page).toHaveURL(/workspace=main$/);
  expect(await menu_items(sidebar)).toEqual(['New workspace', 'Log out']);
  await pick(sidebar, 'New workspace');
  const dialog = page.getByRole('dialog');
  await dialog.getByLabel('Name').fill('Side project');
  await dialog.getByRole('button', { name: 'Create workspace' }).click();
  await expect(switcher(sidebar)).toHaveAccessibleName('Workspace: Side project');
  // Its owner: the workspace's admin pages show, and the members list has only them.
  await workspace_settings(page, 'Members');
  const people = page.getByRole('region', { name: 'People in Side project' });
  await expect(people.getByRole('article')).toHaveCount(1);
  await expect(people.getByRole('article', { name: 'me' })).toContainText('Owner');
  await expect(page.getByRole('navigation', { name: 'Settings' }).getByRole('link', { name: 'Code' })).toBeVisible();
});

test('workspaces: the menu is on the phone More page', async ({ page }) => {
  await page.setViewportSize({ width: 360, height: 740 });
  await page.goto('/?mock=1#/more');
  const more = page.getByRole('navigation', { name: 'More' });
  await expect(switcher(more)).toHaveAccessibleName('Workspace: Main');
  expect(await menu_items(more)).toEqual(['New workspace', 'Log out']);
});

test('workspaces: repos and knowledge are per workspace; admins import knowledge from another workspace', async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 800 });
  await page.goto('/?mock=1#/knowledge');
  const sidebar = page.getByRole('complementary', { name: 'Main' });
  const sources = page.locator('article.source');
  await expect(sources).toHaveCount(3);

  // A new workspace starts with no sources and no repos, and its search sees nothing of Main's.
  await pick(sidebar, 'New workspace');
  let dialog = page.getByRole('dialog');
  await dialog.getByLabel('Name').fill('Ops');
  await dialog.getByRole('button', { name: 'Create workspace' }).click();
  await expect(switcher(sidebar)).toHaveAccessibleName('Workspace: Ops');
  await expect(page.getByText('No sources yet.')).toBeVisible();
  await page.getByRole('textbox', { name: 'Search knowledge' }).fill('refresh token');
  await page.getByRole('button', { name: 'Search', exact: true }).click();
  await expect(page.getByText('No matches.')).toBeVisible();

  // An admin imports Main's wiki: the same source, now on Ops too.
  await page.getByRole('button', { name: 'Import', exact: true }).click();
  dialog = page.getByRole('dialog');
  await dialog.getByLabel('Source from another workspace').selectOption({ label: 'Engineering wiki' });
  await dialog.getByRole('button', { name: 'Import' }).click();
  await expect(page.getByText('Engineering wiki imported')).toBeVisible();
  await expect(sources).toHaveCount(1);
  await page.getByRole('button', { name: 'Search', exact: true }).click();
  await expect(page.getByText('The refresh token is valid for 30 days', { exact: false })).toBeVisible();

  // Repositories are the workspace's own list (Settings → Code): Ops starts with none, Main keeps its three.
  await workspace_settings(page, 'Code');
  const repos = page.getByRole('region', { name: 'Repositories' });
  await expect(repos.locator('.sun-empty')).toHaveText('No repositories yet.');
  await pick(sidebar, 'Main');
  await expect(repos.getByRole('listitem')).toHaveCount(3);

  // Main keeps its own sources.
  await page.goto('/?mock=1#/knowledge?workspace=main');
  await expect(sources).toHaveCount(3);
});

test('workspaces: an old #/board?board= link opens Tasks on that workspace', async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 800 });
  await page.goto(`/?mock=1#/board?board=${MAIN}`);
  await expect(page).toHaveURL(/\?workspace=main$/);
  await expect(page.getByRole('heading', { name: 'Tasks', level: 1 })).toBeVisible();
  await expect(page.locator('.task-card').first()).toBeVisible();
});

test('workspaces: members get no import controls', async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 800 });
  await page.goto('/?mock=1&mock_role=member#/knowledge');
  await expect(page.locator('article.source').first()).toBeVisible();
  await expect(page.getByRole('button', { name: 'Import', exact: true })).toHaveCount(0);
});
