import { test, expect } from '@playwright/test';
import { readFile } from 'node:fs/promises';
import { api, call, mkdirp, put, rmrf } from './sys.js';
const origin = process.env.TERMINAL_TEST_URL || 'http://127.0.0.1:8787';
const password = (await readFile('.dev.vars', 'utf8')).match(/^ADMIN_PASSWORD=(.+)$/m)[1];
const file = path => '/api/file?path=' + encodeURIComponent(path);
let base, headers;

// /bin's programs: uutils' coreutils, built for wasm32-wasip1, each run as a process of its own
// in the page's WASI engine, its calls the kernel's.
test.beforeEach(async ({ request, page }) => {
  base = '/bin-' + Date.now().toString(36);
  headers = { Origin: origin, 'CF-Connecting-IP': 'bin-' + base };
  await page.setExtraHTTPHeaders({ 'CF-Connecting-IP': 'bin-ui-' + base });
  expect((await api(request, 'login', { headers, data: { password } })).ok()).toBe(true);
  await mkdirp(request, base);
  expect((await put(request, file(base + '/names.txt'), { headers, data: 'ann:3\nbob:1\ncid:2\n' })).status()).toBe(201);
});
test.afterEach(async ({ request }) => { await rmrf(request, base); });

const transcript = page => page.locator('.transcript');
// Run `text`, to its end unless it asks something first (su's password).
async function run(page, text, { wait = true } = {}) {
  const input = page.getByRole('combobox', { name: 'Terminal command' });
  const busy = page.locator('#command').locator('xpath=ancestor::form').locator('.busy');
  await expect(busy).toHaveText('↵');
  await input.fill(text); await input.press('Enter');
  if (wait) await expect(busy).toHaveText('↵', { timeout: 30_000 });
}
async function terminal(page) {
  await page.goto('/');
  await run(page, 'su root', { wait: false });
  await page.getByLabel('Password', { exact: true }).fill(password); await page.locator('#auth button[type=submit]').click();
  await expect(page.locator('#auth')).not.toBeVisible();
  await run(page, 'cd ' + base);
}

test('uutils runs as programs of /bin: arguments, the working directory, pipes and exit statuses', async ({ page }) => {
  await terminal(page);
  await run(page, 'seq 3');
  await expect(transcript(page)).toContainText('1\n2\n3');
  // Relative paths are the shell's directory's, as PWD says.
  await run(page, 'cut -d: -f1 names.txt');
  await expect(transcript(page)).toContainText('ann\nbob\ncid');
  // What the shell's own commands write goes on to them, and theirs to the shell's.
  await run(page, 'cat names.txt | tr a-z A-Z | cut -c1-3');
  await expect(transcript(page)).toContainText('ANN\nBOB\nCID');
  await run(page, 'printf "%s-%s\\n" one two');
  await expect(transcript(page)).toContainText('one-two');
  await run(page, 'sha256sum names.txt');
  await expect(transcript(page)).toContainText('names.txt');
  // A failure is the program's own words, on stderr, and a status the shell sees.
  await run(page, 'cut -d: -f1 missing.txt');
  await expect(transcript(page)).toContainText("cut: missing.txt: No such file or directory");
  await run(page, 'basename /a/b/c.txt .txt');
  await expect(page.locator('.transcript pre').last()).toHaveText('c');
  // /bin/ls is uutils' ls, not the shell's.
  await run(page, '/bin/ls');
  await expect(transcript(page)).toContainText('names.txt');
  // A program of /bin reaches the kernel's own calls too, which WASI has no words for, through
  // the engine's terminal.sys: the batch goes as it is.
  await run(page, `syscall '["getcreds",{"lstat":{"path":"names.txt"}},{"stat":{"path":"${base}/names.txt"}}]'`);
  await expect(page.locator('.transcript pre').last()).toContainText('"user":"root"');
  // The kernel takes absolute paths only, as user space makes them.
  await expect(page.locator('.transcript pre').last()).toContainText('{"err":{"detail":"a path must be absolute","errno":"EINVAL"}}');
  await expect(page.locator('.transcript pre').last()).toContainText('"mode":33188');
  await run(page, 'coreutils');
  await expect(transcript(page)).toContainText('coreutils (uutils): arch base32');
  // help lists them, and man says where their help is.
  await run(page, 'help');
  await expect(page.locator('.transcript pre').last()).toContainText("/bin (uutils' coreutils, run as processes of their own)");
  await expect(page.locator('.transcript pre').last()).toContainText('sha256sum');
  await run(page, 'man cut');
  await expect(transcript(page)).toContainText("cut is /bin's (uutils): cut --help says how to use it");
});

test('a program of /bin writes files through the kernel, as its account', async ({ page, request }) => {
  await terminal(page);
  // Written anew in order, closed whole.
  await run(page, 'seq 1000 | tee counted.txt | tail -n 1');
  await expect(transcript(page)).toContainText('1000');
  const counted = await (await request.get(file(base + '/counted.txt'), { headers })).text();
  expect(counted.split('\n').length).toBe(1001);
  // Changed in place: what is there is read, changed and written back whole.
  await run(page, 'truncate -s 4 names.txt');
  expect(await (await request.get(file(base + '/names.txt'), { headers })).text()).toBe('ann:');
  // Appended to: the same.
  await run(page, 'echo more | tee -a names.txt');
  expect(await (await request.get(file(base + '/names.txt'), { headers })).text()).toBe('ann:more\n');
  await run(page, '/bin/mkdir -p made/deeper');
  await run(page, '/bin/touch made/deeper/x');
  const made = await call(request, { stat: { path: base + '/made/deeper/x' } }, { headers });
  expect([made.user, made.size]).toEqual(['root', 0]);
  // The kernel decides: what a guest may not do, it may not do through /bin either.
  await run(page, 'logout');
  await run(page, 'cd ' + base);
  await run(page, '/bin/rm counted.txt');
  await expect(transcript(page)).toContainText("rm: cannot remove 'counted.txt': Permission denied");
});

test('Ctrl+C kills a program of /bin, and output past the limit stops it', async ({ page }) => {
  await terminal(page);
  const input = page.getByRole('combobox', { name: 'Terminal command' });
  const busy = page.locator('#command').locator('xpath=ancestor::form').locator('.busy');
  await input.fill('sleep 30'); await input.press('Enter');
  await expect(busy).not.toHaveText('↵');
  await page.waitForTimeout(500);
  await input.press('Control+c');
  await expect(busy).toHaveText('↵', { timeout: 5_000 });
  await run(page, 'yes | head -n 2');
  await expect(transcript(page)).toContainText('output larger than 10 MB was cut short');
  await expect(transcript(page)).toContainText('y\ny');
});
