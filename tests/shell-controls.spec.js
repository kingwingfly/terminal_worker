import { test, expect, request } from '@playwright/test';
import { readFile } from 'node:fs/promises';
const origin = process.env.TERMINAL_TEST_URL || 'http://127.0.0.1:8788';
const rootPassword = (await readFile('.dev.vars','utf8')).match(/^ADMIN_PASSWORD=(.+)$/m)[1];
const password = 'shell-testing-password-621';
const file = path => '/api/file?path=' + encodeURIComponent(path);
const command = (client,args,extra={}) => client.post('/api/command',{data:{args,cwd:'/',...extra}});
async function ok(response,status=200) { expect(response.status(),await response.text()).toBe(status); return response; }
async function fixture() {
 const make=()=>request.newContext({baseURL:origin,extraHTTPHeaders:{Origin:origin,'CF-Connecting-IP':'shell-'+Date.now()+'-'+Math.random()}});
 const root=await make(), user=await make(), guest=await make();
 await ok(await root.post('/api/login',{data:{password:rootPassword}}));
 const name='shell'+Date.now().toString(36), home='/home/'+name;
 await ok(await command(root,['useradd','-m','-G','wheel',name]));
 await ok(await root.post('/api/passwd',{data:{username:name,password}}));
 await ok(await user.post('/api/login',{data:{username:name,password}}));
 return {root,user,guest,name,home,async cleanup(){
  await command(root,['userdel','-r',name]);await command(root,['groupdel',name]);
  for(const client of [root,user,guest])await client.dispose();
 }};
}
const run=async(page,text)=>{await expect(page.locator('.busy')).toHaveText('↵');await page.locator('#command').fill(text);await page.locator('#command').press('Enter');};

test('su restores typing focus; Ctrl+C clears drafts, completion and history; forced password dialog accepts short passwords',async({page})=>{
 const f=await fixture();try {
  await page.setExtraHTTPHeaders({'CF-Connecting-IP':'shell-ui-'+Date.now()});await page.goto('/');
  await run(page,'su - '+f.name);await page.locator('#password').fill(password);await page.locator('#password').press('Enter');
  await expect(page.locator('.lock')).toContainText(f.name);await expect(page.locator('.busy')).toHaveText('↵');await expect(page.locator('#command')).toBeFocused();
  await page.keyboard.type('pwd');await page.keyboard.press('Enter');await expect(page.locator('.transcript pre').last()).toHaveText(f.home);
  await page.keyboard.type('mkdir discard-me');await page.keyboard.press('Control+c');await expect(page.locator('#command')).toHaveValue('');
  await page.keyboard.press('ArrowUp');await expect(page.locator('#command')).toHaveValue('pwd');
  await page.keyboard.press('Control+c');await page.keyboard.press('ArrowDown');await expect(page.locator('#command')).toHaveValue('');
  await page.keyboard.type('su');await page.keyboard.press('Tab');await expect(page.locator('#completions')).toBeVisible();
  await page.keyboard.press('Control+c');await expect(page.locator('#command')).toHaveValue('');await expect(page.locator('#completions')).toHaveCount(0);
  expect((await page.request.get('/api/stat?path='+encodeURIComponent(f.home+'/discard-me'))).status()).toBe(404);
  await run(page,'passwd --force');await expect(page.locator('#auth-description')).toContainText('--force');
  await page.locator('#current-password').fill(password);await page.locator('#password').fill('1234');await page.locator('#confirm-password').fill('1234');await page.locator('#auth button[type=submit]').click();
  await expect(page.locator('#auth')).not.toBeVisible();await expect(page.locator('.busy')).toHaveText('↵');await expect(page.locator('#command')).toBeFocused();
  await page.keyboard.type('whoami');await page.keyboard.press('Enter');await expect(page.locator('.transcript pre').last()).toHaveText(f.name);
  await run(page,'exit');await expect(page.locator('.lock')).toContainText('guest');await run(page,'su '+f.name);
  await page.locator('#password').fill('1234');await page.locator('#password').press('Enter');await expect(page.locator('.lock')).toContainText(f.name);
  await expect(page.locator('.busy')).toHaveText('↵');await expect(page.locator('#command')).toBeFocused();
  await run(page,'su root');await page.locator('#cancel-auth').click();await expect(page.locator('.busy')).toHaveText('↵');await expect(page.locator('#command')).toBeFocused();
 }finally{await f.cleanup();}
});

test('force skips only the minimum password length, preserving identity checks and session revocation',async()=>{
 const f=await fixture();try {
  expect((await f.user.post('/api/passwd',{data:{password:'1',current_password:password}})).status()).toBe(400);
  expect((await f.guest.post('/api/passwd',{data:{username:f.name,password:'1',force:true}})).status()).toBe(401);
  expect((await f.user.post('/api/passwd',{data:{username:'root',password:'1',force:true}})).status()).toBe(403);
  expect((await f.user.post('/api/passwd',{data:{password:'1',current_password:'wrong',force:true}})).status()).toBe(401);
  for(const value of ['', 'x'.repeat(513)])expect((await f.user.post('/api/passwd',{data:{password:value,current_password:password,force:true}})).status()).toBe(400);
  const cookie=(await f.user.storageState()).cookies.find(c=>c.name==='terminal_session').value;
  await ok(await f.user.post('/api/passwd',{data:{password:'1',current_password:password,force:true}}));
  expect((await(await f.guest.get('/api/session',{headers:{Cookie:'terminal_session='+cookie}})).json()).username).toBe('guest');
  await ok(await f.guest.post('/api/login',{data:{username:f.name,password:'1'}}));
  expect((await(await f.user.get('/api/session')).json()).username).toBe(f.name);
 }finally{await f.cleanup();}
});

test('touch creates readable empty files, preserves content and ACLs, validates batches and supports sudo',async()=>{
 const f=await fixture();try {
  const path=f.home+'/notes.txt';
  await ok(await command(f.user,['touch','~/notes.txt','~/README','~/notes.txt']));
  const empty=await ok(await f.user.get(file(path)));expect(await empty.text()).toBe('');
  expect((await f.user.head(file(path))).headers()['content-length']).toBe('0');
  const stat=await(await f.user.get('/api/stat?path='+encodeURIComponent(path))).json();expect(stat.owner).toBe(f.name);expect(stat.mode).toBe(0o644);
  await ok(await f.user.put(file(path)+'&overwrite=true',{data:'preserve this'}));
  await ok(await command(f.user,['chmod','600',path]));
  const before=await(await f.user.get(file(path))).text();const etag=(await f.user.head(file(path))).headers().etag;
  await ok(await command(f.user,['touch',path]));expect(await(await f.user.get(file(path))).text()).toBe(before);expect((await f.user.head(file(path))).headers().etag).toBe(etag);
  expect((await command(f.user,['touch',f.home+'/atomic.txt','/root/blocked.txt'])).status()).toBe(403);
  expect((await f.user.get('/api/stat?path='+encodeURIComponent(f.home+'/atomic.txt'))).status()).toBe(404);
  expect((await command(f.guest,['touch','/denied.txt'])).status()).toBe(401);
  await ok(await command(f.user,['touch','-c',f.home+'/skip.txt']));expect((await f.user.get(file(f.home+'/skip.txt'))).status()).toBe(404);
  await ok(await command(f.user,['touch','--','-h'],{cwd:f.home}));await ok(await f.user.get(file(f.home+'/-h')));
  await ok(await command(f.user,['chmod','400',path]));expect((await command(f.user,['touch',path])).status()).toBe(403);
  expect((await command(f.user,['touch',f.home])).status()).toBe(409);
  await ok(await f.user.post('/api/sudo',{data:{password}}));await ok(await command(f.user,['touch',f.home+'/root-owned.txt'],{sudo:true}));
  expect((await(await f.user.get('/api/stat?path='+encodeURIComponent(f.home+'/root-owned.txt'))).json()).owner).toBe('root');
  expect((await(await f.user.get('/api/session')).json()).username).toBe(f.name);
 }finally{await f.cleanup();}
});

test('touch integrates with completion, help, preview and editing extensionless files',async({page})=>{
 const f=await fixture();try {
  await page.setExtraHTTPHeaders({'CF-Connecting-IP':'touch-ui-'+Date.now()});await page.goto('/');
  await run(page,'su - '+f.name);await page.locator('#password').fill(password);await page.locator('#auth button[type=submit]').click();await expect(page.locator('.lock')).toContainText(f.name);
  await page.locator('#command').fill('touc');await page.keyboard.press('Tab');await expect(page.locator('#command')).toHaveValue('touch ');
  await run(page,'touch README notes.md');await expect(page.locator('.transcript pre').last()).toContainText('Created');
  await expect(page.locator('.filename').filter({hasText:'README'})).toBeVisible();
  await run(page,'vim README');await expect(page.locator('.cm-content')).toBeVisible();await page.locator('.cm-content').fill('hello terminal');await page.keyboard.press('Control+s');await expect(page.locator('#editor-status')).toHaveText('Saved');await page.keyboard.press('Escape');
  await run(page,'touch README');await expect(page.locator('.transcript pre').last()).toContainText('content unchanged');
  await run(page,'open README');await expect(page.locator('.text-preview')).toHaveText('hello terminal');await page.getByRole('button',{name:'Close preview'}).click();
  await run(page,'touch -h');await expect(page.locator('.transcript pre').last()).toContainText('touch [-c] [--] FILE...');
 }finally{await f.cleanup();}
});
