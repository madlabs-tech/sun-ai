// The repo editor on the real kernel: `ui_server`'s `editor-box` is a real sun-host serving the `fixture` repo, so a
// save goes through the API and the host to disk. Edit, save and reload; create and delete; the errors a save can
// meet (409 changed or deleted meanwhile, 413, 422) as a dialog or a toast; and a viewer, who sends no writes.
import { expect, test, type APIRequestContext, type Page } from '@playwright/test';

const MAIN = '00000000000000000000000001';
const OWNER = { email: 'me@example.com', password: 'test-token' };

const tree = (page: Page) => page.getByRole('navigation', { name: 'Files' });
const entry = (page: Page, name: string) => tree(page).getByRole('button', { name, exact: true });
const code = (page: Page) => page.locator('.cm-content');
const tab = (page: Page, name: string) => page.getByRole('tab', { name });
const unsaved = (page: Page, name: string) => tab(page, name).getByRole('img', { name: 'Unsaved changes' });
const dialog = (page: Page, title: string) => page.getByRole('dialog').filter({ has: page.getByRole('heading', { name: title }) });
const toast = (page: Page) => page.locator('.sun-toasts').getByRole('status');

test.use({ viewport: { width: 1280, height: 800 } });

/** The fixture repo and the host that serves it, read as the owner. */
async function fixture(request: APIRequestContext) {
  const repos = await (await request.get(`/api/v1/workspaces/${MAIN}/repos`)).json();
  const hosts = await (await request.get(`/api/v1/hosts?workspace=${MAIN}`)).json();
  const repo: string = repos.find((r: { description: string }) => r.description === 'Editor fixture').id;
  const host: string = hosts.find((h: { name: string }) => h.name === 'editor-box').id;
  const at = (route: string, path = '') =>
    `/api/v1/workspaces/${MAIN}/repos/${repo}/editor/${route}?host_id=${host}&path=${encodeURIComponent(path)}`;
  return { repo, at };
}

/** Signs `page`'s context in (its cookie jar is the page's), opens the owner's worktree, and the fixture's handles. */
async function sign_in(page: Page, who = OWNER) {
  expect((await page.request.post('/api/v1/session', { data: who })).status()).toBe(204);
  const f = await fixture(page.request);
  if (who === OWNER) expect((await page.request.post(f.at('open'), { data: {} })).status()).toBe(200);
  return f;
}

/** Writes `path` in the signed-in caller's worktree behind the page's back (another tab, a terminal). */
async function put(request: APIRequestContext, url: string, text: string) {
  const r = await request.put(url, { data: text, headers: { 'content-type': 'text/plain; charset=utf-8' } });
  expect(r.status(), await r.text()).toBe(200);
}

async function open_editor(page: Page, repo: string) {
  await page.goto(`/#/editor/${repo}`);
  await expect(page.getByRole('heading', { name: 'Editor' })).toBeVisible();
}

/** Types `text` at the end of the open file and presses Ctrl/Cmd+S. */
async function append_and_save(page: Page, text: string) {
  await code(page).click();
  await page.keyboard.press('ControlOrMeta+End');
  await page.keyboard.type(text);
  await page.keyboard.press('ControlOrMeta+s');
}

test('the owner expands a folder, edits and saves a file, and the edit is on disk after a reload', async ({ page }) => {
  const { repo, at } = await sign_in(page);
  // A file of this run's own under src/: a parallel run saving the same file would (rightly) be a conflict.
  const stamp = Date.now();
  const name = `edit-${stamp}.ts`;
  await put(page.request, at('file', `src/${name}`), 'export const edited = false;\n');
  const line = `// saved ${stamp}`;
  await open_editor(page, repo);
  await expect(page.getByText('Read-only')).toHaveCount(0);
  await expect(entry(page, 'src')).toHaveAttribute('aria-expanded', 'false');
  await entry(page, 'src').click();
  await expect(entry(page, 'src')).toHaveAttribute('aria-expanded', 'true');
  await expect(entry(page, 'greet.ts')).toBeVisible();
  await entry(page, name).click();
  await expect(code(page)).toContainText('export const edited');

  await code(page).click();
  await page.keyboard.press('ControlOrMeta+End');
  await page.keyboard.type(line);
  await expect(unsaved(page, name)).toBeVisible();
  await page.keyboard.press('ControlOrMeta+s');
  await expect(unsaved(page, name)).toHaveCount(0);

  // On the host's disk, not only in the page.
  const file = await (await page.request.get(at('file', `src/${name}`))).json();
  expect(file.content).toBe(`export const edited = false;\n${line}`);

  await page.reload();
  await entry(page, 'src').click();
  await entry(page, name).click();
  await expect(code(page)).toContainText(line);
});

test('the owner creates a file and deletes it, on disk too', async ({ page }) => {
  const { repo, at } = await sign_in(page);
  const name = `notes-${Date.now()}.md`;
  await open_editor(page, repo);
  await tree(page).getByRole('button', { name: 'New file' }).click();
  await dialog(page, 'New file').getByLabel('Name').fill(name);
  await page.getByRole('button', { name: 'Create' }).click();
  await expect(entry(page, name)).toBeVisible();
  await expect(tab(page, name)).toHaveAttribute('aria-selected', 'true');
  expect((await page.request.get(at('file', name))).status()).toBe(200);

  await page.getByRole('button', { name: `Actions for ${name}` }).click();
  await page.getByRole('menuitem', { name: 'Delete' }).click();
  await dialog(page, 'Delete file?').getByRole('button', { name: 'Delete' }).click();
  await expect(entry(page, name)).toHaveCount(0);
  await expect(tab(page, name)).toHaveCount(0);
  expect((await page.request.get(at('file', name))).status()).toBe(404);
});

test('a file changed on disk since it was opened: Reload takes theirs, Overwrite keeps mine', async ({ page }) => {
  const { repo, at } = await sign_in(page);
  const name = `conflict-${Date.now()}.txt`;
  await put(page.request, at('file', name), 'base\n');
  await open_editor(page, repo);
  await entry(page, name).click();
  await expect(code(page)).toContainText('base');
  const conflict = dialog(page, 'The file changed on disk');

  await put(page.request, at('file', name), 'theirs\n');
  await append_and_save(page, 'mine');
  await expect(conflict).toBeVisible();
  await conflict.getByRole('button', { name: 'Reload' }).click();
  await expect(conflict).toHaveCount(0);
  await expect(code(page)).toContainText('theirs');
  await expect(code(page)).not.toContainText('mine');
  await expect(unsaved(page, name)).toHaveCount(0);

  await put(page.request, at('file', name), 'again\n');
  await append_and_save(page, 'mine');
  await conflict.getByRole('button', { name: 'Overwrite' }).click();
  await expect(conflict).toHaveCount(0);
  await expect(unsaved(page, name)).toHaveCount(0);
  const file = await (await page.request.get(at('file', name))).json();
  expect(file.content).toBe('theirs\nmine');
});

test('a file deleted since it was opened: Recreate brings it back, Close tab drops it', async ({ page }) => {
  const { repo, at } = await sign_in(page);
  const name = `vanish-${Date.now()}.txt`;
  await put(page.request, at('file', name), 'here\n');
  await open_editor(page, repo);
  await entry(page, name).click();
  await expect(code(page)).toContainText('here');

  // The 409 the save gets says `current: null`.
  expect((await page.request.delete(at('file', name))).status()).toBe(204);
  const saved = page.waitForResponse((r) => r.request().method() === 'PUT' && r.url().includes(encodeURIComponent(name)));
  await append_and_save(page, 'mine');
  const reply = await saved;
  expect(reply.status()).toBe(409);
  expect((await reply.json()).current).toBeNull();

  const deleted = dialog(page, 'The file was deleted');
  await expect(deleted).toBeVisible();
  await deleted.getByRole('button', { name: 'Recreate' }).click();
  await expect(deleted).toHaveCount(0);
  await expect(unsaved(page, name)).toHaveCount(0);
  expect((await (await page.request.get(at('file', name))).json()).content).toBe('here\nmine');

  expect((await page.request.delete(at('file', name))).status()).toBe(204);
  await append_and_save(page, '!');
  await deleted.getByRole('button', { name: 'Close tab' }).click();
  await expect(deleted).toHaveCount(0);
  await expect(tab(page, name)).toHaveCount(0);
  expect((await page.request.get(at('file', name))).status()).toBe(404);
});

test('a save over 2 MiB is a toast, not a crash, and the edit is kept', async ({ page }) => {
  const { repo, at } = await sign_in(page);
  await open_editor(page, repo);
  await entry(page, 'near_limit.txt').click();
  await expect(code(page)).toContainText('aaaa');
  const saved = page.waitForResponse((r) => r.request().method() === 'PUT');
  await append_and_save(page, 'b');
  expect((await saved).status()).toBe(413);
  await expect(toast(page).filter({ hasText: 'Couldn’t save near_limit.txt' })).toContainText('This file is over 2 MB, too large to save here.');
  await expect(unsaved(page, 'near_limit.txt')).toBeVisible();
  await expect(code(page)).toBeVisible();
  // Nothing written: the file is still 2 MiB of a's.
  const file = await (await page.request.get(at('file', 'near_limit.txt'))).json();
  expect(file.size).toBe(2 * 1024 * 1024);
});

test('422s are toasts: a symlink out of the repo, and a save onto what became a folder', async ({ page }) => {
  const { repo, at } = await sign_in(page);
  const name = `turned-${Date.now()}`;
  await put(page.request, at('file', name), 'file\n');
  await open_editor(page, repo);

  await entry(page, 'escape.txt').click();
  await expect(toast(page).filter({ hasText: 'Couldn’t open escape.txt' })).toContainText('leads outside the repo');
  await expect(tab(page, 'escape.txt')).toHaveCount(0);

  await entry(page, name).click();
  await expect(code(page)).toContainText('file');
  expect((await page.request.delete(at('file', name))).status()).toBe(204);
  expect((await page.request.post(at('file', name), { data: { kind: 'dir' } })).status()).toBe(201);
  const saved = page.waitForResponse((r) => r.request().method() === 'PUT');
  await append_and_save(page, 'x');
  expect((await saved).status()).toBe(422);
  await expect(toast(page).filter({ hasText: `Couldn’t save ${name}` })).toContainText('not a file');
  await expect(unsaved(page, name)).toBeVisible();
});

/** A new viewer in Main (made by the owner), optionally granted `editor.read` on the fixture repo. */
async function viewer(owner: APIRequestContext, repo: string, read: boolean) {
  const stamp = `${Date.now()}${Math.floor(Math.random() * 1000)}`;
  const who = { email: `vic${stamp}@example.com`, password: 'vic’s long password' };
  const made = await owner.post('/api/v1/members', {
    data: { ...who, username: `vic${stamp}`, workspace_id: MAIN, role: 'viewer', must_change: false },
  });
  expect(made.status(), await made.text()).toBe(201);
  if (read) {
    const grant = await owner.post(`/api/v1/workspaces/${MAIN}/editor/grants`, {
      data: { repo_id: repo, subject_type: 'member', subject_id: (await made.json()).id, capability: 'editor.read' },
    });
    expect(grant.status(), await grant.text()).toBe(201);
  }
  return who;
}

test('a viewer with editor.read reads only, and the page sends no write', async ({ browser, playwright }, info) => {
  const owner = await playwright.request.newContext({ baseURL: info.project.use.baseURL });
  expect((await owner.post('/api/v1/session', { data: OWNER })).status()).toBe(204);
  const { repo, at } = await fixture(owner);
  const who = await viewer(owner, repo, true);

  const page = await (await browser.newContext({ viewport: { width: 1280, height: 800 } })).newPage();
  const writes: string[] = [];
  page.on('request', (r) => {
    // `POST …/open` makes the worktree: an `editor.read` call.
    if (r.url().includes('/editor/') && r.method() !== 'GET' && !r.url().includes('/editor/open?')) writes.push(`${r.method()} ${r.url()}`);
  });
  await sign_in(page, who);
  await open_editor(page, repo);
  await expect(page.getByText('Read-only')).toBeVisible();
  await entry(page, 'src').click();
  await entry(page, 'greet.ts').click();
  await expect(code(page)).toContainText('export const greet');

  // No write action in the DOM, and none reachable by keyboard or right-click.
  await expect(page.getByRole('button', { name: 'Save' })).toHaveCount(0);
  await expect(tree(page).getByRole('button', { name: 'New file' })).toHaveCount(0);
  await expect(tree(page).getByRole('button', { name: 'New folder' })).toHaveCount(0);
  await expect(page.getByRole('button', { name: /^Actions for / })).toHaveCount(0);
  await entry(page, 'README.md').click({ button: 'right' });
  await expect(page.getByRole('menuitem')).toHaveCount(0);
  await code(page).click();
  await page.keyboard.type('nope');
  await page.keyboard.press('ControlOrMeta+s');
  await expect(code(page)).not.toContainText('nope');
  await expect(unsaved(page, 'greet.ts')).toHaveCount(0);
  expect(writes).toEqual([]);

  // The kernel agrees: a write by hand is 403, and the file is unchanged.
  const forced = await page.request.put(at('file', 'src/greet.ts'), { data: 'pwned', headers: { 'content-type': 'text/plain' } });
  expect(forced.status()).toBe(403);
  expect((await (await owner.get(at('file', 'src/greet.ts'))).json()).content).not.toContain('pwned');
  await page.context().close();
  await owner.dispose();
});

test('a viewer without a grant gets 403 and a read-only page that says why', async ({ browser, playwright }, info) => {
  const owner = await playwright.request.newContext({ baseURL: info.project.use.baseURL });
  expect((await owner.post('/api/v1/session', { data: OWNER })).status()).toBe(204);
  const { repo } = await fixture(owner);
  const who = await viewer(owner, repo, false);

  const page = await (await browser.newContext({ viewport: { width: 1280, height: 800 } })).newPage();
  const { at } = await sign_in(page, who);
  // The kernel refuses the viewer's editor calls.
  expect((await page.request.post(at('open'), { data: {} })).status()).toBe(403);
  expect((await page.request.get(at('tree'))).status()).toBe(403);
  // The page reads `editor/me` first, so it never asks the kernel for anything else of the editor.
  const editor_calls: string[] = [];
  page.on('request', (r) => {
    if (r.url().includes('/editor/') && !r.url().includes('/editor/me')) editor_calls.push(`${r.method()} ${r.url()}`);
  });
  await open_editor(page, repo);
  await expect(page.getByText('Read-only')).toBeVisible();
  await expect(page.getByRole('alert')).toHaveText('You don’t have access to this repository’s editor. Ask a workspace owner or admin to grant it.');
  await expect(tree(page)).toHaveCount(0);
  await expect(page.getByRole('button', { name: 'Save' })).toHaveCount(0);
  await expect(page.getByRole('button', { name: 'Terminal', exact: true })).toHaveCount(0);
  expect(editor_calls).toEqual([]);
  await page.context().close();
  await owner.dispose();
});
