import { test, expect } from '@playwright/test';
import { readFile } from 'node:fs/promises';
const origin = process.env.TERMINAL_TEST_URL || 'http://127.0.0.1:8787';
const password = (await readFile('.dev.vars', 'utf8')).match(/^ADMIN_PASSWORD=(.+)$/m)[1];
const headers = { Origin: origin };
const url = (path, recursive = false) => '/api/file?path=' + encodeURIComponent(path) + '&recursive=' + recursive;
async function command(page, text) {
  const input = page.getByRole('combobox', { name: 'Terminal command' });
  await expect(page.locator('.busy')).toHaveText('↵');
  await input.fill(text); await input.press('Enter');
}

test('stdout and errors scroll after DOM updates; completion uses a separate pager', async ({ page }) => {
  await page.goto('/');
  await expect(page.getByRole('heading', { name: 'Your files, right here.' })).toBeVisible();
  const log = page.locator('.transcript');
  await page.route('**/api/ls?*', async route => { await new Promise(resolve => setTimeout(resolve, 120)); await route.continue(); });
  for (const width of [1440, 390]) {
    await page.setViewportSize({ width, height: 780 });
    for (let i = 0; i < 4; i++) { await command(page, 'help'); await expect(log.locator('pre').last()).toContainText('useradd NAME makes an account'); }
    expect(await log.evaluate(e => e.scrollHeight > e.clientHeight)).toBe(true);
    await log.evaluate(e => { e.scrollTop = 0; });
    await command(page, 'ls');
    await expect(log.locator('pre').last()).not.toContainText('visitor:');
    await expect.poll(() => log.evaluate(e => e.scrollHeight - e.clientHeight - e.scrollTop)).toBeLessThanOrEqual(2);
    await command(page, 'unknown-command');
    await expect(log.locator('pre').last()).toContainText('Unknown command');
    await expect.poll(() => log.evaluate(e => e.scrollHeight - e.clientHeight - e.scrollTop)).toBeLessThanOrEqual(2);
    const input = page.getByRole('combobox', { name: 'Terminal command' });
    const transcript = await log.innerText();
    await input.fill(''); await input.press('Tab');
    await expect(page.getByRole('listbox', { name: 'Completions' })).toBeVisible();
    expect(await log.innerText()).toBe(transcript);
    await input.press('Escape');
  }
});

test('delete requires auth and origin; recursive deletion preserves neighboring prefixes', async ({ request }) => {
  const base = '/rm-api-' + Date.now(), target = base + '/directory', sibling = base + '/Directory-kept';
  expect((await request.delete(url(base), { headers })).status()).toBe(401);
  await request.post('/api/login', { headers, data: { password } });
  try {
    for (const dir of [target + '/nested', sibling]) {
      expect((await request.post('/api/mkdir?path=' + encodeURIComponent(dir), { headers })).ok()).toBe(true);
      expect((await request.put(url(dir + '/With spaces.txt'), { headers, data: 'keep or delete' })).status()).toBe(201);
    }
    expect((await request.delete(url(target, true), { headers: { Origin: 'https://elsewhere.invalid' } })).status()).toBe(403);
    expect((await request.delete(url('/a/..', true), { headers })).status()).toBe(403);
    expect((await request.delete(url(target), { headers })).status()).toBe(409);
    expect((await request.delete(url(base + '/missing.txt'), { headers })).status()).toBe(404);
    expect((await request.delete(url(target, true), { headers })).ok()).toBe(true);
    expect((await request.get('/api/ls?path=' + encodeURIComponent(target))).status()).toBe(404);
    expect((await request.get(url(target + '/nested/With spaces.txt'))).status()).toBe(404);
    expect((await request.get(url(sibling + '/With spaces.txt'))).ok()).toBe(true);
    expect((await request.delete(url(sibling + '/With spaces.txt'), { headers })).ok()).toBe(true);
    expect((await request.head(url(sibling + '/With spaces.txt'))).status()).toBe(404);
    expect((await request.get('/api/ls?path=' + encodeURIComponent(sibling))).ok()).toBe(true);
  } finally { await request.delete(url(base, true), { headers }); }
});

test('rm requires explicit login, handles quoted paths, refreshes lists and refuses cwd deletion', async ({ page }) => {
  const base = '/rm-ui-' + Date.now();
  await page.goto('/');
  await command(page, 'rm');
  await expect(page.locator('.transcript pre').last()).toContainText('rm: missing PATH');
  await command(page, 'su root');
  await expect(page.locator('#auth')).toBeVisible();
  await page.getByLabel('Password', { exact: true }).fill(password);
  await page.locator('#auth button[type=submit]').click();
  await expect(page.locator('.busy')).toHaveText('↵');
  await command(page, 'mkdir ' + base);
  await expect(page.locator('.transcript')).toContainText('Created ' + base);
  try {
    await command(page, 'cd ' + base);
    await expect(page.locator('.directory-bar')).toContainText(base);
    await command(page, 'mkdir "Delete this directory"');
    await expect(page.locator('.filename').filter({ hasText: 'Delete this directory' })).toBeVisible();
    await command(page, 'rm "Delete this directory"');
    await expect(page.locator('.transcript pre').last()).toContainText('This is a directory');
    const input = page.getByRole('combobox', { name: 'Terminal command' });
    await input.fill('rm -r Dele'); await input.press('Tab');
    await expect(input).toHaveValue('rm -r "Delete this directory/"'); await input.press('Enter');
    await expect(page.locator('.filename').filter({ hasText: 'Delete this directory' })).toHaveCount(0);
    await command(page, 'rm -r .'); await expect(page.locator('.transcript pre').last()).toContainText('Cannot remove the current directory');
    const chooser = page.waitForEvent('filechooser'); await page.getByRole('button', { name: '↑ Upload files' }).click();
    await (await chooser).setFiles({ name: 'Delete me.txt', mimeType: 'text/plain', buffer: Buffer.from('temporary') });
    await expect(page.locator('.filename').filter({ hasText: 'Delete me.txt' })).toBeVisible();
    await command(page, 'logout'); await expect(page.locator('.lock')).toContainText('guest');
    await command(page, 'rm "' + base + '/Delete me.txt"');
    await expect(page.locator('.transcript pre').last()).toContainText('Permission denied');
    await expect(page.locator('#auth')).not.toBeVisible();
    await expect(page.locator('.lock')).toContainText('guest');
    await command(page, 'su root'); await expect(page.locator('#auth')).toBeVisible();
    await page.getByLabel('Password', { exact: true }).fill(password); await page.locator('#auth button[type=submit]').click();
    await expect(page.locator('.busy')).toHaveText('↵');
    await command(page, 'rm "' + base + '/Delete me.txt"');
    await expect(page.locator('.filename').filter({ hasText: 'Delete me.txt' })).toHaveCount(0);
    await expect(page.locator('.transcript pre').last()).toContainText('Removed ' + base + '/Delete me.txt');
  } finally {
    await page.request.delete(url(base, true), { headers });
  }
});
