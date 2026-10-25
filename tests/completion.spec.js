import { test, expect } from '@playwright/test';
import { readFile } from 'node:fs/promises';
const origin = process.env.TERMINAL_TEST_URL || 'http://127.0.0.1:8788', headers = { Origin: origin };
const password = (await readFile('.dev.vars', 'utf8')).match(/^ADMIN_PASSWORD=(.+)$/m)[1];
const names = ['alpha.txt', 'alphabet.txt', 'z-alpha.txt', 'a_l_p_h_a.txt', 'GammaSpectrum.txt', 'γάμμα-spectrum.md', 'name "quote".txt'];
let base;
test.beforeEach(async ({ request, page }) => {
  base = '/completion-' + Date.now();
  expect((await request.post('/api/login', { headers, data: { password } })).ok()).toBe(true);
  for (const path of [base + '/a-dir', base + '/b-dir']) expect((await request.post('/api/mkdir?path=' + encodeURIComponent(path), { headers })).ok()).toBe(true);
  for (const name of [...names, 'a-dir/inside.txt']) expect((await request.put('/api/file?path=' + encodeURIComponent(base + '/' + name), { headers, data: 'fixture' })).status()).toBe(201);
  await page.goto('/');
  const input = page.getByRole('combobox', { name: 'Terminal command' });
  await input.fill('cd ' + base); await input.press('Enter');
  await expect(page.locator('.directory-bar')).toContainText(base);
});
test.afterEach(async ({ request }) => { await request.delete('/api/file?recursive=true&path=' + encodeURIComponent(base), { headers }); });

test('cycles the original fuzzy matches both ways, browses files on an empty line, and preserves stdout', async ({ page }) => {
  const input = page.getByRole('combobox', { name: 'Terminal command' }), options = page.getByRole('option');
  const transcript = await page.locator('.transcript').innerText();
  await input.fill('open alp'); await input.press('Tab'); await expect(input).toHaveValue('open "alpha.txt"');
  await expect(options).toHaveCount(4);
  await input.press('Tab'); await expect(input).toHaveValue('open "alphabet.txt"');
  await input.press('Shift+Tab'); await expect(input).toHaveValue('open "alpha.txt"');
  await input.press('Shift+Tab'); await expect(input).toHaveValue('open "a_l_p_h_a.txt"');
  await input.press('Tab'); await expect(input).toHaveValue('open "alpha.txt"');
  await input.press('Escape'); await expect(input).toHaveValue('open alp'); await expect(options).toHaveCount(0);
  await input.fill(''); await input.press('Tab'); await expect(input).toHaveValue('cd "a-dir/"');
  await expect(options).toHaveCount(names.length + 2);
  for (let i = 0; i < names.length + 2; i++) await input.press('Tab');
  await expect(input).toHaveValue('cd "a-dir/"');
  await options.filter({ hasText: 'GammaSpectrum.txt' }).click(); await expect(input).toHaveValue('open "GammaSpectrum.txt"');
  await expect(options).toHaveCount(0);
  expect(await page.locator('.transcript').innerText()).toBe(transcript);
  await input.fill('cd '); await input.press('Tab'); await expect(options).toHaveCount(2);
  await input.press('Tab'); await expect(input).toHaveValue('cd "b-dir/"');
  await input.fill('open '); await input.press('Tab'); await expect(options).toHaveCount(names.length + 2);
});

test('matches case-insensitive subsequences and quoted Unicode paths, then descends into a directory', async ({ page }) => {
  const input = page.getByRole('combobox', { name: 'Terminal command' });
  await input.fill('open gMsP'); await input.press('Tab'); await expect(input).toHaveValue('open "GammaSpectrum.txt"');
  await input.fill('open "γα'); await input.press('Tab'); await expect(input).toHaveValue('open "γάμμα-spectrum.md"');
  await input.fill('open quo'); await input.press('Tab'); await expect(input).toHaveValue('open "name \\"quote\\".txt"');
  await input.press('Enter'); await expect(page.locator('.text-preview')).toHaveText('fixture'); await page.getByRole('button', { name: 'Close preview' }).click();
  await input.fill('open a-d'); await input.press('Tab'); await expect(input).toHaveValue('open "a-dir/"');
  await input.press('ArrowRight');
  await expect.poll(() => input.evaluate(e => e.selectionStart)).toBe('open "a-dir/'.length);
  await input.pressSequentially('in'); await input.press('Tab'); await expect(input).toHaveValue('open "a-dir/inside.txt"');
  await input.fill('rm -r -- b-d'); await input.press('Tab'); await expect(input).toHaveValue('rm -r -- "b-dir/"');
  await input.fill('zzzzzzzz'); await input.press('Tab'); await expect(page.locator('.completion-hint')).toHaveText('No matches'); await expect(input).toHaveValue('zzzzzzzz');
  await input.fill('op'); await input.press('Tab'); await expect(input).toHaveValue('open '); await input.press('ArrowRight');
  await input.press('Tab'); await expect(page.getByRole('option')).toHaveCount(names.length + 2);
});

test('queues fast Tab presses and discards late listings after editing', async ({ page }) => {
  const input = page.getByRole('combobox', { name: 'Terminal command' });
  let requests = 0;
  await page.route('**/api/ls?*', async route => { requests++; await new Promise(resolve => setTimeout(resolve, 500)); await route.continue(); });
  await input.fill('open alp'); await input.press('Tab'); await input.press('Tab'); await input.press('Tab');
  await expect(input).toHaveValue('open "z-alpha.txt"'); expect(requests).toBe(1);
  await input.fill('open a-dir/in');
  const response = page.waitForResponse(r => r.url().includes('/api/ls?'));
  await input.press('Tab'); await input.fill('pwd'); await response;
  // A rendering frame lets the discarded response finish all its callbacks.
  await page.evaluate(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))));
  await expect(input).toHaveValue('pwd'); await expect(page.getByRole('option')).toHaveCount(0);
});
