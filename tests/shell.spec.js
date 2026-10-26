import { test, expect } from '@playwright/test';
import { readFile } from 'node:fs/promises';
const origin = process.env.TERMINAL_TEST_URL || 'http://127.0.0.1:8787';
// Each test signs in as its own client, so that it does not use up others' login rate limit.
let headers;
const password = (await readFile('.dev.vars', 'utf8')).match(/^ADMIN_PASSWORD=(.+)$/m)[1];
const file = path => '/api/file?path=' + encodeURIComponent(path);
const command = (request, args, cwd = '/') => request.post('/api/command', { headers, data: { args, cwd } });
let base;

test.beforeEach(async ({ request, page }) => {
  base = '/shell-' + Date.now().toString(36);
  headers = { Origin: origin, 'CF-Connecting-IP': 'shell-' + base };
  await page.setExtraHTTPHeaders({ 'CF-Connecting-IP': 'shell-ui-' + base });
  expect((await request.post('/api/login', { headers, data: { password } })).ok()).toBe(true);
  for (const dir of ['', '/docs', '/docs/deep', '/.hidden']) expect((await request.post('/api/mkdir?path=' + encodeURIComponent(base + dir), { headers })).ok()).toBe(true);
  const files = { 'a.md': 'alpha\nbeta\n', 'docs/b.md': 'one\ntwo\nthree\n', 'docs/deep/spéctrum notes.md': 'γ\n', 'docs/c.txt': 'TODO later\ndone\nTODO now\n', '.hidden/d.md': 'x\n', 'x.tmp': '', 'y.tmp': '' };
  for (const [name, text] of Object.entries(files)) expect((await request.put(file(base + '/' + name), { headers, data: text || ' ' })).status()).toBe(201);
});
test.afterEach(async ({ request }) => { await request.delete(file(base) + '&recursive=true', { headers }); });

async function terminal(page) {
  await page.goto('/');
  const input = page.getByRole('combobox', { name: 'Terminal command' });
  await run(page, 'su root');
  await page.getByLabel('Password', { exact: true }).fill(password); await page.locator('#auth button[type=submit]').click();
  await expect(page.locator('#auth')).not.toBeVisible();
  await run(page, 'cd ' + base);
  return input;
}
async function run(page, text) {
  const input = page.getByRole('combobox', { name: 'Terminal command' });
  await expect(page.locator('#command').locator('xpath=ancestor::form').locator('.busy')).toHaveText('↵');
  await input.fill(text); await input.press('Enter');
}
const last = page => page.locator('.terminal-panel:not([hidden]) .transcript pre').last();
async function output(page, text) { await run(page, text); await expect(page.locator('#command').locator('xpath=ancestor::form').locator('.busy')).toHaveText('↵'); return (await last(page).textContent()).trim(); }

test('pipes join filters, fd finds what may be listed, and xargs and fd -x run commands on it', async ({ page, request }) => {
  await terminal(page);
  expect(await output(page, 'echo hello world | wc -c')).toBe('12');
  expect(await output(page, 'ls | grep -i A.md')).toBe('a.md');
  expect(await output(page, 'cat docs/b.md a.md | sort -r | head -n 2')).toBe('two\nthree');
  expect(await output(page, 'grep -c TODO docs/c.txt')).toBe('2');
  expect(await output(page, 'fd -e md | sort')).toBe('a.md\ndocs/b.md\ndocs/deep/spéctrum notes.md');
  expect(await output(page, 'fd -H -e md | wc -l')).toBe('4');
  expect(await output(page, 'fd -t d')).toBe('docs\ndocs/deep');
  expect(await output(page, 'fd -e md -X wc -l')).toContain('6 total');
  // Names with spaces and Unicode survive NUL separation.
  expect(await output(page, 'fd -0 -e md | xargs -0 wc -l')).toContain('1 docs/deep/spéctrum notes.md');
  expect(await output(page, 'fd -e md . docs -x echo {/.}')).toMatch(/^(b\nspéctrum notes|spéctrum notes\nb)$/);
  expect(await output(page, 'cat x.md missing.md | wc -l')).toBe('0');
  await expect(page.locator('.terminal-panel:not([hidden]) .transcript')).toContainText('cat: missing.md');
  await run(page, 'ls || pwd'); await expect(last(page)).toContainText('not supported');
  await run(page, 'fd -e tmp | xargs rm');
  await expect.poll(async () => (await (await request.get('/api/ls?path=' + encodeURIComponent(base))).json()).entries.map(e => e.name)).toEqual(['.hidden', 'docs', 'a.md']);
  // xargs with nothing to do still runs once, as GNU xargs does, unless -r.
  await run(page, 'fd -e none | xargs rm'); await expect(last(page)).toContainText('Usage: rm');
  expect(await output(page, 'fd -e none | xargs -r rm')).not.toContain('Usage');
});

test('wildcards expand to sorted paths as bash expands them, and rm -rf * empties a directory', async ({ page, request }) => {
  const names = async path => (await (await request.get('/api/ls?path=' + encodeURIComponent(base + path))).json()).entries.map(e => e.name).sort();
  await terminal(page);
  // Sorted, relative as typed, without hidden names, in every part of a path.
  expect(await output(page, 'echo *')).toBe('a.md docs x.tmp y.tmp');
  expect(await output(page, 'echo *.tmp ?.md')).toBe('x.tmp y.tmp a.md');
  expect(await output(page, 'echo docs/*/*.md')).toBe('docs/deep/spéctrum notes.md');
  expect(await output(page, 'echo */')).toBe('docs/');
  expect(await output(page, 'echo [xy].tmp')).toBe('x.tmp y.tmp');
  expect(await output(page, 'echo .*')).toBe('.hidden');
  expect(await output(page, 'echo ' + base + '/d*')).toBe(base + '/docs');
  // Quoted or escaped wildcards are literal; a pattern that matches nothing stays as typed.
  expect(await output(page, `echo '*' \\*.md "?.md" *.none`)).toBe('* *.md ?.md *.none');
  expect(await output(page, 'cat docs/*.md docs/*.txt | wc -l')).toBe('6');
  await run(page, 'rm *.tmp');
  await expect.poll(() => names('')).toEqual(['.hidden', 'a.md', 'docs']);
  // rm -rf * leaves hidden names, as bash does; in an empty directory it does nothing.
  await run(page, 'cd docs'); await run(page, 'rm -rf *');
  await expect.poll(() => names('/docs')).toEqual([]);
  await run(page, 'rm -rf *');
  await expect(page.locator('#command').locator('xpath=ancestor::form').locator('.busy')).toHaveText('↵');
  await expect(last(page)).toHaveClass(/command-echo/);
  await run(page, 'cd ..'); await run(page, 'rm -r *');
  await expect.poll(() => names('')).toEqual(['.hidden']);
});

test('mv and cp follow Linux defaults, ask with -i, share content, and refuse what rename would', async ({ page, request }) => {
  await terminal(page);
  await run(page, 'cp -r docs copy'); await run(page, 'mv a.md docs/');
  expect(await output(page, 'fd -t f -e md | sort')).toBe('copy/b.md\ncopy/deep/spéctrum notes.md\ndocs/a.md\ndocs/b.md\ndocs/deep/spéctrum notes.md');
  // Removing the original keeps the copy's content.
  await run(page, 'rm -r docs/deep');
  expect(await (await request.get(file(base + '/copy/deep/spéctrum notes.md'))).text()).toBe('γ\n');
  // An existing file is replaced, unless -n; -i asks.
  await run(page, 'cp docs/b.md copy/deep/spéctrum\\ notes.md');
  expect(await (await request.get(file(base + '/copy/deep/spéctrum notes.md'))).text()).toBe('one\ntwo\nthree\n');
  await run(page, 'mv -n docs/a.md copy/b.md');
  expect(await (await request.get(file(base + '/copy/b.md'))).text()).toBe('one\ntwo\nthree\n');
  await run(page, 'mv -i docs/a.md copy/b.md');
  await expect(last(page)).toContainText("overwrite '" + base + "/copy/b.md'? (y/n)");
  await expect(page.locator('.terminal-panel:not([hidden]) .busy')).toHaveText('Answer ↵');
  await page.locator('#command').fill('n'); await page.locator('#command').press('Enter');
  await expect(page.locator('.terminal-panel:not([hidden]) .busy')).toHaveText('↵');
  expect((await request.head(file(base + '/docs/a.md'))).status()).toBe(200);
  await run(page, 'mv -iv docs/a.md copy/b.md'); await expect(page.locator('.terminal-panel:not([hidden]) .busy')).toHaveText('Answer ↵');
  await page.locator('#command').fill('y'); await page.locator('#command').press('Enter');
  await expect(last(page)).toContainText("renamed '" + base + "/docs/a.md' -> '" + base + "/copy/b.md'");
  expect(await (await request.get(file(base + '/copy/b.md'))).text()).toBe('alpha\nbeta\n');
  for (const [line, message] of [['mv docs docs/inner', 'into itself'], ['cp docs docs-flat', '-r not specified'], ['mv . elsewhere', 'current directory'], ['cp -r copy copy/again', 'into itself'], ['mv /home elsewhere', 'home']]) {
    await run(page, line); await expect(last(page)).toContainText(message);
  }
  // Several sources need a directory; each failure is reported while the others proceed.
  await run(page, 'mv docs/b.md copy/deep copy/b.md'); await expect(last(page)).toContainText("target '" + base + "/copy/b.md' is not a directory");
  await run(page, 'rm copy/b.md nothing.md copy/deep');
  await expect(page.locator('.terminal-panel:not([hidden]) .transcript')).toContainText('Removed ' + base + '/copy/b.md');
  await expect(page.locator('.terminal-panel:not([hidden]) .transcript')).toContainText('does not exist');
  await expect(page.locator('.terminal-panel:not([hidden]) .transcript')).toContainText('use rm -r DIR');
  // Batches run each command on its own, in order.
  const batch = await (await request.post('/api/command', { headers, data: { batch: [{ args: ['touch', base + '/n1'] }, { args: ['rm', base + '/missing'] }, { args: ['touch', base + '/n2'] }] } })).json();
  expect(batch.results.map(r => r.status)).toEqual([200, 404, 200]);
  expect((await (await command(request, ['ls', base])).json()).entries.map(e => e.name)).toEqual(expect.arrayContaining(['n1', 'n2']));
});

test('tabs run at once with their own directory and history, and Ctrl+C stops a tab between steps', async ({ page, request }) => {
  for (let i = 0; i < 60; i++) expect((await request.put(file(`${base}/docs/n${i}.dat`), { headers, data: String(i) })).status()).toBe(201);
  const input = await terminal(page);
  await run(page, 'pwd');
  // A second tab starts where the first is, then goes its own way.
  await page.keyboard.press('Alt+t');
  await expect(page.getByRole('tab')).toHaveCount(2);
  await expect(page.locator('#command')).toBeFocused();
  await run(page, 'cd docs');
  await expect(page.locator('.directory-bar')).toContainText(base + '/docs');
  // Tab 2 runs a long xargs; tab 1 works meanwhile.
  await run(page, 'fd -e dat | xargs -t -n 1 cat');
  await expect(page.locator('.terminal-panel:not([hidden]) .transcript')).toContainText('+ cat');
  await page.keyboard.press('Alt+1');
  await expect(page.locator('.directory-bar')).toContainText(base);
  await expect(page.locator('.directory-bar')).not.toContainText('docs');
  expect(await output(page, 'pwd')).toBe(base);
  await input.press('ArrowUp'); await expect(page.locator('#command')).toHaveValue('pwd');
  await page.keyboard.press('Alt+2');
  await page.locator('#command').press('Control+c');
  await expect(page.locator('.terminal-panel:not([hidden]) .transcript')).toContainText('^C');
  await expect(page.locator('.terminal-panel:not([hidden]) .busy')).toHaveText('↵');
  const ran = await page.locator('.terminal-panel:not([hidden]) .transcript pre', { hasText: '+ cat' }).count();
  expect(ran).toBeGreaterThan(0); expect(ran).toBeLessThan(60);
  await page.locator('#command').press('ArrowUp'); await expect(page.locator('#command')).toHaveValue('fd -e dat | xargs -t -n 1 cat');
  // Tabs survive a reload; closing one keeps the other.
  await page.reload();
  await expect(page.getByRole('tab')).toHaveCount(2);
  await page.getByRole('button', { name: 'Close tab 2' }).click();
  await expect(page.getByRole('tab')).toHaveCount(1);
  await expect(page.locator('.directory-bar')).toContainText(base);
});
