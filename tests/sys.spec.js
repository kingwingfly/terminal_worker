import { test, expect, request as playwrightRequest } from '@playwright/test';
import { readFile } from 'node:fs/promises';
import { program, rmrf } from './sys.js';
const origin = process.env.TERMINAL_TEST_URL || 'http://127.0.0.1:8787';
const rootPassword = (await readFile('.dev.vars', 'utf8')).match(/^ADMIN_PASSWORD=(.+)$/m)[1];
let sequence = 0;
const address = name => `sys-${name}-${Date.now()}-${++sequence}`;

// A process of user space: its session, and the calls it makes, a batch a request.
async function session(username, password) {
  const context = await playwrightRequest.newContext({ baseURL: origin, extraHTTPHeaders: { Origin: origin, 'CF-Connecting-IP': address(username || 'guest') } });
  if (username) expect((await context.post('/api/login', { data: { username, password } })).ok()).toBe(true);
  const batch = async (calls, options = {}) => {
    const response = await context.post('/api/sys', { data: { calls, ...options } });
    expect(response.status()).toBe(200);
    return (await response.json()).results;
  };
  // One call: its value, or its errno.
  const sys = async (call, options) => { const [result] = await batch([call], options); return 'ok' in result ? result.ok : result.err.errno; };
  return { context, batch, sys };
}

let root, ann, base;
const name = 'sysann' + Date.now().toString(36), password = 'sys-password-' + Date.now();
test.beforeAll(async () => {
  root = await session('root', rootPassword);
  expect((await program(root.context, ['useradd', '-m', name])).ok()).toBe(true);
  expect((await root.context.post('/api/passwd', { data: { username: name, password } })).ok()).toBe(true);
  ann = await session(name, password);
  base = '/sys-' + Date.now().toString(36);
});
test.afterAll(async () => {
  await rmrf(root.context, base);
  await program(root.context, ['userdel', '-r', name]);
  for (const { context } of [root, ann]) await context.dispose();
});

test('names: each call alone, answered with its value or its errno, as Linux would', async () => {
  const results = await root.batch([
    { mkdir: { path: base, mode: 0o777 } },
    { mkdir: { path: base, mode: 0o777 } },
    { mknod: { path: base + '/file', mode: 0o666 } },
    { stat: { path: base + '/file' } },
    { rmdir: { path: base + '/missing' } },
    { symlink: { target: 'file', path: base + '/link' } },
    { readlink: { path: base + '/link' } },
    { getdents: { path: base } },
  ], { umask: 0o027 });
  expect(results[0]).toEqual({ ok: null });
  // A batch is not a transaction: the second mkdir fails, the rest go on.
  expect(results[1]).toEqual({ err: { errno: 'EEXIST' } });
  const stat = results[3].ok;
  expect([stat.mode, stat.user, stat.size, stat.nlink]).toEqual([0o100640, 'root', 0, 1]);
  expect(stat.mtime).toBeGreaterThan(0);
  expect(results[4]).toEqual({ err: { errno: 'ENOENT' } });
  expect(results[6]).toEqual({ ok: 'file' });
  expect(results[7].ok.map(entry => entry.name).sort()).toEqual(['file', 'link']);
  // The directory is 0750 after the umask 027: others may not look inside.
  expect((await root.sys({ stat: { path: base } })).mode).toBe(0o40750);
  expect(await ann.sys({ getdents: { path: base } })).toBe('EACCES');
  expect(await ann.sys({ stat: { path: base + '/file' } })).toBe('EACCES');
  expect(await root.sys({ chmod: { path: base, mode: 0o1777 } })).toBe(null);
  // Sticky: ann makes her own, and may not take root's away; links followed, `..` physical.
  expect(await ann.sys({ mknod: { path: base + '/hers', mode: 0o644 } })).toBe(null);
  expect((await ann.sys({ stat: { path: base + '/hers' } })).user).toBe(name);
  expect(await ann.sys({ unlink: { path: base + '/file' } })).toBe('EPERM');
  expect(await ann.sys({ rename: { from: base + '/hers', to: base + '/file' } })).toBe('EPERM');
  expect((await ann.sys({ stat: { path: base + '/link/../hers' } }))).toBe('ENOTDIR');
  expect(await root.sys({ rmdir: { path: base } })).toBe('ENOTEMPTY');
  expect(await root.sys({ unlink: { path: base } })).toBe('EISDIR');
  expect(await root.sys({ mkdir: { path: 'relative', mode: 0o777 } })).toBe('EINVAL');
});

test('ACLs are extended attributes, set by the owner, that decide as setfacl\'s do', async () => {
  const dir = base + '-acl';
  expect(await root.sys({ mkdir: { path: dir, mode: 0o755 } })).toBe(null);
  expect(await root.sys({ mknod: { path: dir + '/secret', mode: 0o600 } })).toBe(null);
  expect(await ann.sys({ access: { path: dir + '/secret', mode: 4 } })).toBe('EACCES');
  // No ACL yet beyond its mode.
  expect(await root.sys({ getxattr: { path: dir + '/secret', name: 'system.posix_acl_access' } })).toBe('ENODATA');
  const creds = await ann.sys('getcreds');
  expect([creds.user, creds.signed_in, creds.uid]).toEqual([name, true, creds.uid]);
  const entries = { user: 6, group: 0, other: 0, mask: 4, users: { [creds.uid]: 4 }, groups: {} };
  const value = [...Buffer.from(JSON.stringify(entries))];
  expect(await ann.sys({ setxattr: { path: dir + '/secret', name: 'system.posix_acl_access', value } })).toBe('EPERM');
  expect(await root.sys({ setxattr: { path: dir + '/secret', name: 'system.posix_acl_access', value } })).toBe(null);
  expect(await ann.sys({ access: { path: dir + '/secret', mode: 4 } })).toBe(null);
  expect(await ann.sys({ access: { path: dir + '/secret', mode: 2 } })).toBe('EACCES');
  const stat = await ann.sys({ stat: { path: dir + '/secret' } });
  expect([stat.acl, stat.mode]).toEqual([true, 0o100640]);
  expect(await ann.sys({ listxattr: { path: dir + '/secret' } })).toEqual(['system.posix_acl_access']);
  const read = JSON.parse(Buffer.from(await ann.sys({ getxattr: { path: dir + '/secret', name: 'system.posix_acl_access' } })).toString());
  expect(read.users[creds.uid]).toBe(4);
  // user.* on what may be written; trusted.* root's, unseen by others.
  expect(await ann.sys({ setxattr: { path: dir + '/secret', name: 'user.note', value: [104, 105] } })).toBe('EACCES');
  expect(await root.sys({ setxattr: { path: dir + '/secret', name: 'trusted.mark', value: [49] } })).toBe(null);
  expect(await ann.sys({ getxattr: { path: dir + '/secret', name: 'trusted.mark' } })).toBe('ENODATA');
  await root.batch([{ removexattr: { path: dir + '/secret', name: 'system.posix_acl_access' } }]);
  expect(await ann.sys({ access: { path: dir + '/secret', mode: 4 } })).toBe('EACCES');
  await root.batch([{ unlink: { path: dir + '/secret' } }, { rmdir: { path: dir } }]);
});

test('a guest is uid guest; sudo runs a batch as root only once its account has authenticated', async () => {
  const guest = await session();
  const creds = await guest.sys('getcreds');
  expect([creds.user, creds.signed_in, creds.can_sudo]).toEqual(['guest', false, false]);
  expect(await guest.sys({ mkdir: { path: '/nope-' + Date.now(), mode: 0o777 } })).toBe('EACCES');
  // ann is no wheel member: sudo is refused, whatever the call.
  const refused = await ann.batch([{ stat: { path: '/' } }], { sudo: true });
  expect(refused[0].err.errno).toBe('EPERM');
  // Root's own session is root already.
  expect((await root.sys('getcreds', { sudo: true })).uid).toBe(0);
  await guest.context.dispose();
});

test('a batch has a limit, and what is not a batch is refused', async () => {
  const response = await root.context.post('/api/sys', { data: { calls: Array.from({ length: 257 }, () => 'getcreds') } });
  expect(response.status()).toBe(400);
  const invalid = await root.context.post('/api/sys', { data: { calls: [{ frobnicate: {} }] } });
  expect(invalid.status()).toBe(400);
});
