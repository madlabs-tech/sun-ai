// The @ mention picker in Task Activity (BLO-128 part 2 test pass): the listbox and its keys, a picked name
// deleted from the draft is no longer mentioned, the hint names who wakes, and only mentioned names are marked.
import { expect, test, type Locator, type Page } from '@playwright/test';

const main = (page: Page) => page.getByRole('main');

async function pick(page: Page, pill: string, item: string) {
  await page.getByRole('dialog').getByRole('button', { name: new RegExp(`^${pill}:`) }).click();
  await page.getByRole('menuitemradio', { name: new RegExp(`^${item}`) }).click();
}

/** A new Backlog task assigned to Ada, open on its page; its comment box. */
async function adas_task(page: Page): Promise<Locator> {
  await page.goto('/?mock=1#/');
  await page.getByRole('complementary', { name: 'Main' }).getByRole('button', { name: 'New task' }).click();
  const dialog = page.getByRole('dialog');
  await dialog.getByRole('textbox', { name: 'Title' }).fill('Tidy the README');
  await pick(page, 'Assignee', 'Ada');
  await pick(page, 'Status', 'Backlog');
  await dialog.getByRole('button', { name: 'Create task' }).click();
  await expect(page).toHaveURL(/#\/tasks\/SUN-\d+/);
  return main(page).getByRole('textbox', { name: 'Comment' });
}

test.beforeEach(async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 860 });
});

test('@ opens the agent list: arrows wrap, Enter and Tab pick, Escape closes, a click picks', async ({ page }) => {
  const box = await adas_task(page);
  const list = page.getByRole('listbox', { name: 'Mention an agent' });
  const options = list.getByRole('option');

  await box.pressSequentially('@');
  await expect(options).toHaveText(['Ada', 'Night Owl']);
  await expect(box).toHaveAttribute('aria-expanded', 'true');
  await expect(options.nth(0)).toHaveAttribute('aria-selected', 'true');
  await box.press('ArrowDown');
  await expect(options.nth(1)).toHaveAttribute('aria-selected', 'true');
  await expect(options.nth(0)).toHaveAttribute('aria-selected', 'false');
  await expect(box).toHaveAttribute('aria-activedescendant', (await options.nth(1).getAttribute('id'))!);
  await box.press('ArrowDown');
  await expect(options.nth(0)).toHaveAttribute('aria-selected', 'true');
  await box.press('ArrowUp');
  await expect(options.nth(1)).toHaveAttribute('aria-selected', 'true');
  await box.press('Enter');
  await expect(list).toHaveCount(0);
  await expect(box).toHaveValue('@Night Owl ');
  await expect(box).toHaveAttribute('aria-expanded', 'false');

  // Tab picks the highlighted one; what was typed after @ narrows the list, ignoring case.
  await box.pressSequentially('and @a');
  await expect(options).toHaveText(['Ada']);
  await box.press('Tab');
  await expect(box).toHaveValue('@Night Owl and @Ada ');
  await expect(box).toBeFocused();

  // Escape closes the list and leaves the text as typed.
  await box.pressSequentially('@');
  await expect(list).toBeVisible();
  await box.press('Escape');
  await expect(list).toHaveCount(0);
  await expect(box).toHaveValue('@Night Owl and @Ada @');

  // A click picks, and the caret goes after the inserted name.
  await box.pressSequentially('n');
  await options.filter({ hasText: 'Night Owl' }).click();
  await expect(box).toHaveValue('@Night Owl and @Ada @Night Owl ');
  await box.pressSequentially('!');
  await expect(box).toHaveValue('@Night Owl and @Ada @Night Owl !');

  // No list mid-word (an email) or for a name nobody has.
  await box.fill('');
  await box.pressSequentially('mail ana@n');
  await expect(list).toHaveCount(0);
  await box.pressSequentially(' @zz');
  await expect(list).toHaveCount(0);
});

test('the hint names who wakes, and a picked name deleted from the draft is no longer mentioned', async ({ page }) => {
  const box = await adas_task(page);
  const hint = main(page).locator('#comment-hint');
  const activity = main(page).getByRole('region', { name: 'Activity' });
  await expect(hint).toHaveText('A comment wakes Ada: it reads it and picks the task up.');

  await box.pressSequentially('@ni');
  await box.press('Enter');
  await expect(hint).toHaveText('Wakes Night Owl: each answers here.');
  await box.pressSequentially('and @ad');
  await box.press('Enter');
  await expect(hint).toHaveText('Wakes Ada, Night Owl: each answers here.');

  // Night Owl's name edited away: only Ada is mentioned.
  await box.fill('@Ada and Night');
  await expect(hint).toHaveText('Wakes Ada: each answers here.');
  // Both gone: back to the assignee.
  await box.fill('never mind');
  await expect(hint).toHaveText('A comment wakes Ada: it reads it and picks the task up.');
  await box.press('ControlOrMeta+Enter');
  await expect(activity.getByText('never mind')).toBeVisible();
  await expect(activity.getByText('never mind').locator('mark')).toHaveCount(0);

  // Control: a comment that does mention Night Owl gets his reply. Had "never mind" mentioned him, its reply
  // would have come first (the mock answers each in order), so one reply proves it didn't.
  await box.pressSequentially('@ni');
  await box.press('Enter');
  await box.pressSequentially('one more?');
  await box.press('ControlOrMeta+Enter');
  await expect(activity).toContainText('Night Owl replied');
  await expect(activity.getByText('Night Owl replied')).toHaveCount(1);
  // The box is empty again and nobody is still picked.
  await expect(box).toHaveValue('');
  await box.fill('Night Owl, are you there?');
  await expect(hint).toHaveText(/^A comment wakes Ada/);
});

test('only the names picked from the list are mentioned and marked; a typed @Name is plain text', async ({ page }) => {
  const box = await adas_task(page);
  const activity = main(page).getByRole('region', { name: 'Activity' });
  await box.pressSequentially('@night');
  await box.press('Enter');
  // Typed out in full, never picked: closes on the space, not a mention.
  await box.pressSequentially('and @Ada too');
  await expect(main(page).locator('#comment-hint')).toHaveText('Wakes Night Owl: each answers here.');
  await box.press('ControlOrMeta+Enter');
  const comment = activity.getByText(/and @Ada too/);
  await expect(comment.locator('mark')).toHaveText(['@Night Owl']);
  await expect(activity).toContainText('Night Owl replied');
  await expect(activity).not.toContainText('Ada started a run');
});

test('at 360 px the agent list fits the screen', async ({ page }) => {
  const box = await adas_task(page);
  await page.setViewportSize({ width: 360, height: 760 });
  await box.pressSequentially('@');
  const list = page.getByRole('listbox', { name: 'Mention an agent' });
  await expect(list).toBeVisible();
  const at = (await list.boundingBox())!;
  expect(at.x).toBeGreaterThanOrEqual(0);
  expect(at.x + at.width).toBeLessThanOrEqual(360);
  const option = (await list.getByRole('option').first().boundingBox())!;
  expect(option.height).toBeGreaterThanOrEqual(32);
  expect(await page.evaluate(() => document.documentElement.scrollWidth)).toBeLessThanOrEqual(360);
});
