// Runtimes screen against the in-memory API: one row per (host, CLI) with the TRD §6 copy per state,
// and "Check again" (POST /workspaces/{id}/hosts/{host}/reprobe). Permission modes are each agent's.
import { expect, test, type Page } from '@playwright/test';

const no_overflow = async (page: Page) =>
  expect(await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth)).toBeLessThanOrEqual(0);
const row = (page: Page, name: string) => page.getByRole('article', { name, exact: true });

test('runtimes: one row per host and CLI, each state with its badge, line and action', async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 800 });
  await page.goto('/?mock=1#/runtimes');
  await expect(page.getByRole('heading', { level: 1 })).toHaveText('Runtimes');

  // Key-backed entries are credentials, in their own section.
  await expect(page.getByRole('region', { name: 'Credentials' }).getByRole('article', { name: 'lan-qwen' })).toBeVisible();
  await expect(page.getByRole('article', { name: / on / }).filter({ hasText: 'lan-qwen' })).toHaveCount(0);

  // Two hosts with the same CLI are two rows.
  await expect(page.getByRole('article', { name: /^Claude Code on / })).toHaveCount(3);
  await expect(page.getByRole('article', { name: /^Codex on / })).toHaveCount(2);

  // Copy per state (TRD §6), one row each.
  const states: [string, RegExp | string, string | null][] = [
    ['Claude Code on rina-laptop', /Host offline\s*rina-laptop hasn’t been seen since \w{3} \d{1,2}, \d\d:\d\d\./, 'How to restart the host'],
    ['Claude Code on build-box-01', /Needs login\s*Claude Code is installed but not signed in on build-box-01\. Run claude auth login there\./, 'Check again'],
    ['OpenCode on build-box-01', /Limit reached\s*This account hit its usage limit\. Back at \d\d:\d\d\./, null],
    ['kiro-cli on build-box-01', /Not supported yet\s*Sun found kiro-cli 1\.4 but can’t run it yet\./, 'Request support'],
    ['Claude Code on hery-mbp', 'Busy 2/3', null],
    ['Codex on hery-mbp', /Ready\s*Untested version 0\.161\.0/, null],
  ];
  for (const [name, copy, action] of states) {
    const r = row(page, name);
    await expect(r.locator('.sun-row__body')).toContainText(copy);
    if (action) await expect(r.getByRole('button', { name: action }).or(r.getByRole('link', { name: action }))).toBeVisible();
  }
  await expect(row(page, 'Claude Code on build-box-01').locator('code')).toHaveText('claude auth login');
  await expect(row(page, 'kiro-cli on build-box-01').getByRole('link', { name: 'Request support' })).toHaveAttribute('href', /issues\/new\?title=Support%20kiro-cli%201\.4/);
  // Busy and limited rows have no action.
  await expect(row(page, 'Claude Code on hery-mbp').locator('.runtime__action')).toHaveCount(0);
  await expect(row(page, 'OpenCode on build-box-01').locator('.runtime__action')).toHaveCount(0);

  // Restart steps for an offline host.
  await row(page, 'Claude Code on rina-laptop').getByRole('button', { name: 'How to restart the host' }).click();
  const dialog = page.getByRole('dialog');
  await expect(dialog.getByRole('heading', { name: 'Restart rina-laptop' })).toBeVisible();
  await expect(dialog).toContainText('sun host start');
  await dialog.getByRole('button', { name: 'Done' }).click();
  await expect(dialog).toBeHidden();
});

test('check again: a login done since shows; none done says No change', async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 800 });
  await page.goto('/?mock=1#/runtimes');
  const codex = row(page, 'Codex on build-box-01');
  await expect(codex).toContainText('Needs login');
  await codex.getByRole('button', { name: 'Check again' }).click();
  await expect(page.getByText('Codex on build-box-01: ready')).toBeVisible();
  await expect(codex).toContainText('Ready');
  await expect(codex.getByRole('button', { name: 'Check again' })).toHaveCount(0);

  const claude = row(page, 'Claude Code on build-box-01');
  await claude.getByRole('button', { name: 'Check again' }).click();
  await expect(page.getByText('No change', { exact: true })).toBeVisible();
  await expect(claude).toContainText('Needs login');
});

test('runtimes: no permission mode per CLI, and the page fits 360 px', async ({ page }) => {
  await page.setViewportSize({ width: 360, height: 740 });
  await page.goto('/?mock=1#/runtimes');
  await expect(row(page, 'OpenCode on build-box-01')).toBeVisible();
  await expect(page.getByRole('combobox', { name: /^Permissions for / })).toHaveCount(0);
  await no_overflow(page);
});
