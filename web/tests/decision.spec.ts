import { expect, test, type Page } from '@playwright/test';

// An escalated decide step: approve the suggestion or answer differently, mock API.
const no_sideways_scroll = async (page: Page) =>
  expect(await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth)).toBeLessThanOrEqual(0);

for (const width of [1280, 360]) {
  test(`answering a decision differently records a correction (${width} px)`, async ({ page }) => {
    await page.setViewportSize({ width, height: 800 });
    await page.goto('/?mock=1#/tasks/SUN-138');
    const drawer = page.getByRole('main');
    await expect(drawer.getByText('Suggested: brand · 41% sure')).toBeVisible();
    await expect(drawer.getByText('Decision flag needs a person')).toBeVisible();
    await expect(drawer.getByRole('button', { name: 'Approve' })).toBeVisible();

    await drawer.getByRole('button', { name: 'Answer differently' }).click();
    const form = drawer.getByRole('group', { name: 'Which review does this post need?' });
    await expect(form.getByRole('radio')).toHaveCount(3);
    const save = drawer.getByRole('button', { name: 'Save answer' });
    await expect(save).toBeDisabled();
    await form.getByText('It makes claims about money').click();
    await expect(form.getByRole('radio', { name: /^legal/ })).toBeChecked();
    await no_sideways_scroll(page);
    await save.click();

    await expect(page.getByText('Answer saved')).toBeVisible();
    await expect(drawer.getByText('Decided flag: legal · by you')).toBeVisible();
    const corrections = drawer.getByRole('region', { name: /Corrections/ });
    await expect(corrections.locator('.correction')).toContainText('Decision corrected');
    // The next step is the plain approval.
    await expect(drawer.getByRole('button', { name: 'Answer differently' })).toHaveCount(0);
    await expect(drawer.getByRole('button', { name: 'Approve' })).toBeVisible();
    await no_sideways_scroll(page);
  });
}
