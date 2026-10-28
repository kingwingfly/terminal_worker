import { test, expect, request as playwrightRequest } from '@playwright/test';
import { readFile } from 'node:fs/promises';
const origin = process.env.TERMINAL_TEST_URL || 'http://127.0.0.1:8787';
const rootPassword = (await readFile('.dev.vars', 'utf8')).match(/^ADMIN_PASSWORD=(.+)$/m)[1];
const file = path => '/api/file?path=' + encodeURIComponent(path);
// Chromium's fake microphone, which plays a tone, granted without asking; and the launch options
// of playwright.config.js, which test.use replaces.
const browserEnv = { ...process.env }; delete browserEnv.DISPLAY;
test.use({ launchOptions: { env: browserEnv, args: ['--ozone-platform=headless', '--use-gl=angle', '--use-angle=swiftshader', '--enable-unsafe-swiftshader', '--use-fake-device-for-media-stream', '--use-fake-ui-for-media-stream'] } });
let sequence = 0;
const address = name => `sound-${name}-${Date.now()}-${++sequence}`;

const busy = page => page.locator('.terminal-panel:not([hidden]) .busy');
const transcript = page => page.locator('.terminal-panel:not([hidden]) .transcript');
async function run(page, text) {
  await expect(busy(page)).toHaveText('↵');
  const input = page.getByRole('combobox', { name: 'Terminal command' });
  await input.fill(text); await input.press('Enter');
}
async function device(browser, username, password) {
  const context = await browser.newContext({ permissions: ['microphone'], extraHTTPHeaders: { 'CF-Connecting-IP': address(username || 'guest') } });
  // The microphones a page takes, to end them as if unplugged.
  await context.addInitScript(() => {
    const take = navigator.mediaDevices.getUserMedia.bind(navigator.mediaDevices);
    navigator.mediaDevices.getUserMedia = async constraints => { const stream = await take(constraints); (window.microphones ??= []).push(stream); return stream; };
  });
  if (username) expect((await context.request.post('/api/login', { headers: { Origin: origin }, data: { username, password } })).ok()).toBe(true);
  const page = await context.newPage(); await page.goto(origin + '/');
  await expect(page.locator('.lock')).toContainText(username || 'guest');
  return { context, page };
}

let root, name, password, home;
test.beforeAll(async () => {
  root = await playwrightRequest.newContext({ baseURL: origin, extraHTTPHeaders: { Origin: origin, 'CF-Connecting-IP': address('api') } });
  expect((await root.post('/api/login', { data: { password: rootPassword } })).ok()).toBe(true);
  name = 'voice' + Date.now().toString(36); password = 'sound-password-' + Date.now();
  expect((await root.post('/api/command', { data: { args: ['useradd', '-m', name], cwd: '/' } })).ok()).toBe(true);
  expect((await root.post('/api/passwd', { data: { username: name, password } })).ok()).toBe(true);
  home = '/home/' + name;
});
test.afterAll(async () => { await root.post('/api/command', { data: { args: ['userdel', '-r', name], cwd: '/' } }); await root.dispose(); });

test('stream records the microphone into a file that grows as it is heard, and is whole once stopped', async ({ browser }) => {
  const speaker = await device(browser, name, password);
  const page = speaker.page, recording = home + '/talk.weba';
  try {
    await run(page, 'cd ~');
    // Browsers record Opus in WebM, so a recording is a .weba.
    await run(page, 'stream talk.aac');
    await expect(transcript(page)).toContainText('browsers record Opus in WebM; name the recording NAME.weba');
    await run(page, 'stream talk.weba');
    await expect(transcript(page)).toContainText(/● \d\d:\d\d/);
    // The file is there at once, a recording, and grows, a chunk at a time.
    const stat = async () => (await root.get('/api/stat?path=' + encodeURIComponent(recording))).json();
    await expect.poll(async () => (await stat()).recording, { timeout: 15_000 }).toBe(true);
    await expect.poll(async () => (await stat()).size, { timeout: 30_000, intervals: [1000] }).toBeGreaterThan(0);
    const grown = (await stat()).size;
    // jobs, in another tab, says it is recording, and how much it has.
    const other = await speaker.context.newPage(); await other.goto(origin + '/');
    await run(other, 'jobs');
    await expect(transcript(other)).toContainText(new RegExp(`recording  ${recording}  [\\d.]+ KB`));
    await other.close();
    // An upload that is not a recording says how big it will be.
    expect((await root.post('/api/uploads?path=' + encodeURIComponent(home + '/sizeless.bin'), { data: {} })).status()).toBe(400);
    // What arrived may be read meanwhile, from its start.
    const partial = await (await root.get(file(recording), { headers: { 'Accept-Encoding': 'identity' } })).body();
    expect(partial.length).toBeGreaterThanOrEqual(grown);
    expect(partial.subarray(0, 4).toString('hex')).toBe('1a45dfa3');
    // Ctrl+C stops it; the rest arrives, and it is whole.
    await page.waitForTimeout(2000);
    await page.locator('#command').press('Control+c');
    await expect(transcript(page)).toContainText(new RegExp(`✓ ${recording} · \\d\\d:\\d\\d · [\\d.]+ KB`), { timeout: 30_000 });
    const whole = await stat();
    expect(whole.recording).toBeFalsy(); expect(whole.uploaded).toBeUndefined();
    expect(whole.size).toBeGreaterThan(grown);
    const bytes = await (await root.get(file(recording), { headers: { 'Accept-Encoding': 'identity' } })).body();
    expect(bytes.length).toBe(whole.size);
    expect(bytes.subarray(0, 4).toString('hex')).toBe('1a45dfa3');
    // It is a new file: another recording does not replace it.
    await run(page, 'stream talk.weba');
    await expect(transcript(page)).toContainText('File already exists');
  } finally {
    await root.delete(file(recording));
    await speaker.context.close();
  }
});

test('a recording whose microphone stops on its own is saved, with what it heard', async ({ browser }) => {
  const speaker = await device(browser, name, password);
  const page = speaker.page, recording = home + '/unplugged.weba';
  try {
    await run(page, 'cd ~');
    await run(page, 'stream unplugged.weba');
    await expect(transcript(page)).toContainText(/● 00:0[3-9]/, { timeout: 15_000 });
    // Unplugged, or taken back by the browser: every track of the microphone ends.
    await page.evaluate(() => window.microphones.forEach(stream => stream.getTracks().forEach(track => track.stop())));
    await expect(transcript(page)).toContainText(new RegExp(`✓ ${recording} · \\d\\d:\\d\\d · [\\d.]+ KB.* · the microphone stopped`), { timeout: 30_000 });
    const whole = await (await root.get('/api/stat?path=' + encodeURIComponent(recording))).json();
    expect(whole.recording).toBeFalsy(); expect(whole.size).toBeGreaterThan(0);
    await expect(busy(page)).toHaveText('↵');
  } finally {
    await root.delete(file(recording));
    await speaker.context.close();
  }
});

test('live sound: /dev/snd/NAME is heard by whoever may read the recording, and with the presentation of its name', async ({ browser }) => {
  const live = await root.post('/api/sound/listen?path=' + encodeURIComponent('/dev/snd/nothing'));
  test.skip(live.status() === 503, 'The site has no Cloudflare Realtime app (REALTIME_APP_ID, REALTIME_APP_TOKEN)');
  const speaker = await device(browser, name, password), guest = await device(browser);
  const remote = await speaker.context.newPage(); await remote.goto(origin + '/');
  const recording = home + '/lecture.weba', deck = home + '/lecture.slides.md';
  try {
    expect((await root.put(file(deck), { data: '# One\n\n---\n\n# Two\n' })).status()).toBe(201);
    for (const args of [['chmod', '755', home], ['chmod', '644', deck]]) expect((await root.post('/api/command', { data: { args, cwd: '/' } })).ok()).toBe(true);
    await run(speaker.page, 'cd ~');
    await run(speaker.page, 'stream lecture.weba');
    await expect(transcript(speaker.page)).toContainText('live as /dev/snd/lecture', { timeout: 20_000 });
    // A guest hears it, as the recording, made with the umask, may be read by others.
    await run(guest.page, 'open /dev/snd/lecture');
    const player = guest.page.getByRole('region', { name: 'Live sound' });
    await expect(player).toBeVisible(); await expect(player).toHaveAttribute('data-state', /live|paused/, { timeout: 20_000 });
    await expect.poll(() => guest.page.evaluate(() => document.getElementById('listening-audio').srcObject?.getAudioTracks()[0]?.readyState), { timeout: 20_000 }).toBe('live');
    // A presentation of the same name brings its sound with it.
    await player.getByRole('button', { name: 'Stop listening' }).click();
    await expect(player).toBeHidden();
    await run(remote, `present -n lecture ${deck}`);
    await expect(remote.locator('.slides-current .slide:visible h1')).toHaveText('One');
    await run(guest.page, 'open /dev/present/lecture');
    await expect(guest.page.locator('.slides-current .slide:visible h1')).toHaveText('One');
    await expect(guest.page.getByRole('region', { name: 'Live sound' })).toBeVisible();
    // Stopped, the sound ends for its listeners.
    await speaker.page.locator('#command').press('Control+c');
    await expect(transcript(speaker.page)).toContainText(`✓ ${recording}`, { timeout: 30_000 });
    await expect(guest.page.getByRole('region', { name: 'Live sound' })).toBeHidden({ timeout: 10_000 });
  } finally {
    await root.delete(file(recording)); await root.delete(file(deck));
    await root.delete('/api/present?name=lecture');
    await speaker.context.close(); await guest.context.close();
  }
});
