import { expect, test } from '@playwright/test';

// Context-window meter and pinned constraints on the task page, mock API.
test('task page shows how full the context is and that Sun rotated the session', async ({ page }) => {
  await page.goto('/?mock=1#/tasks/SUN-142');
  const drawer = page.getByRole('main');
  const meter = drawer.getByRole('meter', { name: 'Context window' });
  // The latest session (after one rotation) is at 31k of 200k.
  await expect(meter).toHaveAttribute('aria-valuenow', '15');
  await expect(meter).toHaveAttribute('aria-valuetext', /a fresh session starts at 80%/);
  const row = drawer.getByTestId('context-meter');
  await expect(row).toContainText('31K / 200K · 15%');
  await expect(row.locator('.ml-badge--info')).toHaveText('Rotated once');
  await expect(row.locator('.meter__fill--ok')).toBeVisible();
  await expect(drawer.locator('.timeline')).toContainText('Context 82% full: continuing in a fresh `claude-code` session');

  // Near the threshold the bar turns amber.
  await page.goto('/?mock=1#/tasks/SUN-145');
  await expect(page.getByTestId('context-meter')).toContainText('138K / 200K · 69%');
  await expect(page.locator('.meter__fill--high')).toBeVisible();
});

test('pinned constraints are added and removed on the task', async ({ page }) => {
  const missing: string[] = [];
  page.on('response', (r) => {
    if (r.url().includes('/icons/') && !r.ok()) missing.push(r.url());
  });
  await page.goto('/?mock=1#/tasks/SUN-142');
  const pins = page.getByRole('region', { name: /Pinned constraints/ });
  await expect(pins).toContainText('Every agent session gets these');
  const add = pins.getByRole('button', { name: 'Pin', exact: true });
  await expect(add).toBeDisabled();

  await pins.getByLabel('Pin a constraint').fill('  Never change the billing schema  ');
  await add.click();
  const list = pins.getByRole('list', { name: 'Pinned constraints' });
  await expect(list.locator('.pins__text')).toHaveText(['Never change the billing schema']);
  await expect(pins.getByLabel('Pin a constraint')).toHaveValue('');
  await expect(list.locator('.ml-icon, svg, img').first()).toBeVisible();
  expect(missing).toEqual([]);

  await list.getByRole('button', { name: 'Unpin: Never change the billing schema' }).click();
  await expect(list).toHaveCount(0);
});
