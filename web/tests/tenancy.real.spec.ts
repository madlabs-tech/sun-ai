// Two workspaces on the real kernel: switching between them never shows the other's tasks, agents or members, and a
// viewer of one finds no control that writes there and no deployment page.
import { expect, test, type APIRequestContext, type Page } from '@playwright/test';

const MAIN = '00000000000000000000000001';
const main = (page: Page) => page.getByRole('main');
const sidebar = (page: Page) => page.getByRole('complementary', { name: 'Main' });
const switcher = (page: Page) => sidebar(page).getByRole('button', { name: /^Workspace: / });
const loaded = async (page: Page) => expect(page.getByText('Loading…')).toHaveCount(0);

/** Signs `owner` in as the deployment owner. */
async function as_owner(owner: APIRequestContext) {
  expect((await owner.post('/api/v1/session', { data: { email: 'me@example.com', password: 'test-token' } })).status()).toBe(204);
  return owner;
}

async function ok(response: Promise<import('@playwright/test').APIResponse>) {
  const r = await response;
  expect(r.ok(), `${r.url()}: ${r.status()} ${await r.text()}`).toBeTruthy();
  return r.json().catch(() => null);
}

async function sign_in(page: Page, email: string, password: string) {
  await page.setViewportSize({ width: 1280, height: 860 });
  await page.goto('/');
  await page.getByLabel('Email').fill(email);
  await page.getByLabel('Password', { exact: true }).fill(password);
  await page.getByRole('button', { name: 'Sign in' }).click();
  await expect(page.getByRole('heading', { name: 'Sign in to this kernel' })).toBeHidden();
  const setup = page.getByRole('dialog').filter({ has: page.getByRole('heading', { name: 'Set up Sun' }) });
  await setup.getByRole('button', { name: 'Skip for now' }).click({ timeout: 3_000 }).catch(() => {});
}

/** A workspace with a task, an agent and a viewer account of its own. */
async function seeded(owner: APIRequestContext, stamp: number) {
  const name = `Tenant ${stamp % 100000}`;
  const ws = await ok(owner.post('/api/v1/workspaces', { data: { name } }));
  await ok(owner.post('/api/v1/tasks', { data: { title: `Only in ${name}`, workspace_id: ws.id } }));
  await ok(owner.post(`/api/v1/agents?workspace=${ws.id}`, { data: { name: `Agent ${stamp % 100000}`, runtime_chain: ['codex'] } }));
  const email = `vee${stamp}@example.com`;
  const password = 'vee’s long password';
  await ok(owner.post('/api/v1/members', {
    data: { email, username: `vee${stamp}`, workspace_id: ws.id, role: 'viewer', password, must_change: false },
  }));
  return { ...ws, name, email, password, viewer: `vee${stamp}`, agent: `Agent ${stamp % 100000}` };
}

test('switching workspaces never shows the other one’s tasks, agents or members', async ({ page, playwright }, info) => {
  const origin = new URL(info.project.use.baseURL!).origin;
  const stamp = Date.now();
  const owner = await as_owner(await playwright.request.newContext({ baseURL: origin }));
  const ws = await seeded(owner, stamp);
  const main_title = `Only in Main ${stamp}`;
  await ok(owner.post('/api/v1/tasks', { data: { title: main_title, workspace_id: MAIN } }));
  const main_agent = `Main agent ${stamp % 100000}`;
  await ok(owner.post('/api/v1/agents', { data: { name: main_agent, runtime_chain: ['codex'] } }));
  await owner.dispose();

  await sign_in(page, 'me@example.com', 'test-token');
  await page.goto('/#/tasks?workspace=main');
  await expect(main(page).getByText(main_title)).toBeVisible();
  await switcher(page).click();
  await page.getByRole('menu', { name: 'Account and workspaces' }).getByRole('menuitemradio', { name: ws.name }).click();
  await expect(switcher(page)).toHaveAccessibleName(`Workspace: ${ws.name}`);
  await expect(page).toHaveURL(new RegExp(`workspace=${ws.slug}`));
  await expect(main(page).getByText(`Only in ${ws.name}`)).toBeVisible();
  await expect(main(page).getByText(main_title)).toHaveCount(0);

  await page.goto(`/#/agents?workspace=${ws.slug}`);
  await expect(main(page).getByRole('article', { name: ws.agent })).toBeVisible();
  await expect(main(page).getByRole('article', { name: main_agent })).toHaveCount(0);
  await page.goto(`/#/settings/members?workspace=${ws.slug}`);
  const people = page.getByRole('region', { name: `People in ${ws.name}` });
  await expect(people.getByRole('article', { name: ws.viewer })).toBeVisible();
  await expect(people.getByRole('article')).toHaveCount(2);

  // And back to Main: nothing of the other workspace's.
  await page.goto('/#/agents?workspace=main');
  await expect(main(page).getByRole('article', { name: main_agent })).toBeVisible();
  await expect(main(page).getByRole('article', { name: ws.agent })).toHaveCount(0);
  await page.goto('/#/tasks?workspace=main');
  await expect(main(page).getByText(main_title)).toBeVisible();
  await expect(main(page).getByText(`Only in ${ws.name}`)).toHaveCount(0);
});

test('a viewer of a workspace sees it read-only and no deployment page', async ({ page, playwright }, info) => {
  const origin = new URL(info.project.use.baseURL!).origin;
  const owner = await as_owner(await playwright.request.newContext({ baseURL: origin }));
  const ws = await seeded(owner, Date.now());
  await owner.dispose();

  await sign_in(page, ws.email, ws.password);
  await expect(switcher(page)).toHaveAccessibleName(`Workspace: ${ws.name}`);
  for (const [route, heading, button] of [
    ['agents', 'Agents', 'New agent'],
    ['runtimes', 'Runtimes', 'Add runtime'],
    ['workflows', 'Workflows', 'New workflow'],
  ]) {
    await page.goto(`/#/${route}?workspace=${ws.slug}`);
    await expect(page.getByRole('heading', { name: heading, level: 1 })).toBeVisible();
    await loaded(page);
    await expect(main(page).getByRole('alert'), route).toHaveCount(0);
    await expect(main(page).getByRole('button', { name: button }), route).toHaveCount(0);
  }
  await page.goto(`/#/settings/members?workspace=${ws.slug}`);
  await expect(page.getByRole('region', { name: `People in ${ws.name}` }).getByRole('article')).toHaveCount(2);
  await expect(page.getByRole('combobox')).toHaveCount(0);
  await expect(page.getByRole('region', { name: 'Invite people' })).toHaveCount(0);
  const nav = page.getByRole('navigation', { name: 'Settings' });
  await expect(nav.getByRole('heading', { level: 2 })).toHaveText(['My Account', ws.name]);
  for (const name of ['Secrets', 'Code', 'Users', 'Audit log']) await expect(nav.getByRole('link', { name, exact: true }), name).toHaveCount(0);
  // Main is not hers: its link lands on her own workspace.
  await page.goto('/#/tasks?workspace=main');
  await expect(switcher(page)).toHaveAccessibleName(`Workspace: ${ws.name}`);
  await expect(main(page).getByText(`Only in ${ws.name}`)).toBeVisible();
});
