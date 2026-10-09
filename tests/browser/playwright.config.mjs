import { defineConfig } from '@playwright/test';
import { mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';

// Firefox 157 also resolves its shared app-data directory outside -profile.
// Isolate that directory on macOS instead of accessing the user's Firefox data.
// https://github.com/microsoft/playwright/issues/42768
const firefoxHome = process.platform === 'darwin'
  ? process.env.DEEP_OBSIDIAN_BROWSER_FIREFOX_HOME
    || mkdtempSync(path.join(tmpdir(), 'deep-obsidian-firefox-')) : undefined;
if (firefoxHome) process.env.DEEP_OBSIDIAN_BROWSER_FIREFOX_HOME = firefoxHome;

export default defineConfig({
  testDir: '.',
  testMatch: '*.spec.mjs',
  timeout: 30_000,
  workers: 1,
  retries: 0,
  globalTeardown: './cleanup.mjs',
  outputDir: '../../output/playwright/test-results',
  reporter: 'list',
  // Local TLS certificates are generated per fixture, never installed in trust stores.
  use: { ignoreHTTPSErrors: true, trace: 'retain-on-failure',
    javaScriptEnabled: process.env.DEEP_OBSIDIAN_BROWSER_NO_JS !== '1' },
  projects: ['chromium', 'firefox', 'webkit'].map(browserName => ({
    name: browserName, use: { browserName,
      ...(browserName === 'firefox' && firefoxHome ? { launchOptions: {
        env: { ...process.env, CFFIXED_USER_HOME: firefoxHome },
      } } : {}),
    },
  })),
});
