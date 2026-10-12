// Workspace invitations on the real kernel (no mail server): the owner invites from a workspace's Members page and
// gets the link to pass on; the invitee opens it signed out, signs in below it and joins.
import { expect, test } from '@playwright/test';

const MAIN = '00000000000000000000000001';

test('an invitation link from Members: the invitee signs in on its page and joins the workspace', async ({ page, browser, playwright }, info) => {
  const origin = new URL(info.project.use.baseURL!).origin;
  const stamp = Date.now();
  const email = `ivy${stamp}@example.com`;
  const password = 'ivy’s long password';
  const name = `Studio ${stamp % 100000}`;

  // The owner makes a workspace, and an account for Ivy in Main only (a password counts as a verified email).
  const owner = await playwright.request.newContext({ baseURL: origin });
  expect((await owner.post('/api/v1/session', { data: { email: 'me@example.com', password: 'test-token' } })).status()).toBe(204);
  const created = await owner.post('/api/v1/workspaces', { data: { name } });
  expect(created.status()).toBe(201);
  const { slug } = await created.json();
  const account = await owner.post('/api/v1/members', {
    data: { email, username: `ivy${stamp}`, workspace_id: MAIN, role: 'viewer', password, must_change: false },
  });
  expect(account.status()).toBe(201);
  await owner.dispose();

  // In the UI the owner invites her from the workspace's Members page; without mail the link comes back.
  await page.setViewportSize({ width: 1280, height: 800 });
  await page.goto('/');
  await page.getByLabel('Email').fill('me@example.com');
  await page.getByLabel('Password', { exact: true }).fill('test-token');
  await page.getByRole('button', { name: 'Sign in' }).click();
  await expect(page.getByRole('heading', { name: 'Sign in to this kernel' })).toBeHidden();
  await page.goto(`/#/settings/members?workspace=${slug}`);
  await expect(page.getByRole('region', { name: `People in ${name}` }).getByRole('article')).toHaveCount(1);
  const invite = page.getByRole('region', { name: 'Invite people' });
  await invite.getByLabel('Email').fill(email);
  await invite.getByLabel('Role').selectOption('member');
  await invite.getByRole('button', { name: 'Invite' }).click();
  const dialog = page.getByRole('dialog').filter({ hasText: 'Send the link yourself' });
  const link = await dialog.getByLabel('Invitation link').inputValue();
  expect(link).toMatch(/\/#\/invite\/[A-Za-z0-9_-]{43}$/);
  await dialog.getByRole('button', { name: 'Done' }).click();
  await expect(page.getByRole('region', { name: 'Pending invitations' }).getByRole('article', { name: email })).toContainText('Member');

  // Ivy, in a browser of her own: the link's page reads it without a session, then she signs in below it.
  const ivy = await (await browser.newContext()).newPage();
  await ivy.setViewportSize({ width: 360, height: 740 });
  await ivy.goto(link);
  await expect(ivy.getByRole('heading', { name: `Join ${name}` })).toBeVisible();
  await expect(ivy.getByText(`invited ${email} to join as Member`)).toBeVisible();
  await expect(ivy.getByLabel('Email')).toHaveValue(email);
  await ivy.getByLabel('Password', { exact: true }).fill(password);
  await ivy.getByRole('button', { name: 'Sign in', exact: true }).click();
  await ivy.getByRole('button', { name: `Join ${name}` }).click();
  await expect(ivy).toHaveURL(new RegExp(`#/tasks\\?workspace=${slug}$`));
  expect(await ivy.evaluate(() => document.documentElement.scrollWidth - window.innerWidth)).toBeLessThanOrEqual(0);

  // As a member she reads the workspace's agents, runtimes and memory (the reads name the workspace).
  for (const page_id of ['agents', 'runtimes', 'memory']) {
    await ivy.evaluate(([id, ws]) => (location.hash = `#/${id}?workspace=${ws}`), [page_id, slug]);
    await expect(ivy.getByRole('heading', { level: 1 })).toHaveText(page_id[0].toUpperCase() + page_id.slice(1));
    await expect(ivy.getByText('Loading…')).toHaveCount(0);
    await expect(ivy.getByRole('main').getByRole('alert')).toHaveCount(0);
  }

  // The link worked once.
  await ivy.goto(link);
  await expect(ivy.getByRole('heading', { name: 'This invitation doesn’t work' })).toBeVisible();
  await ivy.context().close();
});
