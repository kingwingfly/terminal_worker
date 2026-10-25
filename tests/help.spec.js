import { test, expect } from '@playwright/test';
import { readFile } from 'node:fs/promises';

const source = await readFile('crates/core/src/completion.rs', 'utf8');
const commands = [...source.split('pub const COMMANDS:')[1].split('];')[0].matchAll(/"([a-z]+)"/g)].map(match => match[1]);

test('every command has side-effect-free help, including privileged and interactive commands', async ({ page }) => {
  await page.goto('/');
  const input = page.getByRole('combobox', { name: 'Terminal command' });
  const requests = [];
  page.on('request', request => { if (request.method() !== 'GET' && request.method() !== 'HEAD') requests.push(request.url()); });
  for (const command of commands) {
    await expect(page.locator('.busy')).toHaveText('↵');
    await input.fill(command + ' -h'); await input.press('Enter');
    await expect(page.locator('.transcript pre').last()).toContainText('Usage: ');
    await expect(page.locator('.transcript pre').last()).toContainText('man ' + command);
    await expect(page.locator('#auth')).not.toBeVisible();
    await expect(page.locator('#viewer')).not.toBeVisible();
  }
  for (const command of ['sudo chmod -h', 'rm --help']) {
    await expect(page.locator('.busy')).toHaveText('↵');
    await input.fill(command); await input.press('Enter');
    await expect(page.locator('.transcript pre').last()).toContainText('Usage: ');
  }
  expect(requests).toEqual([]);
  await expect(page.locator('.lock')).toContainText('guest');
  // Explicit -- keeps -h a path instead of interpreting it as help.
  await expect(page.locator('.busy')).toHaveText('↵');
  await input.fill('rm -- -h'); await input.press('Enter');
  await expect(page.locator('.transcript pre').last()).toContainText('Permission denied');
  await expect(page.locator('#auth')).not.toBeVisible();
});
