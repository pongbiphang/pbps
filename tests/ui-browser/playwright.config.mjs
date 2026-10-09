import { defineConfig } from '@playwright/test';
export default defineConfig({
  testDir: '.', testMatch: '*.spec.mjs', workers: 1, retries: 0,
  forbidOnly: true, timeout: 45_000, globalTimeout: 300_000,
  expect: { timeout: 10_000 }, reporter: './reporter.mjs',
  outputDir: './results',
  use: { browserName: 'chromium', headless: true, actionTimeout: 10_000,
    navigationTimeout: 15_000, trace: 'off', screenshot: 'off', video: 'off',
    serviceWorkers: 'block' },
});
