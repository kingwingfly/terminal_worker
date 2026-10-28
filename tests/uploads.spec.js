import { test, expect, request as playwrightRequest } from '@playwright/test';
import { readFile } from 'node:fs/promises';
import { createHash } from 'node:crypto';
const origin = process.env.TERMINAL_TEST_URL || 'http://127.0.0.1:8787';
const rootPassword = (await readFile('.dev.vars', 'utf8')).match(/^ADMIN_PASSWORD=(.+)$/m)[1];
const CHUNK = 8 * 1024 * 1024;
const file = path => '/api/file?path=' + encodeURIComponent(path);
const ls = path => '/api/ls?path=' + encodeURIComponent(path);
const sha256 = bytes => createHash('sha256').update(bytes).digest('hex');
let sequence = 0;
async function root() {
  const client = await playwrightRequest.newContext({ baseURL: origin, extraHTTPHeaders: { Origin: origin, 'CF-Connecting-IP': 'uploads-' + Date.now() + '-' + (++sequence) } });
  expect((await client.post('/api/login', { data: { username: 'root', password: rootPassword } })).status()).toBe(200);
  return client;
}
// Two and a half chunks of bytes that differ by position, so that a misplaced chunk shows.
function content() {
  const bytes = Buffer.alloc(CHUNK * 2 + CHUNK / 2);
  for (let i = 0; i < bytes.length; i += 4) bytes.writeUInt32LE(i, i);
  return bytes;
}
const slices = bytes => Array.from({ length: Math.ceil(bytes.length / CHUNK) }, (_, i) => bytes.subarray(i * CHUNK, (i + 1) * CHUNK));
// A chunk of upload `id`, sent by the page holding it with `hold`.
async function chunk(client, id, hold, index, bytes, sha = sha256(bytes)) {
  return client.put(`/api/uploads/chunk?id=${id}&index=${index}`, { headers: { 'X-Chunk-SHA256': sha, 'X-Upload-Hold': hold, 'Content-Type': 'application/octet-stream' }, data: bytes });
}
async function login(page) {
  await page.setExtraHTTPHeaders({ 'CF-Connecting-IP': 'uploads-ui-' + Date.now() });
  await page.goto('/');
  const input = page.getByRole('combobox', { name: 'Terminal command' });
  await input.fill('su root'); await input.press('Enter');
  await expect(page.locator('#auth')).toBeVisible();
  await page.getByLabel('Password', { exact: true }).fill(rootPassword); await page.locator('#auth button[type=submit]').click();
  await expect(page.locator('.busy')).toHaveText('↵');
  return input;
}

test('chunked uploads check every chunk, are readable while they arrive, resume, and cancel', async () => {
  const admin = await root(), reader = await root(), other = await root();
  const base = '/uploads-' + Date.now().toString(36), path = base + '/data.bin';
  const bytes = content(), parts = slices(bytes);
  try {
    expect((await admin.post('/api/mkdir?path=' + encodeURIComponent(base))).status()).toBe(200);
    const begin = await admin.post('/api/uploads?path=' + encodeURIComponent(path), { data: { size: bytes.length, sha256: sha256(bytes) } });
    expect(begin.status(), await begin.text()).toBe(200);
    const { id, chunk_size, hold } = await begin.json();
    expect(chunk_size).toBe(CHUNK); expect(hold).toBeTruthy();
    // A wrong checksum or length is refused, and nothing is committed.
    expect((await chunk(admin, id, hold, 0, parts[0], sha256(parts[1]))).status()).toBe(400);
    expect((await chunk(admin, id, hold, 2, parts[1])).status()).toBe(400);
    expect((await chunk(admin, id, hold, 3, parts[0])).status()).toBe(400);
    expect((await chunk(admin, id, hold, 0, parts[0])).status()).toBe(200);
    // A chunk is committed once: sending it again is harmless, other content is refused.
    expect((await chunk(admin, id, hold, 0, parts[0])).status()).toBe(200);
    expect((await chunk(admin, id, hold, 0, parts[1])).status()).toBe(409);
    // Another account cannot write to this upload, or see it.
    expect((await reader.get('/api/uploads?id=' + id)).status()).toBe(200);
    const status = await (await admin.get('/api/uploads?id=' + id)).json();
    expect(status.committed).toEqual([0]);
    // The file is listed while uploading, with what has arrived so far.
    const entry = (await (await reader.get(ls(base))).json()).entries[0];
    expect(entry.name).toBe('data.bin'); expect(entry.uploaded).toBe(CHUNK);
    const head = await reader.head(file(path));
    expect(head.headers()['x-upload-state']).toBe('uploading');
    expect(head.headers()['content-length']).toBe(String(bytes.length));
    // A range of what has arrived is served at once ...
    const first = await reader.get(file(path), { headers: { Range: 'bytes=0-15' } });
    expect(first.status()).toBe(206); expect(Buffer.compare(await first.body(), bytes.subarray(0, 16))).toBe(0);
    // ... and a whole download follows the upload as it continues.
    const whole = reader.get(file(path), { headers: { 'Accept-Encoding': 'identity' }, timeout: 60_000 });
    await new Promise(r => setTimeout(r, 1500));
    // One page sends an upload at a time: while this one holds it, the same file from another
    // session is refused, and so are chunks without its hold, or a Ctrl+C there.
    const start = client => client.post('/api/uploads?path=' + encodeURIComponent(path), { data: { size: bytes.length, sha256: sha256(bytes) } });
    const refused = await start(other);
    expect(refused.status()).toBe(409); expect((await refused.json()).error).toContain('another tab or device');
    expect((await chunk(other, id, 'not-the-hold', 1, parts[1])).status()).toBe(409);
    expect((await other.delete(`/api/uploads?id=${id}&hold=not-the-hold`)).status()).toBe(409);
    expect((await other.post(`/api/uploads/hold?id=${id}&hold=${hold}`)).status()).toBe(200);
    // Let go of, as a page does when it stops, the upload resumes elsewhere, which then holds
    // it: the first page may not go on.
    expect((await admin.post(`/api/uploads/release?id=${id}&hold=${hold}`)).status()).toBe(200);
    const again = await (await start(other)).json();
    expect(again).toMatchObject({ id, resumed: true, committed: [0] });
    expect(again.hold).not.toBe(hold);
    const stopped = await chunk(admin, id, hold, 2, parts[2]);
    expect(stopped.status()).toBe(409); expect((await stopped.json()).error).toContain('another tab or device');
    expect((await chunk(other, id, again.hold, 2, parts[2])).status()).toBe(200);
    // Progress counts every chunk that arrived, as the uploading page does, not only those
    // readable from the start.
    expect((await (await reader.get(ls(base))).json()).entries[0].uploaded).toBe(CHUNK + parts[2].length);
    expect((await other.post(`/api/uploads/complete?id=${id}&hold=${again.hold}`)).status()).toBe(409);
    expect((await chunk(other, id, again.hold, 1, parts[1])).status()).toBe(200);
    // A chunk sent while the upload completes never changes the completed file.
    const late = chunk(other, id, again.hold, 1, parts[0]);
    await new Promise(r => setTimeout(r, 50));
    expect((await other.post(`/api/uploads/complete?id=${id}&hold=${again.hold}`)).status()).toBe(201);
    expect((await late).status()).toBe(409);
    const downloaded = await whole;
    expect(downloaded.status()).toBe(200);
    expect(Buffer.compare(await downloaded.body(), bytes)).toBe(0);
    expect(Buffer.compare(await (await reader.get(file(path), { headers: { 'Accept-Encoding': 'identity' } })).body(), bytes)).toBe(0);
    expect((await reader.head(file(path))).headers()['x-upload-state']).toBeUndefined();
    expect((await admin.get('/api/uploads?id=' + id)).status()).toBe(404);
    // Cancelling removes a new file at once; jobs cancel, which holds nothing, cancels it from
    // any session of its account.
    const cancelled = await (await admin.post('/api/uploads?path=' + encodeURIComponent(base + '/gone.bin'), { data: { size: 10, sha256: sha256(Buffer.alloc(10)) } })).json();
    expect((await other.delete('/api/uploads?id=' + cancelled.id)).status()).toBe(200);
    expect((await (await reader.get(ls(base))).json()).entries.map(e => e.name)).toEqual(['data.bin']);
  } finally {
    await admin.delete(file(base) + '&recursive=true');
    await admin.dispose(); await reader.dispose(); await other.dispose();
  }
});

test('the browser resumes an interrupted upload from the chunks already sent, and Cancel removes a partial file', async ({ page }) => {
  const folder = '/browser-uploads-' + Date.now().toString(36);
  const input = await login(page);
  for (const command of ['mkdir ' + folder, 'cd ' + folder]) {
    await input.fill(command); await input.press('Enter'); await expect(page.locator('.busy')).toHaveText('↵');
  }
  await expect(page.locator('.directory-bar')).toContainText(folder);
  const bytes = content();
  const sent = [];
  // Drop the connection on the third chunk, until the client gives up after its retries.
  let offline = true;
  await page.route('**/api/uploads/chunk?**', route => {
    const index = Number(new URL(route.request().url()).searchParams.get('index'));
    if (index === 2 && offline) return route.abort('connectionreset');
    sent.push(index); return route.continue();
  });
  const choose = async () => { const chooser = page.waitForEvent('filechooser'); await page.getByRole('button', { name: '↑ Upload files' }).click(); await (await chooser).setFiles({ name: 'data.log', mimeType: 'application/octet-stream', buffer: bytes }); };
  await choose();
  // Progress and results show in the tab that uploads, not over the page.
  const transcript = page.locator('.terminal-panel:not([hidden]) .transcript');
  await expect(transcript).toContainText('choose the same file again to resume', { timeout: 30_000 });
  expect(sent.sort()).toEqual([0, 1]);
  offline = false;
  await expect(page.locator('.filename').filter({ hasText: 'data.log' })).toBeVisible();
  sent.length = 0;
  await choose();
  await expect(transcript).toContainText('✓ data.log (resumed)', { timeout: 30_000 });
  expect(sent).toEqual([2]);
  const stored = await page.request.get(file(folder + '/data.log'), { headers: { 'Accept-Encoding': 'identity' } });
  expect(Buffer.compare(await stored.body(), bytes)).toBe(0);
  // Slow chunks down, and cancel during the upload.
  await page.unroute('**/api/uploads/chunk?**');
  await page.route('**/api/uploads/chunk?**', async route => { await new Promise(r => setTimeout(r, 1500)); return route.continue(); });
  const chooser = page.waitForEvent('filechooser'); await page.getByRole('button', { name: '↑ Upload files' }).click();
  await (await chooser).setFiles({ name: 'other.log', mimeType: 'application/octet-stream', buffer: bytes.subarray(0, CHUNK + 5) });
  await expect(transcript).toContainText('↑ other.log', { timeout: 15_000 });
  await page.locator('#command').press('Control+c');
  await expect(transcript).toContainText('✕ other.log: Upload cancelled', { timeout: 15_000 });
  await expect.poll(async () => (await (await page.request.get(ls(folder))).json()).entries.map(e => e.name)).toEqual(['data.log']);
  await page.unroute('**/api/uploads/chunk?**');
  // Any file uploads: media are served as such, what is neither text nor media as bytes.
  for (const [name, type] of [['tone.aac', 'audio/aac'], ['report.docx', 'application/octet-stream']]) {
    const chooser = page.waitForEvent('filechooser'); await page.getByRole('button', { name: '↑ Upload files' }).click();
    await (await chooser).setFiles({ name, mimeType: 'application/octet-stream', buffer: Buffer.from([0xFF, 0xF1, 0x50, 0x80, 0x00, 0x1F, 0xFC]) });
    await expect(transcript).toContainText('✓ ' + name, { timeout: 15_000 });
    expect((await page.request.head(file(folder + '/' + name))).headers()['content-type']).toBe(type);
  }
  // A chooser the browser closes without saying so, or never opens, leaves upload waiting
  // until Ctrl+C, which gives it no files.
  await page.evaluate(() => { HTMLInputElement.prototype.click = function () {}; });
  await input.fill('upload'); await input.press('Enter');
  await expect(page.locator('.busy')).toHaveText('Working...');
  await page.locator('#command').press('Control+c');
  await expect(page.locator('.busy')).toHaveText('↵');
  await page.request.delete(file(folder) + '&recursive=true', { headers: { Origin: origin } });
});
