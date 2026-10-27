import { test, expect, request as playwrightRequest } from '@playwright/test';
import { readFile } from 'node:fs/promises';
const origin = process.env.TERMINAL_TEST_URL || 'http://127.0.0.1:8787';
const password = (await readFile('.dev.vars', 'utf8')).match(/^ADMIN_PASSWORD=(.+)$/m)[1];
const file = path => '/api/file?path=' + encodeURIComponent(path);
let base, headers, sequence = 0;

test.beforeEach(async ({ request, page }) => {
  base = '/links-' + Date.now().toString(36);
  headers = { Origin: origin, 'CF-Connecting-IP': 'links-' + base };
  await page.setExtraHTTPHeaders({ 'CF-Connecting-IP': 'links-ui-' + base });
  expect((await request.post('/api/login', { headers, data: { password } })).ok()).toBe(true);
  for (const dir of ['', '/docs']) expect((await request.post('/api/mkdir?path=' + encodeURIComponent(base + dir), { headers })).ok()).toBe(true);
  for (const [name, text] of Object.entries({ 'a.md': 'alpha\n', 'docs/b.md': 'beta\n' })) expect((await request.put(file(base + '/' + name), { headers, data: text })).status()).toBe(201);
});
test.afterEach(async ({ request }) => { await request.delete(file(base) + '&recursive=true', { headers }); });

const busy = page => page.locator('.terminal-panel:not([hidden]) .busy');
const last = page => page.locator('.terminal-panel:not([hidden]) .transcript pre').last();
async function run(page, text) {
  await expect(busy(page)).toHaveText('↵');
  await page.locator('#command').fill(text); await page.locator('#command').press('Enter');
  await expect(busy(page)).toHaveText('↵');
}
async function output(page, text) { await run(page, text); return (await last(page).textContent()).trim(); }
async function terminal(page) {
  await page.goto('/');
  await expect(busy(page)).toHaveText('↵');
  await page.locator('#command').fill('su root'); await page.locator('#command').press('Enter');
  await page.getByLabel('Password', { exact: true }).fill(password); await page.locator('#auth button[type=submit]').click();
  await expect(page.locator('#auth')).not.toBeVisible();
  await run(page, 'cd ' + base);
}
/** A client signed out, as a new visitor is. */
async function visitor() {
  return playwrightRequest.newContext({ baseURL: origin, extraHTTPHeaders: { Origin: origin, 'CF-Connecting-IP': 'links-guest-' + Date.now() + '-' + (++sequence) } });
}

test('ln makes symbolic and hard links that behave as on Linux', async ({ page, request }) => {
  await terminal(page);
  await run(page, 'ln -s docs/b.md link.md'); await run(page, 'ln a.md a-too.md');
  // ls -l shows where a link points, and how many names a file has.
  // Piped, a listing is text, a line an entry.
  const long = await output(page, 'ls -l | cat');
  expect(long).toMatch(/^lrwxrwxrwx +1 root root +9 link\.md -> docs\/b\.md$/m);
  expect(long).toMatch(/^-rw-r--r-- +2 root root +6 a-too\.md$/m);
  expect(await output(page, 'cat link.md')).toBe('beta');
  expect(await output(page, 'readlink link.md')).toBe('docs/b.md');
  expect(await output(page, 'readlink -f link.md')).toBe(base + '/docs/b.md');
  // Writing through one name of a file changes it under every name; chmod too.
  expect((await request.put(file(base + '/a-too.md') + '&overwrite=true', { headers, data: 'changed\n' })).ok()).toBe(true);
  expect(await (await request.get(file(base + '/a.md'), { headers })).text()).toBe('changed\n');
  await run(page, 'chmod 600 a-too.md');
  expect(await output(page, 'ls -l a.md')).toMatch(/^-rw------- +2 /);
  // rm removes a link, not what it points to; a name, not the file's other names.
  await run(page, 'rm link.md a.md');
  expect(await (await request.get(file(base + '/docs/b.md'), { headers })).text()).toBe('beta\n');
  expect(await (await request.get(file(base + '/a-too.md'), { headers })).text()).toBe('changed\n');
  // A link to a directory is entered by its name, and left the same way.
  await run(page, 'ln -s docs d'); await run(page, 'cd d');
  expect(await output(page, 'pwd')).toBe(base + '/d');
  expect(await output(page, 'ls')).toBe('b.md');
  await run(page, 'cd ..'); expect(await output(page, 'pwd')).toBe(base);
  // ls -ld shows the link itself; a trailing slash follows it.
  expect(await output(page, 'ls -ld d')).toContain('d -> docs');
  expect(await output(page, 'ls d/')).toBe('b.md');
  for (const [line, message] of [['ln docs hard', 'hard link not allowed for directory'], ['ln -s a-too.md docs/b.md', 'File exists'], ['ln -s loop loop', '']]) {
    await run(page, line);
    if (message) await expect(last(page)).toContainText(message);
  }
  await run(page, 'cat loop'); await expect(last(page)).toContainText('Too many levels of symbolic links');
  // ln -sf repoints a link; fd -t l finds links.
  await run(page, 'ln -sfn a-too.md d'); expect(await output(page, 'readlink d')).toBe('a-too.md');
  expect(await output(page, 'fd -t l | sort')).toBe('d\nloop');
  // cp follows a link named; -r copies links as links; -a keeps names of a file together.
  await run(page, 'ln a-too.md docs/a3.md');
  // The copy of a file with mode 600 is 600 too: the umask only takes away.
  await run(page, 'cp d plain.md'); expect(await output(page, 'ls -l plain.md')).toMatch(/^-rw------- +1 /);
  await run(page, 'cp -r . ../copy-' + base.slice(1));
  expect(await output(page, `readlink ../copy-${base.slice(1)}/d`)).toBe('a-too.md');
  await run(page, 'cp -a . ../kept-' + base.slice(1));
  expect(await output(page, `ls -l ../kept-${base.slice(1)}/docs/a3.md`)).toMatch(/^-rw------- +2 /);
  for (const dir of ['copy', 'kept']) await request.delete(file(`/${dir}-${base.slice(1)}`) + '&recursive=true', { headers });
});

test('a visitor makes an account in /guest, which is sticky, and shares files with ACLs', async ({ page, request }) => {
  const ada = 'ada' + Date.now().toString(36), bob = 'bob' + Date.now().toString(36), pass = 'guest-password-' + Date.now();
  await page.goto('/');
  await expect(busy(page)).toHaveText('↵');
  // Signed out, one is guest, a real account in group guest.
  expect(await output(page, 'id')).toBe('uid=65534(guest) gid=65534(guest) groups=65534(guest)');
  await page.locator('#command').fill('useradd ' + ada); await page.locator('#command').press('Enter');
  await expect(page.locator('#auth-title')).toHaveText('Create account ' + ada);
  await page.getByLabel('New password', { exact: true }).fill(pass); await page.getByLabel('Confirm new password').fill(pass);
  await page.locator('#auth button[type=submit]').click();
  await expect(page.locator('#auth')).not.toBeVisible();
  await expect(busy(page)).toHaveText('↵');
  await expect(last(page)).toContainText(`Created user ${ada} and signed in; home /guest/${ada}`);
  expect(await output(page, 'pwd')).toBe('/guest/' + ada);
  expect(await output(page, 'groups')).toBe(`${ada} guest`);
  expect(await output(page, 'ls -ld /guest | cat')).toMatch(/^drwxrwxrwt +\d+ root guest/);
  expect(await output(page, 'ls -ld . | cat')).toMatch(new RegExp(`^drwx------ +2 ${ada} ${ada}`));
  // Another visitor makes another account through the API, signed in by it.
  const other = await visitor();
  try {
    const made = await other.post('/api/register', { data: { username: bob, password: pass } });
    expect(made.status(), await made.text()).toBe(201);
    expect((await other.post('/api/register', { data: { username: bob + 'x', password: pass } })).status()).toBe(409);
    expect((await (await visitor()).post('/api/register', { data: { username: ada, password: pass } })).status()).toBe(409);
    // Homes are their owners' only.
    await run(page, 'touch note.txt');
    expect((await other.get(file(`/guest/${ada}/note.txt`))).status()).toBe(403);
    // ada lets bob through her home and read the note: named entries, bounded by the mask.
    await run(page, `setfacl -m u:${bob}:x .`); await run(page, `setfacl -m u:${bob}:rw note.txt`);
    expect((await other.get(file(`/guest/${ada}/note.txt`))).status()).toBe(200);
    expect(await output(page, 'ls -l note.txt')).toMatch(/^-rw-rw-r--\+ /);
    await run(page, 'chmod g-w note.txt');
    expect(await output(page, 'getfacl note.txt')).toBe(`# file: guest/${ada}/note.txt\n# owner: ${ada}\n# group: ${ada}\nuser::rw-\nuser:${bob}:rw-\t#effective:r--\ngroup::r--\nmask::r--\nother::r--`);
    expect((await other.put(file(`/guest/${ada}/note.txt`) + '&overwrite=true', { data: 'x' })).status()).toBe(403);
    // A default ACL is what new files get.
    await run(page, `setfacl -d -m u:${bob}:rw .`); await run(page, 'touch later.txt');
    expect(await output(page, `getfacl -c later.txt`)).toContain(`user:${bob}:rw-`);
    // /guest is sticky: anyone may put a file there, and only its owner remove it.
    await run(page, `touch /guest/${ada}-shared.txt`);
    const removal = await other.delete(file(`/guest/${ada}-shared.txt`));
    expect(removal.status()).toBe(403); expect((await removal.json()).error).toContain('Operation not permitted');
    await run(page, `rm /guest/${ada}-shared.txt`);
    // Extended attributes go with the file, for whoever may write it.
    await run(page, 'setfattr -n user.status -v reviewed note.txt');
    expect(await output(page, 'getfattr -d note.txt')).toBe(`# file: guest/${ada}/note.txt\nuser.status="reviewed"`);
    await run(page, 'setfattr -n trusted.tag -v x note.txt'); await expect(last(page)).toContainText('Operation not permitted');
    // guest cannot get a password, nor be changed.
    await run(page, 'exit');
    expect((await request.post('/api/passwd', { headers, data: { username: 'guest', password: pass, sudo: true } })).status()).toBe(400);
    expect((await request.post('/api/command', { headers, data: { args: ['userdel', 'guest'] } })).status()).toBe(400);
  } finally {
    for (const name of [ada, bob]) {
      await request.post('/api/command', { headers, data: { args: ['userdel', '-r', name] } });
      await request.post('/api/command', { headers, data: { args: ['groupdel', name] } });
    }
    await other.dispose();
  }
});
