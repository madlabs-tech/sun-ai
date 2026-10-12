import { expect, test, type Page } from '@playwright/test';

// At 360 px: every route fits the width with ≥ 44 px targets, the timeline stays readable,
// and a notification link (#/approve/<run>/<step>) acts in two taps.
test.use({ viewport: { width: 360, height: 740 } });

const ROUTES = ['', 'tasks/SUN-142', 'approvals', 'runtimes', 'hosts', 'workflows', 'workflows/new', 'mods', 'catalog', 'knowledge', 'knowledge/src_wiki', 'settings/members', 'settings', 'more', 'login', 'approve/01JAR014000000000000000000/approval', 'canvas/SUN-139', 'evals', 'evals/01JAEVAL000000000000000001', 'memory', 'projects', 'agents'];

/** The mock's run id for task SUN-<n> (mock.ts `fake_id`). */
const run_of = (n: number) => `01JAR${String(n).padStart(4, '0')}`.padEnd(26, '0');

const open = async (page: Page, route: string) => {
  await page.goto(`/?mock=1#/${route}`);
  await page.waitForLoadState('networkidle');
};

const overflow = (page: Page) => page.evaluate(() => document.documentElement.scrollWidth - innerWidth);

/** Visible controls smaller than 44 × 44 px. Fields count by their DS frame; inline text links are exempt (WCAG 2.5.8). */
const small_targets = (page: Page) =>
  page.evaluate(() =>
    [...document.querySelectorAll<HTMLElement>('a[href], button, input, select, textarea, [role="tab"], summary')]
      .filter((el) => el.checkVisibility() && getComputedStyle(el).display !== 'inline')
      .map((el) => [el, (el.closest('.ml-control') ?? el).getBoundingClientRect()] as const)
      .filter(([, r]) => r.width > 0 && (r.width < 44 || r.height < 43.5))
      .map(([el, r]) => `${el.tagName.toLowerCase()} "${(el.getAttribute('aria-label') ?? el.textContent ?? '').trim().slice(0, 30)}" ${Math.round(r.width)}×${Math.round(r.height)}`),
  );

for (const route of ROUTES) {
  test(`#/${route.split('/')[0] || 'tasks'}${route.includes('/') ? '/…' : ''}: fits 360 px, targets ≥ 44 px`, async ({ page }) => {
    await open(page, route);
    await expect(page.getByRole('heading').first()).toBeVisible();
    expect(await overflow(page)).toBeLessThanOrEqual(0);
    expect(await small_targets(page)).toEqual([]);
  });
}

test('timeline: output collapsed, every summary line visible, expanding keeps the width', async ({ page }) => {
  await open(page, 'tasks/SUN-142');
  const panel = page.getByRole('main');
  const events = panel.locator('.timeline > li');
  await expect(events.first()).toBeVisible();
  for (const summary of await panel.locator('.event__summary').all()) await expect(summary).not.toBeEmpty();
  await expect(panel.locator('.event__detail')).toHaveCount(0);
  const show = panel.getByRole('button', { name: /^Show / });
  while (await show.count()) await show.first().click();
  await expect(panel.locator('.event__detail').first()).toBeVisible();
  const page_box = panel.locator('.task-page');
  expect(await page_box.evaluate((el) => el.scrollWidth - el.clientWidth)).toBeLessThanOrEqual(0);
});

test('dialogs are bottom sheets on phones', async ({ page }) => {
  await open(page, 'tasks/SUN-142');
  await page.getByRole('button', { name: 'Cancel', exact: true }).click();
  const sheet = page.getByRole('dialog');
  await expect.poll(async () => { const b = (await sheet.boundingBox())!; return [b.width, b.y + b.height]; }).toEqual([360, 740]);
});

test('catalog: every tab and the install consent sheet fit 360 px', async ({ page }) => {
  await open(page, 'catalog');
  for (const tab of await page.getByRole('tab').all()) {
    await tab.click();
    expect(await overflow(page), (await tab.textContent()) ?? '').toBeLessThanOrEqual(0);
    expect(await small_targets(page)).toEqual([]);
  }
  await page.getByRole('tab', { name: /Skills/ }).click();
  await page.getByRole('button', { name: 'Install' }).first().click();
  await expect(page.getByRole('dialog')).toBeVisible();
  expect(await overflow(page)).toBeLessThanOrEqual(0);
  expect(await small_targets(page)).toEqual([]);
});

test('notification link: approve in two taps', async ({ page }) => {
  await open(page, `approve/${run_of(140)}/approval`); // tap 1: the link
  const sheet = page.getByRole('dialog');
  await expect(sheet.getByRole('heading', { name: 'Approve this step?' })).toBeVisible();
  await expect(sheet).toContainText('SUN-140');
  await expect(sheet).toContainText('PRD: referral program');
  await sheet.getByRole('button', { name: 'Approve' }).click(); // tap 2
  await expect(page.locator('.sun-toasts').getByText('SUN-140 approved')).toBeVisible();
  await expect(page.getByRole('main').getByRole('button', { name: 'Status: Done' })).toBeVisible();
  // The same link again: nothing left to decide.
  await open(page, `approve/${run_of(140)}/approval`);
  await expect(page.getByRole('heading', { name: 'Nothing to do' })).toBeVisible();
});

test('notification link: reject in two taps', async ({ page }) => {
  await open(page, `approve/${run_of(138)}/review_kind`); // its escalated decide step
  await page.getByRole('dialog').getByRole('button', { name: 'Reject' }).click();
  await expect(page.locator('.sun-toasts').getByText('Sent back to the agent')).toBeVisible();
  await expect(page.getByRole('main')).toBeVisible();
});

test('notification link: retry a stalled step in two taps', async ({ page }) => {
  await open(page, `approve/${run_of(139)}/implement`);
  const sheet = page.getByRole('dialog');
  await expect(sheet.getByRole('heading', { name: 'Restart this step?' })).toBeVisible();
  await expect(sheet).toContainText('Stalled');
  await sheet.getByRole('button', { name: 'Retry with hand-off' }).click();
  await expect(page.locator('.sun-toasts').getByText('Restarted with a hand-off pack')).toBeVisible();
});

test('notification link: reply to a running agent', async ({ page }) => {
  await open(page, `approve/${run_of(142)}/implement`);
  const sheet = page.getByRole('dialog');
  await sheet.getByLabel('Message to the agent').fill('Keep the diff small');
  await sheet.getByRole('button', { name: 'Send' }).click();
  await expect(page.getByRole('main').getByText('You: Keep the diff small')).toBeVisible();
});

// Icon-only buttons in the top-right corner and at a row's right edge.
const TIPS: [route: string, tab: string | null, label: string][] = [
  ['tasks/SUN-142', null, 'Send'],
  ['settings/secrets', 'github', 'Delete GITHUB_TOKEN'],
];

for (const [route, tab, label] of TIPS) {
  test(`icon-only button "${label}": the tooltip names it on hover and stays on screen`, async ({ page }) => {
    await open(page, route);
    if (tab) await page.getByRole('tab', { name: tab }).click();
    const button = page.getByRole('button', { name: label, exact: true }).first();
    await button.hover();
    const bubble = button.locator('xpath=..').locator('.ml-tip__bubble');
    await expect(bubble).toHaveText(label);
    await expect(bubble).toHaveCSS('opacity', '1');
    const box = (await bubble.boundingBox())!;
    expect(box.x).toBeGreaterThanOrEqual(0);
    expect(box.x + box.width).toBeLessThanOrEqual(360);
    expect(box.y).toBeGreaterThanOrEqual(0);
    expect(await button.getAttribute('title')).toBeNull();
  });

  test(`icon-only button "${label}": keyboard focus shows the tooltip on screen, blur hides it`, async ({ page }) => {
    await open(page, route);
    if (tab) await page.getByRole('tab', { name: tab }).click();
    const button = page.getByRole('button', { name: label, exact: true }).first();
    const bubble = button.locator('xpath=..').locator('.ml-tip__bubble');
    await button.focus();
    await expect(button).toBeFocused();
    await expect(bubble).toHaveText(label);
    await expect(bubble).toHaveCSS('opacity', '1');
    const box = (await bubble.boundingBox())!;
    expect(box.x).toBeGreaterThanOrEqual(0);
    expect(box.x + box.width).toBeLessThanOrEqual(360);
    expect(box.y).toBeGreaterThanOrEqual(0);
    expect(box.y + box.height).toBeLessThanOrEqual(740);
    // The bubble is decoration: screen readers get the button's own name once.
    await expect(bubble).toHaveAttribute('aria-hidden', 'true');
    await button.blur();
    await expect(bubble).toHaveCSS('opacity', '0');
  });
}

test('notification link: unknown run says so and leads to Tasks', async ({ page }) => {
  await open(page, 'approve/01JANOPE/approval');
  await expect(page.getByRole('heading', { name: 'Run not found' })).toBeVisible();
  await page.getByRole('button', { name: 'Open tasks' }).click();
  await expect(page.getByRole('dialog')).toBeHidden();
  await expect(page.getByRole('heading', { name: 'Tasks', level: 1 })).toBeVisible();
});
