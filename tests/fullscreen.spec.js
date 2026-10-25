import { test, expect } from '@playwright/test';
async function open(page, path) {
  await page.goto('/');
  const input = page.getByRole('combobox', { name: 'Terminal command' });
  await input.fill('open ' + path); await input.press('Enter');
  await expect(page.locator('#viewer')).toBeVisible();
}
async function enter(page) {
  await page.getByRole('button', { name: 'Fullscreen', exact: true }).click();
  await expect.poll(() => page.evaluate(() => document.fullscreenElement?.id)).toBe('viewer-surface');
  await expect(page.getByRole('button', { name: 'Exit fullscreen', exact: true })).toBeVisible();
  await expect(page.locator('#fullscreen-error')).toBeHidden();
  await expect.poll(() => page.locator('#viewer-surface').evaluate(el => {
    const box = el.getBoundingClientRect();
    return Math.abs(box.width-innerWidth) + Math.abs(box.height-innerHeight) + Math.abs(box.x) + Math.abs(box.y);
  })).toBeLessThan(3);
}

test('preview enters fullscreen, toggles out, and closes cleanly while fullscreen', async ({ page }) => {
  await open(page, 'welcome.md');
  await expect(page.locator('.markdown h1')).toBeVisible();
  await enter(page);
  await page.getByRole('button', { name: 'Exit fullscreen', exact: true }).click();
  await expect.poll(() => page.evaluate(() => document.fullscreenElement === null)).toBe(true);
  await expect(page.locator('#viewer')).toBeVisible();
  await enter(page);
  // A browser-initiated exit (such as Escape) must update the toolbar too.
  await page.evaluate(() => document.exitFullscreen());
  await expect(page.getByRole('button', { name: 'Fullscreen', exact: true })).toBeVisible();
  await enter(page);
  await page.getByRole('button', { name: 'Close preview' }).click();
  await expect(page.locator('#viewer')).toBeHidden();
  await expect.poll(() => page.evaluate(() => document.fullscreenElement === null)).toBe(true);
  await expect(page.getByRole('combobox', { name: 'Terminal command' })).toBeFocused();
});

test('slides remain interactive inside the fullscreen surface', async ({ page }) => {
  await open(page, 'demo.slides.md');
  await expect(page.locator('.slides .slide')).toBeVisible();
  const scale = () => page.locator('.slides').evaluate(el => Number(getComputedStyle(el).getPropertyValue('--scale')));
  const windowed = await scale();
  await enter(page);
  // Full screen, the slides scale up to the space they have.
  await expect.poll(scale).toBeGreaterThan(windowed);
  await page.keyboard.press('ArrowRight');
  await expect(page.locator('.slides .slide')).toContainText('One filesystem');
  await page.getByRole('button', { name: 'Close preview' }).click();
  await expect.poll(() => page.evaluate(() => document.fullscreenElement === null)).toBe(true);
});

test('a rejected request explains the failure within the modal', async ({ page }) => {
  await open(page, 'welcome.md');
  await page.evaluate(() => {
    document.getElementById('viewer-surface').requestFullscreen = () => Promise.reject(new DOMException('denied by test policy', 'NotAllowedError'));
  });
  await page.getByRole('button', { name: 'Fullscreen', exact: true }).click();
  await expect(page.locator('#fullscreen-error')).toBeVisible();
  await expect(page.locator('#fullscreen-error')).toContainText('denied by test policy');
  await expect(page.locator('#fullscreen-error')).not.toContainText('This browser does not support');
  await expect(page.locator('#viewer')).toBeVisible();
});
