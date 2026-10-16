// Chat: pick an agent, send a message, watch it reply (status line and live words), start a new chat; chats
// stay off the board; at 360 px the list and the thread take turns.
import { expect, test, type Page } from '@playwright/test';

const open = (page: Page, route = 'chat') => page.goto(`/?mock=1#/${route}`);
const main = (page: Page) => page.getByRole('main');

test('chat: an agent answers in the thread, and New chat starts over', async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 860 });
  await open(page);
  await expect(main(page).getByRole('heading', { level: 1 })).toHaveText('Chat');
  const agents = main(page).getByRole('navigation', { name: 'Agents' });
  const ada = agents.getByRole('button', { name: /^Ada/ });
  await expect(ada).toContainText('No messages yet');
  await ada.click();
  await expect(page).toHaveURL(/#\/chat\/.+/);

  const thread = main(page).getByRole('region', { name: 'Chat with Ada' });
  const box = thread.getByRole('textbox', { name: 'Message' });
  await expect(thread).toContainText('Ada answers here.');
  await box.fill('What are you working on?');
  await box.press('ControlOrMeta+Enter');
  const messages = thread.getByRole('region', { name: 'Messages' });
  await expect(messages.getByText('What are you working on?')).toBeVisible();
  await expect(thread.getByRole('status')).toContainText('Ada is replying');
  await expect(thread.getByText('Looking into it…')).toBeVisible();
  await expect(messages).toContainText('Ada replied');
  await expect(messages).toContainText('Ada here: noted');
  await expect(thread.getByRole('status')).toHaveText('');
  await expect(ada).toContainText('Ada here: noted');

  await thread.getByRole('button', { name: 'New chat' }).click();
  await expect(main(page).getByRole('region', { name: 'Messages' }).getByText('Nothing yet.')).toBeVisible();

  // A chat is no task.
  await open(page, 'tasks');
  await expect(main(page)).not.toContainText('Chat with Ada');
});

test('chat at 360 px: the list, then the thread with a way back', async ({ page }) => {
  await page.setViewportSize({ width: 360, height: 760 });
  await open(page);
  const agents = main(page).getByRole('navigation', { name: 'Agents' });
  await agents.getByRole('button', { name: /^Ada/ }).click();
  await expect(agents).toBeHidden();
  const thread = main(page).getByRole('region', { name: 'Chat with Ada' });
  await expect(thread.getByRole('textbox', { name: 'Message' })).toBeVisible();
  expect(await page.evaluate(() => document.documentElement.scrollWidth)).toBeLessThanOrEqual(360);
  await thread.getByRole('link', { name: 'Agents' }).click();
  await expect(agents).toBeVisible();
});
