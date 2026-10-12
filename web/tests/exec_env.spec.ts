// New task: the exec env picker offers only kinds an online host reports ready.
import { expect, test, type Page } from '@playwright/test';

const no_overflow = async (page: Page) =>
  expect(await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth)).toBeLessThanOrEqual(0);

async function new_task(page: Page) {
  await page.goto('/?mock=1#/');
  await page.getByRole('button', { name: 'New task' }).first().click();
  const dialog = page.getByRole('dialog');
  // The exec env sits under More once the assignee runs something.
  await dialog.getByRole('button', { name: /^Assignee:/ }).click();
  await page.getByRole('menuitemradio', { name: 'Bug fix → verified PR' }).click();
  await dialog.getByText('More', { exact: true }).click();
  return dialog;
}

for (const [label, width, height] of [['desktop', 1280, 800], ['360 px', 360, 740]] as const) {
  test(`${label}: picks a ready container env and explains the missing VM`, async ({ page }) => {
    await page.setViewportSize({ width, height });
    const dialog = await new_task(page);
    const runs_in = dialog.getByRole('combobox', { name: 'Runs in' });
    for (const ready of ['Workflow default', 'On the host (no isolation)', 'Container'])
      await expect(runs_in.locator('option', { hasText: ready })).toHaveCount(1);
    await expect(runs_in.locator('option', { hasText: 'Linux VM' })).toHaveCount(0);
    await dialog.getByText('Why not linux vm?').click();
    await expect(dialog.getByText(/install Lima/)).toBeVisible();

    await dialog.getByRole('textbox', { name: 'Title' }).fill('Build in a box');
    await runs_in.selectOption('container');
    await dialog.getByLabel('Image').fill('--privileged');
    await dialog.getByRole('button', { name: 'Create task' }).click();
    await expect(dialog.getByText(/bad container image/)).toBeVisible();
    await no_overflow(page);

    await dialog.getByLabel('Image').fill('rust:1.90');
    await dialog.getByRole('button', { name: 'Create task' }).click();
    await expect(page.getByText(/SUN-\d+ created/).first()).toBeVisible();
    await expect(dialog).toBeHidden();
  });
}
