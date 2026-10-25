// Playwright configuration for the browser conformance harness.
//
// PLAN.md M4's gate is the same suite running in Chromium, Firefox **and**
// WebKit, so all three are projects here rather than an engine we add later.
// WebKit is the one that usually breaks first: its IndexedDB is the least
// forgiving and its OPFS story differs, which is exactly why it is not optional.

import { defineConfig, devices } from '@playwright/test';

const PORT = Number(process.env.DXDB_DEMO_PORT ?? 4173);
const BASE_URL = `http://127.0.0.1:${PORT}`;

export default defineConfig({
  testDir: './tests',
  // A conformance run is a sequence of storage operations; running specs in
  // parallel within one browser would have them contend for the same origin's
  // IndexedDB/OPFS storage, which is a false-failure generator.
  fullyParallel: false,
  workers: 1,
  forbidOnly: Boolean(process.env.CI),
  // The suite itself will be slow on WebKit; one retry distinguishes a flake
  // from a real failure in CI without hiding it locally.
  retries: process.env.CI ? 1 : 0,
  reporter: process.env.CI
    ? [['list'], ['html', { open: 'never' }], ['json', { outputFile: 'test-results/results.json' }]]
    : [['list'], ['html', { open: 'never' }]],

  timeout: 60_000,
  expect: { timeout: 10_000 },

  use: {
    baseURL: BASE_URL,
    trace: 'retain-on-failure',
    screenshot: 'only-on-failure',
    video: 'off',
  },

  projects: [
    {
      name: 'chromium',
      use: { ...devices['Desktop Chrome'] },
    },
    {
      name: 'firefox',
      use: { ...devices['Desktop Firefox'] },
    },
    {
      name: 'webkit',
      use: { ...devices['Desktop Safari'] },
    },
  ],

  // Playwright starts and stops this server itself, so `npm test` is one
  // command. `reuseExistingServer` keeps a hand-started `npm run serve` usable.
  webServer: {
    command: 'node scripts/serve.mjs',
    url: BASE_URL,
    reuseExistingServer: !process.env.CI,
    timeout: 30_000,
    stdout: 'pipe',
    stderr: 'pipe',
  },
});
