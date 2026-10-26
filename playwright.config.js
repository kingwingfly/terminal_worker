import { defineConfig } from '@playwright/test';
const browserEnv = { ...process.env };
// A desktop DISPLAY can force SwiftShader to connect to an inaccessible X server.
delete browserEnv.DISPLAY;
export default defineConfig({
  testDir: './tests', testMatch: '**/*.spec.js', workers: 1, timeout: 90_000,
  use: { baseURL: process.env.TERMINAL_TEST_URL || 'http://127.0.0.1:8787', viewport: { width: 1440, height: 1000 },
    launchOptions: { env: browserEnv, args: ['--ozone-platform=headless', '--use-gl=angle', '--use-angle=swiftshader', '--enable-unsafe-swiftshader'] },
    screenshot: 'only-on-failure', trace: 'retain-on-failure' },
});
