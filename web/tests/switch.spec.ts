import { expect, test } from '@playwright/test';

// Switch runtime on the mock API: SUN-147's `drafts` agent step runs on Codex.
test('switch runtime moves the running agent step with a hand-off pack; phones fit', async ({ page }) => {
  await page.setViewportSize({ width: 360, height: 740 });
  await page.goto('/?mock=1#/tasks/SUN-147');
  const drawer = page.getByRole('main');
  await drawer.getByRole('button', { name: 'Switch runtime' }).click();

  const dialog = page.getByRole('dialog').filter({ has: page.getByRole('heading', { name: 'Switch runtime' }) });
  await expect(dialog.getByRole('group', { name: 'Continue drafts on' })).toBeVisible();
  // The current runtime isn't offered; each option says who pays and whether it's cooling down.
  await expect(dialog.getByRole('radio', { name: /Codex/ })).toHaveCount(0);
  await expect(dialog.getByText('Your API key · cooling down until')).toBeVisible();
  const confirm = dialog.getByRole('button', { name: 'Switch runtime' });
  await expect(confirm).toBeDisabled();
  await dialog.locator('label', { hasText: 'Claude Code' }).click(); // the DS radio hides its input behind the label
  await expect(dialog.getByRole('radio', { name: /Claude Code/ })).toBeChecked();
  await confirm.click();

  await expect(page.getByText('Switched to Claude Code with a hand-off pack')).toBeVisible();
  await expect(dialog).toBeHidden();
  await expect(drawer.getByText('Moved from Codex to Claude Code with a hand-off pack (switched by hand)')).toBeVisible();
  expect(await page.evaluate(() => document.documentElement.scrollWidth - innerWidth)).toBeLessThanOrEqual(0);
});
