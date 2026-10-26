import { test, expect, request as playwrightRequest } from '@playwright/test';
import { readFile } from 'node:fs/promises';
import http from 'node:http';
const origin = process.env.TERMINAL_TEST_URL || 'http://127.0.0.1:8787';
const rootPassword = (await readFile('.dev.vars', 'utf8')).match(/^ADMIN_PASSWORD=(.+)$/m)[1];
const file = path => '/api/file?path=' + encodeURIComponent(path);
const ls = path => '/api/ls?path=' + encodeURIComponent(path);
async function root() {
  const client = await playwrightRequest.newContext({ baseURL: origin, extraHTTPHeaders: { Origin: origin, 'CF-Connecting-IP': 'storage-' + Date.now() } });
  expect((await client.post('/api/login', { data: { username: 'root', password: rootPassword } })).status()).toBe(200);
  return client;
}
// A PUT whose body arrives slowly, in `parts` pieces `delay` ms apart.
function slowPut(path, cookie, parts, delay) {
  const url = new URL(file(path), origin);
  const piece = Buffer.alloc(64 * 1024, 'x');
  return new Promise((resolve, reject) => {
    const req = http.request(url, { method: 'PUT', headers: { Origin: origin, Cookie: cookie, 'Content-Length': piece.length * parts } }, res => {
      let body = ''; res.on('data', d => body += d); res.on('end', () => resolve({ status: res.statusCode, body, at: Date.now() }));
    });
    req.on('error', reject);
    let sent = 0;
    const next = () => { req.write(piece); if (++sent === parts) req.end(); else setTimeout(next, delay); };
    next();
  });
}

test('a slow upload blocks neither listings nor downloads of other files, and its own download waits for it', async () => {
  const admin = await root(), other = await root();
  const base = '/storage-' + Date.now().toString(36);
  try {
    expect((await admin.post('/api/mkdir?path=' + encodeURIComponent(base))).status()).toBe(200);
    expect((await admin.put(file(base + '/ready.txt'), { data: 'already here' })).status()).toBe(201);
    const cookie = (await admin.storageState()).cookies.map(c => `${c.name}=${c.value}`).join('; ');
    const started = Date.now();
    const upload = slowPut(base + '/slow.bin', cookie, 20, 150);
    await new Promise(r => setTimeout(r, 400));
    // While the upload streams, other requests answer at once.
    const listing = await other.get(ls(base)); const listed = Date.now();
    expect(listing.status()).toBe(200);
    const names = (await listing.json()).entries.map(e => e.name);
    expect(names).toEqual(expect.arrayContaining(['ready.txt', 'slow.bin']));
    const ready = await other.get(file(base + '/ready.txt')); const read = Date.now();
    expect(await ready.text()).toBe('already here');
    // An unfinished file says so, and a download of it follows the upload to its end.
    expect((await other.head(file(base + '/slow.bin'))).headers()['x-upload-state']).toBe('uploading');
    const following = other.get(file(base + '/slow.bin')).then(async r => ({ status: r.status(), length: (await r.body()).length, at: Date.now() }));
    const done = await upload;
    expect(done.status, done.body).toBe(201);
    expect(listed).toBeLessThan(done.at); expect(read).toBeLessThan(done.at);
    expect(done.at - started).toBeGreaterThan(2500);
    const complete = await following;
    expect(complete).toMatchObject({ status: 200, length: 20 * 64 * 1024 });
    expect(complete.at).toBeGreaterThanOrEqual(done.at - 50);
    // Text may be compressed in transit; the declared length is still the file's.
    expect((await other.head(file(base + '/slow.bin'))).headers()['content-length']).toBe(String(20 * 64 * 1024));
  } finally {
    await admin.delete(file(base) + '&recursive=true');
    await admin.dispose(); await other.dispose();
  }
});

test('an interrupted upload leaves no file behind, and an overwrite shows the old content until it completes', async () => {
  const admin = await root(), other = await root();
  const base = '/storage-' + Date.now().toString(36);
  try {
    expect((await admin.post('/api/mkdir?path=' + encodeURIComponent(base))).status()).toBe(200);
    const cookie = (await admin.storageState()).cookies.map(c => `${c.name}=${c.value}`).join('; ');
    // Promise a body, send part of it, and hang up.
    await new Promise(resolve => {
      const req = http.request(new URL(file(base + '/broken.bin'), origin), { method: 'PUT', headers: { Origin: origin, Cookie: cookie, 'Content-Length': 1_000_000 } });
      req.on('error', () => resolve()); req.write(Buffer.alloc(1000)); setTimeout(() => { req.destroy(); resolve(); }, 300);
    });
    await expect.poll(async () => (await (await other.get(ls(base))).json()).entries.map(e => e.name), { timeout: 10_000 }).toEqual([]);
    expect((await admin.put(file(base + '/a.txt'), { data: 'old' })).status()).toBe(201);
    // Without overwrite, an existing name is refused before any byte is stored.
    expect((await slowPut(base + '/a.txt', cookie, 1, 0)).status).toBe(409);
    const etag = (await other.head(file(base + '/a.txt'))).headers().etag;
    const replacing = new Promise((resolve, reject) => {
      const url = new URL(file(base + '/a.txt') + '&overwrite=true', origin);
      const req = http.request(url, { method: 'PUT', headers: { Origin: origin, Cookie: cookie, 'Content-Length': 6, 'If-Match': etag } }, res => { res.resume(); res.on('end', () => resolve(res.statusCode)); });
      req.on('error', reject); req.write('new'); setTimeout(() => { req.end('!!!'); }, 600);
    });
    await new Promise(r => setTimeout(r, 300));
    expect(await (await other.get(file(base + '/a.txt'))).text()).toBe('old');
    expect(await replacing).toBe(200);
    expect(await (await other.get(file(base + '/a.txt'))).text()).toBe('new!!!');
    expect((await other.head(file(base + '/a.txt'))).headers().etag).not.toBe(etag);
  } finally {
    await admin.delete(file(base) + '&recursive=true');
    await admin.dispose(); await other.dispose();
  }
});
