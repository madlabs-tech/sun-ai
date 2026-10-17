// BLO-167 against the in-memory API: who never sees the bypass control, a member's Agent dialog, and the
// policy control driven by keyboard alone.
import { expect, test, type Page } from '@playwright/test';

const go = (page: Page, query: string, hash: string) => page.goto(`/?mock=1${query}#/${hash}`);

test.use({ viewport: { width: 1280, height: 800 } });

for (const role of ['admin', 'member']) {
  test(`a ${role} has no Bypass permissions page, even by its URL`, async ({ page }) => {
    await go(page, `&mock_role=${role}`, 'settings');
    await expect(page.getByRole('heading').first()).toBeVisible();
    await expect(page.getByRole('link', { name: 'Bypass permissions' })).toHaveCount(0);
    await go(page, `&mock_role=${role}`, 'settings/bypass');
    await expect(page.getByRole('heading').first()).toBeVisible();
    await expect(page.getByTestId('bypass-policy')).toHaveCount(0);
  });
}

test('a member (no manage_runtimes) gets no Agent dialog to pick “Anything” in', async ({ page }) => {
  await go(page, '&mock_role=member&bypass_agent', 'agents');
  await expect(page.getByRole('article', { name: 'Night Owl', exact: true })).toBeVisible();
  await expect(page.getByRole('button', { name: 'New agent' })).toHaveCount(0);
  await expect(page.getByRole('button', { name: 'Edit Night Owl' })).toHaveCount(0);
});

test('the owner sets the policy with the keyboard alone', async ({ page }) => {
  await go(page, '', 'settings/bypass');
  const card = page.getByTestId('bypass-policy');
  const off = card.getByRole('radio', { name: /Owners only/ });
  await expect(off).toBeChecked();
  await off.focus();
  await page.keyboard.press('ArrowDown');
  await expect(card.getByRole('radio', { name: /Owners and the members/ })).toBeChecked();
  await page.keyboard.press('ArrowDown');
  const everyone = card.getByRole('radio', { name: /Every member/ });
  await expect(everyone).toBeChecked();
  await expect(everyone).toBeFocused();
  const save = card.getByRole('button', { name: 'Save' });
  await expect(save).toBeEnabled();
  await save.focus();
  await page.keyboard.press('Enter');
  await expect(page.getByText('Bypass permissions saved')).toBeVisible();
  await expect(save).toBeDisabled();
});
