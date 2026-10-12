// Against the real sun-api (embedded UI + REST + SSE), see playwright.config.ts.
import { expect, test } from '@playwright/test';

test('the owner signs in with email and the token as password, create a task, follow it live, approve it, nothing leaves the origin', async ({ page, baseURL }) => {
  const foreign: string[] = [];
  const broken: string[] = [];
  page.on('request', (r) => !r.url().startsWith(baseURL!) && !r.url().startsWith('data:') && foreign.push(r.url()));
  page.on('response', (r) => r.status() >= 400 && !r.url().includes('/api/v1/') && broken.push(`${r.status()} ${r.url()}`));

  await page.setViewportSize({ width: 1280, height: 800 });
  await page.goto('/');
  await expect(page.getByRole('heading', { name: 'Sign in to this kernel' })).toBeVisible();
  // Only email and password: the owner (seeded as admin@sun.local, email since changed) uses the kernel token.
  await expect(page.getByRole('tab')).toHaveCount(0);
  await page.getByLabel('Email').fill('me@example.com');
  await page.getByLabel('Password', { exact: true }).fill('wrong');
  await page.getByRole('button', { name: 'Sign in' }).click();
  await expect(page.getByText('That email, password or code is not right.')).toBeVisible();
  await page.getByLabel('Password', { exact: true }).fill('test-token');
  await page.getByRole('button', { name: 'Sign in' }).click();
  await expect(page.getByRole('heading', { name: 'Needs you', exact: true })).toBeVisible();
  // A kernel with no limit policy yet asks the set-up questions once; skipping stores nothing.
  const setup = page.getByRole('dialog').filter({ has: page.getByRole('heading', { name: 'Set up Sun' }) });
  await expect(setup.getByRole('radio', { name: /Pause and tell me/ })).toBeAttached();
  await setup.getByRole('button', { name: 'Skip for now' }).click();
  await expect(setup).toBeHidden();

  await page.getByRole('complementary', { name: 'Main' }).getByRole('button', { name: 'New task' }).click();
  await page.getByRole('textbox', { name: 'Title' }).fill('Check the release notes');
  await page.getByRole('dialog').getByRole('button', { name: /^Assignee:/ }).click();
  await page.getByRole('menuitemradio', { name: /review-by-a-person/ }).click();
  await page.getByRole('button', { name: 'Create task' }).click();

  // The approval-only workflow parks the run on the human step; the timeline streams over SSE.
  const panel = page.getByRole('main');
  await expect(panel.getByRole('heading', { name: 'Check the release notes' })).toBeVisible();
  await expect(panel.getByRole('button', { name: 'Approve' })).toBeVisible({ timeout: 15_000 });
  await expect(panel.getByText('Live', { exact: true })).toBeVisible();
  await expect(panel.getByRole('heading', { name: /Timeline/ })).toBeVisible();
  await panel.getByRole('button', { name: 'Approve' }).click();
  await expect(panel.getByRole('button', { name: 'Status: Done' })).toBeVisible();
  await panel.getByRole('navigation', { name: 'Breadcrumb' }).getByRole('link', { name: 'Tasks' }).click();
  await expect(page.getByRole('region', { name: 'Done' }).getByText('Check the release notes')).toBeVisible();

  // The session survives a reload (HttpOnly cookie).
  await page.reload();
  await expect(page.getByRole('heading', { name: 'Needs you', exact: true })).toBeVisible();

  expect(foreign).toEqual([]);
  expect(broken).toEqual([]);
});
