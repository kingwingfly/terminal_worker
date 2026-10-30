import { test, expect } from '@playwright/test';

// Processes of /usr/bin (web/wasi/exec.mjs): a pipeline's run at once, through pipes in memory the
// page shares with them, which it may only when isolated from other sites (web/_headers); the
// first reads the terminal, as typed; head closes its input once it has its lines, and what
// writes to it hears SIGPIPE. Where the page is not isolated, they run one after another.
const input = page => page.getByRole('combobox', { name: 'Terminal command' });
const busy = page => page.locator('#command').locator('xpath=ancestor::form').locator('.busy');
const last = page => page.locator('.transcript pre').last();
async function run(page, text, { wait = true } = {}) {
  await expect(busy(page)).toHaveText('↵');
  await input(page).fill(text); await input(page).press('Enter');
  if (wait) await expect(busy(page)).toHaveText('↵', { timeout: 30_000 });
}
async function ready(page) {
  await page.goto('/');
  await expect(busy(page)).toHaveText('↵');
}

test('a pipeline of processes runs at once through pipes, and head stops what it reads', async ({ page }) => {
  await ready(page);
  expect(await page.evaluate(() => crossOriginIsolated)).toBe(true);
  // yes would write forever: head takes three lines, and closes the pipe.
  const started = Date.now();
  await run(page, 'yes | head -n 3');
  await expect(last(page)).toHaveText('y\ny\ny');
  expect(Date.now() - started).toBeLessThan(10_000);
  // Through two pipes; and SIGPIPE ends yes as coreutils' head exits.
  await run(page, 'seq 1 3 | tr 1-3 a-c | fold -w 1');
  await expect(last(page)).toHaveText('a\nb\nc');
  await run(page, 'yes | coreutils head -n 2');
  await expect(last(page)).toHaveText('y\ny');
  await expect(page.locator('.transcript')).not.toContainText('Broken pipe');
  // Errors show as they are written, a line each.
  await run(page, 'coreutils cat /nothing-here');
  await expect(last(page)).toHaveText("! cat: /nothing-here: No such file or directory");
});

test('the first process reads the terminal: a line as typed, Ctrl+D ends a read, Ctrl+C kills it', async ({ page }) => {
  await ready(page);
  // tr waits on the terminal; what it writes for each line shows while it runs.
  await run(page, 'tr a-z A-Z', { wait: false });
  await expect(busy(page)).toHaveText('Input ↵');
  await input(page).fill('hello there'); await input(page).press('Enter');
  await expect(last(page)).toHaveText('HELLO THERE');
  await expect(page.locator('.transcript pre').nth(-2)).toHaveText('hello there');
  await expect(busy(page)).toHaveText('Input ↵');
  // Typed lines are the program's, not the shell's history.
  await input(page).press('Control+d');
  await expect(busy(page)).toHaveText('↵');
  await input(page).press('ArrowUp');
  await expect(input(page)).toHaveValue('tr a-z A-Z');
  await input(page).fill('');
  // Ctrl+D after text gives the text, without a newline; on nothing, it ends the read.
  await run(page, 'coreutils wc -c', { wait: false });
  await expect(busy(page)).toHaveText('Input ↵');
  await input(page).fill('abc'); await input(page).press('Control+d');
  await expect(busy(page)).toHaveText('Input ↵');
  await input(page).press('Control+d');
  await expect(busy(page)).toHaveText('↵');
  await expect(last(page)).toHaveText('3');
  // Ctrl+C kills what reads.
  await run(page, 'coreutils cat', { wait: false });
  await expect(busy(page)).toHaveText('Input ↵');
  await input(page).press('Control+c');
  await expect(busy(page)).toHaveText('↵', { timeout: 5_000 });
  // What reads its input from a pipe does not read the terminal.
  await run(page, 'seq 2 | tr 12 xy');
  await expect(last(page)).toHaveText('x\ny');
});

test('a page not isolated runs a pipeline one process after another, and nothing reads the terminal', async ({ page }) => {
  await page.route(url => new URL(url).pathname === '/', async route => {
    const response = await route.fetch();
    const headers = { ...response.headers() };
    delete headers['cross-origin-embedder-policy']; delete headers['cross-origin-opener-policy'];
    await route.fulfill({ response, headers });
  });
  await ready(page);
  expect(await page.evaluate(() => crossOriginIsolated)).toBe(false);
  await run(page, 'seq 1 3 | tr 1-3 a-c | fold -w 1');
  await expect(last(page)).toHaveText('a\nb\nc');
  await run(page, 'yes | head -n 2');
  await expect(last(page)).toHaveText('y\ny');
  await run(page, 'tr a-z A-Z');
  await expect(last(page)).toHaveText(/tr a-z A-Z$/);
});
