import { defineConfig, devices } from '@playwright/test';

// mock: the production build on `vite preview` with the in-memory API (?mock=1).
// real: the same build embedded in sun-api, served by its test kernel (crates/sun-api/examples/ui_server.rs).
// SUN_MOCK_PORT / SUN_REAL_PORT: run beside another worktree. Setting either also stops
// reusing a server already on the port (it may be another worktree's build).
const MOCK_PORT = process.env.SUN_MOCK_PORT ?? '4173';
const REAL_PORT = process.env.SUN_REAL_PORT ?? '7799';
const MOCK = `http://127.0.0.1:${MOCK_PORT}`;
const REAL = `http://127.0.0.1:${REAL_PORT}`;
const reuse = !process.env.SUN_MOCK_PORT && !process.env.SUN_REAL_PORT;

export default defineConfig({
  testDir: 'tests',
  reporter: 'list',
  use: { trace: 'retain-on-failure' },
  projects: [
    { name: 'mock', testIgnore: /real\.spec/, use: { ...devices['Desktop Chrome'], baseURL: MOCK } },
    { name: 'real', testMatch: /real\.spec/, use: { ...devices['Desktop Chrome'], baseURL: REAL } },
  ],
  webServer: [
    {
      command: `npm run build && npm run preview -- --host 127.0.0.1 --port ${MOCK_PORT} --strictPort`,
      url: MOCK,
      reuseExistingServer: reuse,
    },
    {
      // Debug rust-embed reads web/dist at request time, so this serves the build above.
      command: 'cargo run -q -p sun-api --example ui_server',
      cwd: '..',
      url: REAL,
      reuseExistingServer: reuse,
      timeout: 600_000,
      env: { SUN_UI_ADDR: `127.0.0.1:${REAL_PORT}` },
    },
  ],
});
