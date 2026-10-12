// The task page's Pull requests section against the in-memory API: rows with their state, diff and
// verdict, linking by URL (and the 404 copy), the row menu with how the PR got here, the per-task
// merge switch, the footer that says what the next merge does, the move once every PR merged, and the
// workspace's sidebar switch.
import { expect, test, type Page } from '@playwright/test';

const open = (page: Page, key: string) => page.goto(`/?mock=1#/tasks/${key}`);
const main = (page: Page) => page.getByRole('main');
const section = (page: Page) => main(page).getByRole('region', { name: /^Pull requests/ });
const rows = (page: Page) => section(page).getByRole('list', { name: 'Linked pull requests' }).getByRole('listitem');
const row = (page: Page, n: number) => rows(page).filter({ hasText: `#${n}` });
const rules = (page: Page) => page.getByRole('region', { name: 'Pull requests & issues' });

async function link(page: Page, url: string) {
  await section(page).getByRole('button', { name: 'Link pull request' }).click();
  const dialog = page.getByRole('dialog').filter({ hasText: 'Link pull request' });
  await dialog.getByLabel('Pull request URL').fill(url);
  await dialog.getByRole('button', { name: 'Link' }).click();
  await expect(dialog).toBeHidden();
}

test.use({ viewport: { width: 1280, height: 860 } });

test('rows: state, the title without the key, repo#number across repos, diff or age, and one verdict', async ({ page }) => {
  await open(page, 'SUN-142');
  await expect(section(page).getByRole('heading', { level: 2 })).toHaveText(/Pull requests\s*2/);
  await expect(rows(page)).toHaveCount(2);
  const running = row(page, 470);
  await expect(running.getByRole('link')).toHaveAttribute('href', 'https://github.com/madlabs/api/pull/470');
  await expect(running.getByRole('link')).toHaveAttribute('title', 'madlabs/api#470 · @ada-bot');
  await expect(running).toContainText('Fix login timeout on mobile');
  await expect(running).not.toContainText('SUN-142');
  await expect(running).toContainText('api#470');
  await expect(running).toContainText('+312 −48');
  await expect(running).toContainText('4/5');
  const draft = row(page, 471);
  await expect(draft).toContainText('Retry the session refresh on timeout');
  await expect(draft).toContainText('mobile#471');
  await expect(draft).toContainText('Draft');
  await expect(section(page)).toContainText('Moves to Done when #470, #471 merge');

  await open(page, 'SUN-139');
  const failed = row(page, 466);
  await expect(failed).toContainText('1/4 failed');
  await expect(failed).toContainText('Merge conflicts');
  // One repo: the number alone.
  await expect(failed).toContainText('#466');
  await expect(failed).not.toContainText('web#466');

  await open(page, 'SUN-136');
  const merged = row(page, 477);
  await expect(merged.getByRole('link')).toHaveAttribute('href', 'https://github.com/madlabs/mobile/pull/477');
  await expect(merged).toContainText('Merged');
  await expect(merged).toContainText('ago');
  // Done: a merge has nothing left to do, so the footer says nothing.
  await expect(section(page).locator('.prs__foot')).toHaveCount(0);
});

test('link by URL: a PR Sun never received is refused in plain words; a mirrored one joins the list', async ({ page }) => {
  await open(page, 'SUN-142');
  await section(page).getByRole('button', { name: 'Link pull request' }).click();
  const dialog = page.getByRole('dialog').filter({ hasText: 'Link pull request' });
  await expect(dialog).toContainText('PRs with SUN-142 in the title or branch name link automatically.');
  await expect(dialog.getByRole('button', { name: 'Link' })).toBeDisabled();
  await dialog.getByLabel('Pull request URL').fill('https://github.com/madlabs/web/pull/999');
  await dialog.getByRole('button', { name: 'Link' }).click();
  await expect(dialog).toContainText('Sun hasn’t received this PR yet. Check that its repository is connected.');
  // A query or fragment on the URL is fine.
  await dialog.getByLabel('Pull request URL').fill('https://github.com/madlabs/web/pull/480?diff=split#files');
  await dialog.getByRole('button', { name: 'Link' }).click();
  await expect(dialog).toBeHidden();
  await expect(rows(page)).toHaveCount(3);
  await expect(row(page, 480)).toContainText('web#480');
  await expect(row(page, 480)).toContainText('Ready');
  await expect(section(page)).toContainText('Moves to Done when #470, #471, #480 merge');
  await expect(main(page).getByRole('region', { name: 'Activity' })).toContainText('You linked PR #480 on madlabs/web');

  // A fourth row folds behind "Show 1 more".
  await link(page, 'https://github.com/madlabs/web/pull/466');
  await expect(rows(page)).toHaveCount(3);
  await section(page).getByRole('button', { name: 'Show 1 more' }).click();
  await expect(rows(page)).toHaveCount(4);
  await section(page).getByRole('button', { name: 'Show less' }).click();
  await expect(rows(page)).toHaveCount(3);
});

test('the row menu says how the PR got here and removes it; a closed PR can go from the footer', async ({ page }) => {
  await open(page, 'SUN-142');
  await row(page, 471).getByRole('button', { name: 'Actions for #471' }).click();
  const menu = page.getByRole('menu', { name: 'Actions for #471' });
  await expect(menu).toContainText('Linked by SUN-142 in the title');
  await menu.getByRole('menuitem', { name: 'Remove from task' }).click();
  await expect(page.getByText('Removed #471')).toBeVisible();
  await expect(rows(page)).toHaveCount(1);
  await expect(section(page)).toContainText('Moves to Done when #470 merges');
  await expect(main(page).getByRole('region', { name: 'Activity' })).toContainText('You removed PR #471 on madlabs/mobile');
  await row(page, 470).getByRole('button', { name: 'Actions for #470' }).click();
  await expect(page.getByRole('menu', { name: 'Actions for #470' })).toContainText('Linked by the branch name');
  await page.keyboard.press('Escape');

  await open(page, 'SUN-134');
  await expect(row(page, 463)).toContainText('Closed');
  await expect(section(page)).toContainText('#463 closed without merging');
  await section(page).getByRole('button', { name: 'Remove', exact: true }).click();
  await expect(section(page)).toContainText('No linked pull requests.');
  await expect(section(page).getByRole('heading', { level: 2 })).toHaveText(/^Pull requests\s*$/);
});

test('every linked PR merged: the task moves to Done, with a pr_automation entry in the log', async ({ page }) => {
  await open(page, 'SUN-142');
  await link(page, 'https://github.com/madlabs/mobile/pull/477');
  await expect(rows(page)).toHaveCount(3);
  await row(page, 471).getByRole('button', { name: 'Actions for #471' }).click();
  await page.getByRole('menuitem', { name: 'Remove from task' }).click();
  await row(page, 470).getByRole('button', { name: 'Actions for #470' }).click();
  await page.getByRole('menuitem', { name: 'Remove from task' }).click();
  await expect(page.getByText('Every remaining PR is merged, so the task moved to Done.')).toBeVisible();
  await expect(main(page).getByRole('button', { name: 'Status: Done' })).toBeVisible();
  await expect(main(page).getByRole('region', { name: 'Activity' })).toContainText('Sun moved it from In progress to Done after #477 merged');
  await expect(section(page).locator('.prs__foot')).toHaveCount(0);
});

test('the automation menu keeps one task’s status; the workspace switch turns it off for all', async ({ page }) => {
  await open(page, 'SUN-142');
  await section(page).getByRole('button', { name: 'Pull request automation' }).click();
  const menu = page.getByRole('menu', { name: 'Pull request automation' });
  await expect(menu.getByRole('menuitem')).toHaveText([/Keep status when PRs merge/, /PR merge settings/]);
  await menu.getByRole('menuitem', { name: 'Keep status when PRs merge' }).click();
  await expect(section(page)).toContainText('PR merges won’t change this task');
  await section(page).getByRole('button', { name: 'Pull request automation' }).click();
  await expect(menu.getByRole('menuitem').first()).toHaveText(/Change status when PRs merge/);
  await page.keyboard.press('Escape');
  await section(page).getByRole('button', { name: 'Turn on' }).click();
  await expect(section(page)).toContainText('Moves to Done when #470, #471 merge');

  await section(page).getByRole('button', { name: 'Pull request automation' }).click();
  await menu.getByRole('menuitem', { name: 'PR merge settings' }).click();
  await expect(page).toHaveURL(/#\/settings\/code/);
  const move = rules(page).getByRole('switch', { name: 'Move tasks when their PRs merge' });
  await move.locator('xpath=..').click();
  await expect(move).not.toBeChecked();
  await open(page, 'SUN-142');
  await expect(section(page).locator('.prs__foot')).toHaveCount(0);
  await section(page).getByRole('button', { name: 'Pull request automation' }).click();
  await expect(menu.getByRole('menuitem')).toHaveText([/PR merge settings/]);
});

test('the sidebar switch hides the section; an agent’s task moves to Needs you', async ({ page }) => {
  await page.goto('/?mock=1#/settings/code');
  const sidebar = rules(page).getByRole('switch', { name: 'Pull Request sidebar' });
  await sidebar.locator('xpath=..').click();
  await expect(sidebar).not.toBeChecked();
  await open(page, 'SUN-142');
  await expect(main(page).getByRole('heading', { name: 'Fix login timeout on mobile' })).toBeVisible();
  await expect(section(page)).toHaveCount(0);
  await page.goto('/?mock=1#/settings/code');
  await sidebar.locator('xpath=..').click();
  await expect(sidebar).toBeChecked();

  await page.goto('/?mock=1#/tasks');
  await page.getByRole('complementary', { name: 'Main' }).getByRole('button', { name: 'New task' }).click();
  const dialog = page.getByRole('dialog');
  await dialog.getByRole('textbox', { name: 'Title' }).fill('Pricing table polish');
  await dialog.getByRole('button', { name: /^Assignee:/ }).click();
  await page.getByRole('menuitemradio', { name: /^Ada/ }).click();
  await dialog.getByRole('button', { name: 'Create task' }).click();
  await expect(page).toHaveURL(/#\/tasks\/SUN-152/);
  await expect(section(page)).toContainText('No linked pull requests.');
  await link(page, 'https://github.com/madlabs/web/pull/480');
  await expect(section(page)).toContainText('Moves to Needs you when #480 merges');
});

test('pending checks: the list re-fetches every 30 s until the host finishes them', async ({ page }) => {
  // The kernel's snapshot refresh writes no activity, so the section polls while a check is running.
  await page.clock.install();
  await open(page, 'SUN-142');
  await expect(row(page, 470)).toContainText('4/5');
  await page.clock.runFor(31_000);
  await expect(row(page, 470)).toContainText('Ready');
  await expect(row(page, 470)).not.toContainText('4/5');
});

test('360 px: the rows fit without sideways scroll', async ({ page }) => {
  await page.setViewportSize({ width: 360, height: 740 });
  await open(page, 'SUN-142');
  await expect(rows(page)).toHaveCount(2);
  await expect(row(page, 470)).toContainText('4/5');
  expect(await page.evaluate(() => document.documentElement.scrollWidth - innerWidth)).toBeLessThanOrEqual(0);
  const scroll = await main(page).locator('.task-page').evaluate((el) => el.scrollWidth - el.clientWidth);
  expect(scroll).toBeLessThanOrEqual(0);
});
