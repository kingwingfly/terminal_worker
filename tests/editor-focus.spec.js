import { test, expect } from '@playwright/test';

// Keep this regression independent of local accounts, passwords and stored files.
test('editor close restores actual typing after Escape, close button and discarded changes', async ({ page }) => {
  let content = 'original text', revision = 1;
  const entry = () => ({name:'focus.txt',path:'/focus.txt',directory:false,kind:'text',size:content.length,mode:420,owner:'editor',group:'editor',readable:true,writable:true});
  await page.route('**/api/**', async route => {
    const request = route.request(), url = new URL(request.url());
    if (url.pathname === '/api/session') return route.fulfill({json:{authenticated:true,username:'editor',uid:1000,home:'/',can_write:true}});
    if (url.pathname === '/api/ls') return route.fulfill({json:{path:'/',entries:[entry()]}});
    if (url.pathname === '/api/stat') return route.fulfill({json:entry()});
    if (url.pathname === '/api/file') {
      if (request.method() === 'PUT') { content = request.postData(); revision++; return route.fulfill({json:{etag:`"${revision}"`}}); }
      return route.fulfill({body:request.method() === 'HEAD' ? '' : content,headers:{'Content-Type':'text/plain','ETag':`"${revision}"`}});
    }
    return route.fulfill({status:404,json:{error:'unexpected test request'}});
  });
  await page.goto('/');
  const input = page.locator('#command');
  const run = async command => { await expect(page.locator('.busy')).toHaveText('↵');await input.fill(command);await input.press('Enter'); };
  for (const close of ['escape', 'button', 'discard']) {
    await run('vim /focus.txt');
    const editor = page.getByRole('textbox',{name:'File contents'});
    await expect(editor).toBeVisible();
    await editor.fill('edited with '+close);
    if (close === 'discard') {
      await page.keyboard.press('Escape');
      await expect(page.locator('#editor')).toBeVisible();
      await expect(page.locator('#editor-discard')).toBeVisible();
      await expect(input).not.toBeFocused();
      await page.locator('#editor-discard').click();
    } else {
      await page.keyboard.press('Control+s');
      await expect(page.locator('#editor-status')).toHaveText('Saved');
      if (close === 'escape') await page.keyboard.press('Escape');
      else await page.getByRole('button',{name:'Close editor'}).click();
    }
    await expect(page.locator('#editor')).not.toBeVisible();
    await expect(input).toBeFocused();
    // focus() alone can pass while the browser still refuses native text input.
    // Deliberately avoid fill(), click() or Tab after closing the editor.
    await page.keyboard.type('pwd');
    await expect(input).toHaveValue('pwd');
    await page.keyboard.press('Enter');
    await expect(page.locator('.transcript pre').last()).toHaveText('/');
  }
});
