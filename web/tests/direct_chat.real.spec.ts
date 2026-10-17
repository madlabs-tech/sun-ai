// BLO-166 pass 3 over BLO-162 PR 3 on the real kernel: a chat without an agent end to end (no host is joined, so the
// picker has no tool to offer, and a message's run is placed on the kernel's host and never finishes), switching it
// through the real PATCH, the refusal while that run is in flight, and one member never reaching another's chat in the browser or through the API.
import { expect, test, type APIRequestContext, type Page } from '@playwright/test';

const MAIN = '00000000000000000000000001';
const main = (page: Page) => page.getByRole('main');
const without_agent = (page: Page) => main(page).getByRole('navigation', { name: 'Without an agent' });
/** A number no parallel test picks too (two workers can start in the same millisecond). */
const unique = () => Date.now() + Math.floor(Math.random() * 1e9);

async function signed_in(request: APIRequestContext, email: string, password: string) {
  expect((await request.post('/api/v1/session', { data: { email, password } })).status()).toBe(204);
  return request;
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

/** A chat without an agent on Main with the `fake` tool on model `model`, made by `as`. */
async function direct_chat(as: APIRequestContext, model: string) {
  const made = await as.post('/api/v1/chats', { data: { workspace_id: MAIN, runtime: { tool: 'fake', model }, permission_mode: null } });
  expect(made.status(), await made.text()).toBe(201);
  return (await made.json()) as { id: string; title: string };
}

test('real: with no host joined the picker offers no tool, and a chat made anyway switches through PATCH', async ({ page, playwright }, info) => {
  const origin = new URL(info.project.use.baseURL!).origin;
  const model = `m-${unique()}`;
  const owner = await signed_in(await playwright.request.newContext({ baseURL: origin }), 'me@example.com', 'test-token');
  const chat = await direct_chat(owner, model);

  await sign_in(page, 'me@example.com', 'test-token');
  await page.goto('/#/chat?workspace=main');
  await main(page).getByRole('button', { name: 'Chat with a tool' }).click();
  const picker = page.getByRole('dialog').filter({ has: page.getByRole('heading', { name: 'Chat with a tool' }) });
  await expect(picker).toContainText('No host has reported a tool yet.');
  await picker.getByRole('button', { name: 'Cancel' }).click();

  await without_agent(page).getByRole('button', { name: new RegExp(model) }).click();
  await expect(page).toHaveURL(new RegExp(`#/chat/${chat.id}`));
  const thread = main(page).getByRole('region', { name: `Chat with fake · ${model}` });
  await expect(thread).toContainText(`${model} · Host default mode`);

  // The real PATCH: a settings line in the thread and the new mode in the header.
  await thread.getByRole('button', { name: 'Switch model, effort or mode' }).click();
  const switcher = page.getByRole('dialog').filter({ has: page.getByRole('heading', { name: `Switch fake · ${model}` }) });
  // allow_bypass is off on this kernel.
  await expect(switcher.getByLabel('Permissions').getByRole('option', { name: 'Anything' })).toHaveCount(0);
  await switcher.getByLabel('Permissions').selectOption({ label: 'Plan only' });
  await switcher.getByRole('button', { name: 'Switch' }).click();
  await expect(switcher).toBeHidden();
  const messages = thread.getByRole('region', { name: 'Messages' });
  await expect(messages).toContainText(`switched to ${model} · Plan only. A new session reads the last 20 messages.`);
  await expect(thread).toContainText(`${model} · Plan only`);
  const saved = await (await owner.get(`/api/v1/chats?workspace=${MAIN}`)).json();
  expect(saved.find((c: { id: string }) => c.id === chat.id).permission_mode).toBe('plan');

  // A message's run is queued, then placed on `box`, which never connects: it stays in flight, and while it is the
  // switch is refused and nothing changes.
  const box = thread.getByRole('textbox', { name: 'Message' });
  await box.fill('Anyone there?');
  await box.press('ControlOrMeta+Enter');
  await expect(thread.getByRole('status')).toContainText(`fake · ${model} is replying`);
  await thread.getByRole('button', { name: 'Switch model, effort or mode' }).click();
  await switcher.getByLabel('Permissions').selectOption({ label: 'Ask first' });
  await switcher.getByRole('button', { name: 'Switch' }).click();
  await expect(switcher.getByRole('alert')).toContainText('Wait for the reply or stop it, then switch.');
  await switcher.getByRole('button', { name: 'Cancel' }).click();
  await expect(thread).toContainText(`${model} · Plan only`);
  await owner.dispose();
});

test('real: one member never reaches another member’s chat without an agent, in the browser or the API', async ({ page, playwright }, info) => {
  const origin = new URL(info.project.use.baseURL!).origin;
  const stamp = unique();
  const owner = await signed_in(await playwright.request.newContext({ baseURL: origin }), 'me@example.com', 'test-token');
  const email = `max${stamp}@example.com`;
  const password = 'max’s long password';
  const account = await owner.post('/api/v1/members', {
    data: { email, username: `max${stamp}`, workspace_id: MAIN, role: 'member', password, must_change: false },
  });
  expect(account.status(), await account.text()).toBe(201);
  const owners = await direct_chat(owner, `owner-${stamp}`);
  const member = await signed_in(await playwright.request.newContext({ baseURL: origin }), email, password);
  const members = await direct_chat(member, `member-${stamp}`);

  // In the member's browser: their own chat is listed, the owner's isn't, and its link shows nothing.
  await sign_in(page, email, password);
  await page.goto('/#/chat?workspace=main');
  await expect(without_agent(page)).toContainText(`member-${stamp}`);
  await expect(without_agent(page)).not.toContainText(`owner-${stamp}`);
  await page.goto(`/#/chat/${owners.id}?workspace=main`);
  await expect(main(page)).toContainText('This chat isn\'t here.');
  await expect(main(page).getByRole('textbox', { name: 'Message' })).toHaveCount(0);

  // Through the API, both ways (the owner is the deployment owner): read, post and switch all answer 404.
  for (const [as, other] of [[member, owners.id], [owner, members.id]] as const) {
    expect((await as.get(`/api/v1/tasks/${other}/activity`)).status()).toBe(404);
    expect((await as.post(`/api/v1/tasks/${other}/comments`, { data: { body: 'hello?' } })).status()).toBe(404);
    expect((await as.patch(`/api/v1/chats/${other}`, { data: { permission_mode: 'plan' } })).status()).toBe(404);
    const listed = await (await as.get(`/api/v1/chats?workspace=${MAIN}`)).json();
    expect(listed.map((c: { id: string }) => c.id)).not.toContain(other);
  }
  // Nothing was written to either chat.
  for (const [as, id] of [[owner, owners.id], [member, members.id]] as const) {
    const own = await as.get(`/api/v1/tasks/${id}/activity`);
    expect(own.status()).toBe(200);
    const activity = await own.json();
    expect(activity.filter((a: { kind: string }) => a.kind === 'comment' || a.kind === 'settings')).toEqual([]);
  }
  await member.dispose();
  await owner.dispose();
});
