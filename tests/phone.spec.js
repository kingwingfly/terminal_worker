import { test, expect, devices } from '@playwright/test';
import { readFile } from 'node:fs/promises';
import { mkdirp, rmrf } from './sys.js';
const origin = process.env.TERMINAL_TEST_URL || 'http://127.0.0.1:8787';
const password = (await readFile('.dev.vars', 'utf8')).match(/^ADMIN_PASSWORD=(.+)$/m)[1];
const { defaultBrowserType, ...pixel } = devices['Pixel 7'];
test.use(pixel);

let base;
test.beforeEach(async ({ context }) => {
  base = '/phone-' + Date.now().toString(36);
  await context.setExtraHTTPHeaders({ 'CF-Connecting-IP': 'phone-' + base });
  const headers = { Origin: origin };
  expect((await context.request.post('/api/login', { headers, data: { password } })).ok()).toBe(true);
  await mkdirp(context.request, base);
});
test.afterEach(async ({ context }) => {
  await rmrf(context.request, base);
});

test('a phone shows the terminal as an app that keeps its command line above the keyboard', async ({ page }) => {
  await page.goto('/');
  const input = page.getByRole('combobox', { name: 'Terminal command' });
  await expect(page.locator('.busy')).toHaveText('↵');
  // The intro and the side-by-side switch give way; the account stays on one line.
  await expect(page.locator('.intro')).toBeHidden();
  await expect(page.getByRole('button', { name: 'Toggle workspace layout' })).toBeHidden();
  expect(await page.locator('.lock').evaluate(e => e.getBoundingClientRect().height)).toBeLessThan(60);
  const bottom = async () => { const box = await input.boundingBox(); return box.y + box.height; };
  expect(await bottom()).toBeLessThanOrEqual(page.viewportSize().height);
  // Nothing types capitals or corrections into commands.
  await expect(input).toHaveAttribute('autocapitalize', 'off');
  await expect(input).toHaveAttribute('autocorrect', 'off');
  // ls -l: one line an entry, which scrolls sideways rather than wraps.
  await input.tap(); await input.fill('ls -l /'); await input.press('Enter');
  await expect(page.locator('.listing-long .listing-entry').first()).toBeVisible();
  const heights = await page.locator('.listing-long .listing-entry').evaluateAll(entries => entries.map(e => e.getBoundingClientRect().height));
  expect(Math.max(...heights)).toBeLessThan(40);
  // The keyboard takes half the screen: the header and the files make room, and the command
  // line stays in what is left (app.js keeps --app-height as the visible height).
  await page.evaluate(() => { document.documentElement.style.setProperty('--app-height', '400px'); document.documentElement.classList.add('keyboard'); });
  await expect(page.locator('header')).toBeHidden();
  await expect(page.getByRole('region', { name: 'File manager', exact: true })).toBeHidden();
  expect(await bottom()).toBeLessThanOrEqual(400);
  // A tap on the transcript goes to the command line.
  await input.blur();
  await page.locator('.terminal-panel:not([hidden]) .transcript').tap();
  await expect(input).toBeFocused();
});

test('keys a phone lacks are buttons, and Upload opens the chooser at once, before any request', async ({ page }) => {
  await page.goto('/');
  const input = page.getByRole('combobox', { name: 'Terminal command' });
  await expect(page.locator('.busy')).toHaveText('↵');
  await input.fill('cd ' + base); await input.press('Enter');
  await expect(page.locator('.directory-bar')).toContainText(base);
  // Tab completes, ↑ recalls, ^C clears.
  await input.fill('mkd');
  await page.getByRole('button', { name: 'Tab: complete' }).tap();
  // A lone first word may also be a file to open, so completing it lists the directory.
  await expect(input).toHaveValue('mkdir ', { timeout: 15_000 });
  await input.fill('');
  await page.getByRole('button', { name: 'Previous command' }).tap();
  await expect(input).toHaveValue('cd ' + base);
  await page.getByRole('button', { name: 'Ctrl+C: stop' }).tap();
  await expect(input).toHaveValue('');
  // Phones open a chooser only while the tap is fresh: a slow server must not delay it.
  await page.route('**/api/session*', async route => { await new Promise(r => setTimeout(r, 3000)); await route.continue(); });
  const chooser = page.waitForEvent('filechooser', { timeout: 1000 });
  await page.getByRole('button', { name: '↑ Upload files' }).tap();
  await (await chooser).setFiles({ name: 'photo.png', mimeType: 'image/png', buffer: await readFile('tests/fixtures/pixel.png') });
  await expect(page.locator('.transcript')).toContainText('✓ photo.png', { timeout: 20_000 });
  await page.unroute('**/api/session*');
});

test('long names keep one line and scroll; titles keep the file name; either pane hides; Escape closes only the dialog', async ({ page, context }) => {
  const headers = { Origin: origin }, long = 'a-very-long-file-name-that-does-not-fit-on-a-phone-screen-at-all.txt';
  expect((await context.request.put('/api/file?path=' + encodeURIComponent(base + '/' + long), { headers, data: 'long\n' })).status()).toBe(201);
  await page.goto('/');
  const input = page.getByRole('combobox', { name: 'Terminal command' });
  await expect(page.locator('.busy')).toHaveText('↵');
  await input.fill('cd ' + base); await input.press('Enter');
  // One line, sliding to show the rest: a touch screen points at nothing to wait for.
  const name = page.locator('.files .filename', { hasText: long });
  await expect(name).toHaveAttribute('data-overflow', '');
  expect(await name.evaluate(e => e.getBoundingClientRect().height)).toBeLessThan(40);
  expect(await name.locator('.filename-text').evaluate(e => getComputedStyle(e).animationName)).toBe('name-scroll');
  // A title too long for the bar keeps its end, the file name, in view.
  await name.tap();
  await expect(page.locator('#viewer-title')).toHaveText(base + '/' + long);
  const [title, text] = await page.locator('#viewer-title').evaluate(e => [e.getBoundingClientRect(), e.firstElementChild.getBoundingClientRect()].map(r => [r.left, r.right]));
  expect(text[1]).toBeLessThanOrEqual(title[1] + 1); expect(text[0]).toBeLessThan(title[0]);
  expect(await page.locator('.viewer-bar').evaluate(e => e.scrollWidth <= e.clientWidth)).toBe(true);
  // The download button shows too, small, within the bar, framed as the bar's other buttons are.
  await expect(page.getByRole('link', { name: 'Download' })).toBeVisible();
  const [bar, link] = await page.evaluate(() => [document.querySelector('.viewer-bar'), document.getElementById('download')].map(e => e.getBoundingClientRect().right));
  expect(link).toBeLessThanOrEqual(bar);
  const frame = element => page.locator(element).evaluate(e => { const style = getComputedStyle(e); return [style.borderTopStyle, style.borderTopWidth, Math.round(e.getBoundingClientRect().height)]; });
  expect(await frame('#download')).toEqual(await frame('#close-viewer'));
  // Escape closes the dialog, and the page takes the key, which Safari would also take to leave
  // a full-screen window.
  await page.evaluate(() => window.addEventListener('keydown', e => { window.escapeTaken = e.defaultPrevented; }));
  await page.keyboard.press('Escape');
  await expect(page.locator('#viewer')).toBeHidden();
  expect(await page.evaluate(() => window.escapeTaken)).toBe(true);
  // Either pane may be hidden, never both: hiding one shows the other.
  const files = page.getByRole('region', { name: 'File manager', exact: true }), terminal = page.getByRole('region', { name: 'Terminal', exact: true });
  await page.getByRole('button', { name: 'Toggle terminal' }).tap();
  await expect(terminal).toBeHidden(); await expect(files).toBeVisible();
  await page.getByRole('button', { name: 'Toggle file panel' }).tap();
  await expect(files).toBeHidden(); await expect(terminal).toBeVisible();
  await page.getByRole('button', { name: 'Toggle file panel' }).tap();
  await expect(files).toBeVisible(); await expect(terminal).toBeVisible();
});
