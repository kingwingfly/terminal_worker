import { test, expect, request as playwrightRequest } from '@playwright/test';
import { readFile } from 'node:fs/promises';
const origin = process.env.TERMINAL_TEST_URL || 'http://127.0.0.1:8787';
const password = (await readFile('.dev.vars', 'utf8')).match(/^ADMIN_PASSWORD=(.+)$/m)[1];
const file = path => '/api/file?path=' + encodeURIComponent(path);
let sequence = 0;
// Another device, signed in as root through the API.
async function device() {
  const client = await playwrightRequest.newContext({ baseURL: origin, extraHTTPHeaders: { Origin: origin, 'CF-Connecting-IP': 'sync-' + Date.now() + '-' + (++sequence) } });
  expect((await client.post('/api/login', { data: { password } })).ok()).toBe(true);
  return client;
}

test('a page shows what other devices change as it happens, and hears nothing of what it may not list', async ({ page, context, browser }) => {
  const other = await device(), base = '/sync-' + Date.now().toString(36);
  try {
    expect((await other.post('/api/mkdir?path=' + encodeURIComponent(base))).ok()).toBe(true);
    await context.setExtraHTTPHeaders({ 'CF-Connecting-IP': 'sync-page-' + Date.now() });
    expect((await context.request.post('/api/login', { headers: { Origin: origin }, data: { password } })).ok()).toBe(true);
    await page.goto('/');
    const input = page.getByRole('combobox', { name: 'Terminal command' });
    await expect(page.locator('.busy')).toHaveText('↵');
    await input.fill('cd ' + base); await input.press('Enter');
    await expect(page.locator('.directory-bar')).toContainText(base);
    // Without anything typed here, the files follow the other device.
    const names = page.locator('.files .filename');
    expect((await other.put(file(base + '/hello.txt'), { data: 'hi' })).status()).toBe(201);
    await expect(names).toHaveText(['hello.txt']);
    expect((await other.post('/api/mkdir?path=' + encodeURIComponent(base + '/docs'))).ok()).toBe(true);
    await expect(names).toHaveText(['docs/', 'hello.txt']);
    expect((await other.post('/api/command', { data: { args: ['mv', base + '/hello.txt', base + '/docs/'], cwd: '/' } })).ok()).toBe(true);
    await expect(names).toHaveText(['docs/']);
    // A guest's page hears of a directory it may list, and nothing of /root.
    const guest = await browser.newContext({ extraHTTPHeaders: { 'CF-Connecting-IP': 'sync-guest-' + Date.now() } });
    const listener = await guest.newPage(); await listener.goto(origin + '/');
    await listener.evaluate(url => new Promise(opened => {
      window.heard = []; const socket = new WebSocket(url.replace(/^http/, 'ws') + '/api/events');
      socket.onmessage = event => window.heard.push(...JSON.parse(event.data).changed); socket.onopen = opened;
    }), origin);
    expect((await other.put(file('/root/sync-secret.txt') + '&overwrite=true', { data: 'x' })).status()).toBeLessThan(300);
    expect((await other.put(file(base + '/public.txt'), { data: 'x' })).status()).toBe(201);
    await expect.poll(() => listener.evaluate(() => window.heard)).toContain(base);
    expect(await listener.evaluate(() => window.heard)).not.toContain('/root');
    await guest.close();
    // Only this site may open the line, which carries the session cookie.
    expect((await other.get('/api/events', { headers: { Origin: 'https://elsewhere.example' } })).status()).toBe(403);
    expect((await other.get('/api/events')).status()).toBe(426);
  } finally {
    await other.delete(file(base) + '&recursive=true'); await other.delete(file('/root/sync-secret.txt'));
    await other.dispose();
  }
});

test('removing a file another device is uploading stops that upload, as rm does to a file being written', async ({ page, context }) => {
  const other = await device(), base = '/sync-upload-' + Date.now().toString(36);
  try {
    expect((await other.post('/api/mkdir?path=' + encodeURIComponent(base))).ok()).toBe(true);
    await context.setExtraHTTPHeaders({ 'CF-Connecting-IP': 'sync-uploader-' + Date.now() });
    expect((await context.request.post('/api/login', { headers: { Origin: origin }, data: { password } })).ok()).toBe(true);
    await page.goto('/');
    const input = page.getByRole('combobox', { name: 'Terminal command' });
    await expect(page.locator('.busy')).toHaveText('↵');
    await input.fill('cd ' + base); await input.press('Enter');
    await expect(page.locator('.directory-bar')).toContainText(base);
    // Slow chunks leave the other device time to remove the file.
    // A chunk the stopped upload aborted meanwhile is handled already, and continues nowhere.
    await page.route('**/api/uploads/chunk?**', async route => { await new Promise(r => setTimeout(r, 1500)); await route.continue().catch(() => {}); });
    const chooser = page.waitForEvent('filechooser');
    await input.fill('upload'); await input.press('Enter');
    await (await chooser).setFiles({ name: 'big.log', mimeType: 'application/octet-stream', buffer: Buffer.alloc(3 * 8 * 1024 * 1024 + 100, 7) });
    await expect(page.locator('.files .filename')).toContainText(['big.log'], { timeout: 20_000 });
    expect((await other.delete(file(base + '/big.log'))).ok()).toBe(true);
    await expect(page.locator('.transcript')).toContainText('✕ big.log: stopped: the file was removed, or its upload cancelled, elsewhere', { timeout: 30_000 });
    await expect(page.locator('.files .filename')).toHaveCount(0);
  } finally {
    await page.unroute('**/api/uploads/chunk?**');
    await other.delete(file(base) + '&recursive=true');
    await other.dispose();
  }
});
