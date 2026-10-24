// The editor's terminal panel and quick open on the real kernel: `ui_server`'s `editor-box` is a real sun-host, so
// a key goes through xterm, the kernel and the host to a shell on a PTY, and quick open reads the host's git index.
// A socket drop reattaches to the same shell with the screen kept; a revoked `terminal` grant ends in its own state.
import { spawnSync } from 'node:child_process';
import { expect, test, type APIRequestContext, type Page, type WebSocketRoute } from '@playwright/test';

const MAIN = '00000000000000000000000001';
const OWNER = { email: 'me@example.com', password: 'test-token' };

const panel = (page: Page) => page.getByRole('region', { name: 'Terminal' });
const rows = (page: Page) => panel(page).locator('.xterm-rows > div');
const row = (page: Page, text: RegExp) => rows(page).filter({ hasText: text });

test.use({ viewport: { width: 1280, height: 800 } });

async function fixture(request: APIRequestContext) {
  const repos = await (await request.get(`/api/v1/workspaces/${MAIN}/repos`)).json();
  const hosts = await (await request.get(`/api/v1/hosts?workspace=${MAIN}`)).json();
  const repo: string = repos.find((r: { description: string }) => r.description === 'Editor fixture').id;
  const host: string = hosts.find((h: { name: string }) => h.name === 'editor-box').id;
  const at = (route: string, path = '') =>
    `/api/v1/workspaces/${MAIN}/repos/${repo}/editor/${route}?host_id=${host}&path=${encodeURIComponent(path)}`;
  return { repo, at };
}

async function sign_in(page: Page, who = OWNER) {
  expect((await page.request.post('/api/v1/session', { data: who })).status()).toBe(204);
  const f = await fixture(page.request);
  expect((await page.request.post(f.at('open'), { data: {} })).status()).toBe(200);
  return f;
}

/** The editor with its terminal panel open and its shell live. */
async function open_terminal(page: Page, repo: string) {
  await page.goto(`/#/editor/${repo}`);
  await expect(page.getByRole('navigation', { name: 'Files' }).getByRole('button', { name: 'README.md', exact: true })).toBeVisible();
  await page.getByRole('button', { name: 'Terminal', exact: true }).click();
  await expect(panel(page)).toBeVisible();
  await expect(panel(page).getByRole('status')).toHaveCount(0);
  await expect(rows(page).first()).toBeVisible();
}

/** Types `line` and Enter into the focused terminal. */
async function run(page: Page, line: string) {
  await panel(page).locator('.xterm').click();
  await page.keyboard.type(line);
  await page.keyboard.press('Enter');
}

/** The shell's pid: `echo <tag>-$$`, read off the screen (the typed line shows `$$`, the output digits). */
async function pid(page: Page, tag: string) {
  await run(page, `echo ${tag}-$$`);
  const shown = row(page, new RegExp(`^\\s*${tag}-\\d+\\s*$`));
  await expect(shown).toHaveCount(1);
  return (await shown.innerText()).trim().slice(tag.length + 1);
}

/** Proxies the terminal socket to the real kernel, so a test can drop it as a flaky network would. */
async function proxy(page: Page) {
  const sockets: { page_side: WebSocketRoute; server_side: WebSocketRoute; url: string }[] = [];
  await page.routeWebSocket(/\/terminal\?/, (ws) => {
    sockets.push({ page_side: ws, server_side: ws.connectToServer(), url: ws.url() });
  });
  return sockets;
}

test('a command runs in a real shell through xterm, in the worktree', async ({ page }) => {
  const { repo } = await sign_in(page);
  await open_terminal(page, repo);
  // The output differs from the typed line, so only the shell can have written it.
  await run(page, 'echo sun-$((40+2))-ok');
  await expect(row(page, /^\s*sun-42-ok\s*$/)).toHaveCount(1);
  await run(page, 'test -f README.md && echo in-$((1+1))-worktree');
  await expect(row(page, /^\s*in-2-worktree\s*$/)).toHaveCount(1);
  await run(page, 'exit 7');
  await expect(panel(page).getByRole('status')).toContainText('Session ended (exit 7)');
});

test('claude --version runs in the terminal', async ({ page }) => {
  const installed = spawnSync('sh', ['-c', 'command -v claude'], { stdio: 'ignore' }).status === 0;
  test.skip(!installed, 'claude is not on PATH on this machine (the CI image has no Claude Code)');
  const { repo } = await sign_in(page);
  await open_terminal(page, repo);
  await run(page, 'claude --version; echo done-$((2+3))');
  await expect(row(page, /^\s*done-5\s*$/)).toHaveCount(1);
  await expect(row(page, /\d+\.\d+\.\d+/).first()).toBeVisible();
  await run(page, 'exit');
  await expect(panel(page).getByRole('status')).toContainText('Session ended (exit 0)');
});

test('a dropped socket reattaches to the same shell and keeps the screen', async ({ page }) => {
  const sockets = await proxy(page);
  const { repo } = await sign_in(page);
  await open_terminal(page, repo);
  const before = await pid(page, 'before');
  expect(sockets[0].url).toMatch(/cols=\d+&rows=\d+/);

  // Both sides drop, as on a network blip; the kernel parks the shell and the page comes back for it.
  await sockets[0].page_side.close();
  await sockets[0].server_side.close();
  await expect(panel(page).getByRole('status')).toHaveText('Connection lost. Reconnecting…');
  await expect(panel(page).getByRole('status')).toHaveCount(0, { timeout: 10_000 });
  expect(sockets[1].url).toMatch(/pty_id=[0-9A-Z]+/);

  // The screen from before the drop is still there, and it is the same shell.
  await expect(row(page, new RegExp(`^\\s*before-${before}\\s*$`))).toHaveCount(1);
  const after = await pid(page, 'after');
  expect(after).toBe(before);
  await run(page, 'exit');
  await expect(panel(page).getByRole('status')).toContainText('Session ended (exit 0)');
});

test('a revoked terminal grant ends the terminal as revoked on its next connect', async ({ page, playwright }, info) => {
  const owner = await playwright.request.newContext({ baseURL: info.project.use.baseURL });
  expect((await owner.post('/api/v1/session', { data: OWNER })).status()).toBe(204);
  const { repo } = await fixture(owner);
  const stamp = `${Date.now()}${Math.floor(Math.random() * 1000)}`;
  const who = { email: `tess${stamp}@example.com`, password: 'tess’s long password' };
  const made = await owner.post('/api/v1/members', {
    data: { ...who, username: `tess${stamp}`, workspace_id: MAIN, role: 'member', must_change: false },
  });
  expect(made.status(), await made.text()).toBe(201);
  const member = (await made.json()).id;
  const grant = async (capability: string) => {
    const r = await owner.post(`/api/v1/workspaces/${MAIN}/editor/grants`, {
      data: { repo_id: repo, subject_type: 'member', subject_id: member, capability },
    });
    expect(r.status(), await r.text()).toBe(201);
    return (await r.json()).id as string;
  };
  await grant('editor.read');
  const terminal = await grant('terminal');

  const sockets = await proxy(page);
  await sign_in(page, who);
  await open_terminal(page, repo);
  await pid(page, 'live');

  expect((await owner.delete(`/api/v1/workspaces/${MAIN}/editor/grants/${terminal}`)).status()).toBe(204);
  await sockets[0].page_side.close();
  await sockets[0].server_side.close();
  await expect(panel(page).getByRole('status')).toContainText('Your terminal access was revoked.', { timeout: 10_000 });
  // It asked the kernel, which refused: no new socket got through.
  expect(sockets).toHaveLength(1);
  // The page knows too: no Terminal button after a reload.
  await page.reload();
  await expect(page.getByRole('navigation', { name: 'Files' }).getByRole('button', { name: 'README.md', exact: true })).toBeVisible();
  await expect(page.getByRole('button', { name: 'Terminal', exact: true })).toHaveCount(0);
  await owner.dispose();
});

test('⌘/Ctrl+P finds a file from the host’s index, git-ignored ones left out; Enter opens it', async ({ page }) => {
  const { repo, at } = await sign_in(page);
  const stamp = `${Date.now()}`;
  const put = async (path: string, text: string) => {
    const r = await page.request.put(at('file', path), { data: text, headers: { 'content-type': 'text/plain; charset=utf-8' } });
    expect(r.status(), await r.text()).toBe(200);
  };
  await put(`src/quick${stamp}.ts`, `export const quick = ${stamp};\n`);
  await put(`skipme${stamp}.log`, 'ignored\n');
  await put(`.gitignore`, `skipme*.log\n`);

  await page.goto(`/#/editor/${repo}`);
  await expect(page.getByRole('navigation', { name: 'Files' }).getByRole('button', { name: 'README.md', exact: true })).toBeVisible();
  const palette = page.getByRole('dialog').filter({ has: page.getByRole('heading', { name: 'Go to file' }) });

  await page.keyboard.press('ControlOrMeta+p');
  await page.keyboard.type(`skipme${stamp}`);
  await expect(palette.getByRole('option')).toHaveCount(0);
  await page.keyboard.press('Escape');

  await page.keyboard.press('ControlOrMeta+p');
  await page.keyboard.type(`quick${stamp}`);
  // An option shows the file's name, then its folder.
  await expect(palette.getByRole('option').first()).toContainText(`quick${stamp}.ts`);
  await expect(palette.getByRole('option').first()).toContainText('src');
  await expect(palette.getByRole('option').first()).toHaveAttribute('aria-selected', 'true');
  await page.keyboard.press('Enter');
  await expect(palette).toHaveCount(0);
  await expect(page.getByRole('tab', { name: `quick${stamp}.ts` })).toHaveAttribute('aria-selected', 'true');
  await expect(page.locator('.cm-content')).toContainText(`export const quick = ${stamp};`);
});
