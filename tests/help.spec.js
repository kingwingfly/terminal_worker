import { test, expect } from '@playwright/test';
import { readFile, readdir } from 'node:fs/promises';

// Every command defined for clap in crates/core/src/cli.
const definitions = await Promise.all((await readdir('crates/core/src/cli')).filter(name => name.endsWith('.rs')).map(name => readFile('crates/core/src/cli/' + name, 'utf8')));
const commands = definitions.flatMap(source => [...source.matchAll(/#\[command\(\s*name = "([a-z]+)"/g)].map(match => match[1]));

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

test('help lists every command by section, and -h is an option where Linux makes it one', async ({ page }) => {
  await page.goto('/');
  const input = page.getByRole('combobox', { name: 'Terminal command' });
  const last = page.locator('.transcript pre').last();
  const run = async text => { await expect(page.locator('.busy')).toHaveText('↵'); await input.fill(text); await input.press('Enter'); await expect(page.locator('.busy')).toHaveText('↵'); };
  await run('help');
  for (const section of ['Files', 'Text', 'Archives', 'Permissions', 'Accounts', 'Terminal']) await expect(last).toContainText(section);
  for (const command of commands) await expect(last).toContainText(new RegExp(`^  ${command} +\\S`, 'm'));
  await run('help ln'); await expect(last).toContainText('Usage: ln');
  // Options and examples line up in columns.
  await run('cp -h'); await expect(last).toContainText('Options:\n  -r, -R ');
  await expect(last).toContainText('Examples:\n  cp notes.md notes-backup.md');
  // chown -h alone is help; with operands, it is chown's own -h, which acts on a link itself.
  await run('chown -h'); await expect(last).toContainText('Usage: chown');
  await run('chown -h root /nothing-here'); await expect(last).not.toContainText('Usage: chown');
});
