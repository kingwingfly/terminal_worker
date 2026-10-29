// System calls from tests, as user space makes them (owner's design, 2026-10-29): what the tests
// need to make, find and clean up files, now that the kernel's API is its system calls.
const origin = process.env.TERMINAL_TEST_URL || 'http://127.0.0.1:8787';
const headers = { Origin: origin, 'Content-Type': 'application/json' };

// `calls`, one batch, for the session of `request` (an APIRequestContext): each one's result,
// `{ok}` or `{err: {errno, detail}}`.
export async function sys(request, calls, options = {}) {
  const response = await request.post(origin + '/api/sys', { headers, data: { calls, ...options } });
  if (!response.ok()) throw new Error(`/api/sys: ${response.status()} ${await response.text()}`);
  return (await response.json()).results;
}

// One call: its errno, or null if it was done.
export async function errno(request, call, options) {
  const [result] = await sys(request, [call], options);
  return 'err' in result ? result.err.errno : null;
}

// One call: its value; failed, an error that says its errno.
export async function call(request, call, options) {
  const [result] = await sys(request, [call], options);
  if ('ok' in result) return result.ok;
  const error = new Error(`${Object.keys(call)[0]}: ${result.err.detail || result.err.errno}`);
  error.errno = result.err.errno;
  throw error;
}

// mkdir -p: `path` and each directory above it that is not there.
export async function mkdirp(request, path, mode = 0o777) {
  const parts = path.split('/').filter(Boolean);
  const results = await sys(request, parts.map((_, i) => ({ mkdir: { path: '/' + parts.slice(0, i + 1).join('/'), mode } })));
  const failed = results.findLast(result => result.err && result.err.errno !== 'EEXIST');
  if (failed) { const error = new Error(`mkdir -p ${path}: ${failed.err.detail || failed.err.errno}`); error.errno = failed.err.errno; throw error; }
}

// rm -rf: `path` and what is below it, as user space removes a tree; its first failure's errno,
// or null. One not there is fine.
export async function rmrf(request, path) {
  const [found] = await sys(request, [{ lstat: { path } }]);
  if (found.err) return found.err.errno === 'ENOENT' ? null : found.err.errno;
  if ((found.ok.mode & 0o170000) !== 0o040000) return errno(request, { unlink: { path } });
  const [listed] = await sys(request, [{ getdents: { path } }]);
  if (listed.err) return listed.err.errno;
  let failure = null;
  for (const entry of listed.ok) failure ??= await rmrf(request, path.replace(/\/$/, '') + '/' + entry.name);
  return failure ?? errno(request, { rmdir: { path } });
}

// The names in the directory `path`, as getdents gives them.
export async function names(request, path) {
  return (await call(request, { getdents: { path } })).map(entry => entry.name);
}

// What stat says of `path`, as the tests look at a node: whether it is a directory, its owner's
// and group's names, its permission bits, size, link, and whether it is being recorded; null if
// it is not there.
export async function entry(request, path) {
  const [found] = await sys(request, [{ stat: { path } }]);
  if (found.err?.errno === 'ENOENT') return null;
  if (found.err) { const error = new Error(`stat ${path}: ${found.err.errno}`); error.errno = found.err.errno; throw error; }
  const stat = found.ok;
  return { directory: (stat.mode & 0o170000) === 0o040000, owner: stat.user, group: stat.group, mode: stat.mode & 0o7777,
    size: stat.size, link: stat.link, recording: !!stat.writing?.recording, uploaded: stat.writing?.received };
}

// The entries of the directory `path`, as the terminal lists them: directories first, then by
// name, each as `entry` describes a node (and its name).
export async function listing(request, path) {
  const entries = (await call(request, { getdents: { path } })).map(({ name, stat }) => ({ name,
    directory: !!stat && (stat.mode & 0o170000) === 0o040000, owner: stat?.user, group: stat?.group, mode: stat ? stat.mode & 0o7777 : 0,
    size: stat?.size, link: stat?.link, uploaded: stat?.writing?.received }));
  return entries.sort((a, b) => (b.directory - a.directory) || (a.name < b.name ? -1 : a.name > b.name ? 1 : 0));
}

// The programs of user space the tests run as an account, for what they set up and check:
// chmod, chown, chgrp, touch, rm, mkdir, mv, ln -s and setfacl -m, made of system calls as the
// terminal's are (crates/userland), answered as a request is (200; 403 refused, 401 to a guest;
// 404 not there; 409 there already); anything else goes to the commands the server still runs.
const MOVED = new Set(['chmod', 'chown', 'chgrp', 'touch', 'rm', 'mkdir', 'mv', 'ln', 'setfacl']);
const readText = async (request, path) => { const response = await request.get(origin + '/api/file?path=' + encodeURIComponent(path)); return response.ok() ? response.text() : ''; };
async function idsOf(request, path) {
  return new Map((await readText(request, path)).split('\n').filter(Boolean).map(line => line.split(':')).map(fields => [fields[0], fields]));
}
// The bits a symbolic mode (`go-w`, `u+x,o=r`) or an octal one leaves of `mode`.
function changed(mode, spec, directory) {
  if (/^[0-7]{3,4}$/.test(spec)) return parseInt(spec, 8);
  for (const clause of spec.split(',')) {
    const [, who = 'a', op, perms] = clause.match(/^([ugoa]*)([+=-])([rwxXst]*)$/) || [];
    if (!op) throw new Error(`invalid mode: ${spec}`);
    const classes = (who === 'a' || who === '') ? 'ugo' : who.replace('a', 'ugo');
    let bits = 0;
    for (const c of classes) {
      const shift = { u: 6, g: 3, o: 0 }[c];
      let p = 0;
      for (const ch of perms) p |= { r: 4, w: 2, x: 1, X: directory || mode & 0o111 ? 1 : 0, s: 0, t: 0 }[ch];
      bits |= p << shift;
      if (op === '=') mode &= ~(7 << shift);
    }
    if (perms.includes('s')) bits |= (classes.includes('u') ? 0o4000 : 0) | (classes.includes('g') ? 0o2000 : 0);
    if (perms.includes('t')) bits |= 0o1000;
    mode = op === '-' ? mode & ~bits : mode | bits;
  }
  return mode;
}
// Each node below the directory `path`, and it, deepest last.
async function tree(request, path, options) {
  const [found] = await sys(request, [{ lstat: { path } }], options);
  if (found.err || (found.ok.mode & 0o170000) !== 0o040000) return [path];
  const [listed] = await sys(request, [{ getdents: { path } }], options);
  const below = [];
  for (const entry of listed.ok ?? []) below.push(...await tree(request, path.replace(/\/$/, '') + '/' + entry.name, options));
  return [path, ...below];
}
export async function program(request, args, { cwd = '/', sudo = false, ...extra } = {}) {
  if (!MOVED.has(args[0])) return request.post(origin + '/api/command', { headers: { ...headers, ...(extra.headers || {}) }, data: { args, cwd, sudo } });
  const options = { sudo };
  const creds = await call(request, 'getcreds', options);
  const absolute = p => p === '~' ? creds.home : p.startsWith('~/') ? creds.home + p.slice(1) : p.startsWith('/') ? p : (cwd === '/' ? '' : cwd) + '/' + p;
  const [name, ...rest] = args;
  const flags = new Set(), operands = [];
  for (let i = 0; i < rest.length; i++) {
    if (rest[i] === '--') { operands.push(...rest.slice(i + 1)); break; }
    if (rest[i].startsWith('-') && rest[i].length > 1 && !(name === 'chmod' && /^-[rwxXst]+$/.test(rest[i]))) {
      for (const flag of rest[i].slice(1)) flags.add(flag);
      if (name === 'setfacl' && rest[i] === '-m') { flags.add('m'); operands.push(rest[++i]); }
    } else operands.push(rest[i]);
  }
  let failure = null;
  const run = async c => { const e = await errno(request, c, options); failure ??= e; return e; };
  switch (name) {
    case 'chmod': case 'chown': case 'chgrp': {
      const [spec, ...paths] = operands;
      let uid, gid;
      if (name !== 'chmod') {
        const users = await idsOf(request, '/etc/passwd'), groups = await idsOf(request, '/etc/group');
        const [user, group] = name === 'chgrp' ? ['', spec] : spec.split(':');
        uid = user ? Number(users.get(user)?.[2] ?? user) : undefined;
        gid = group ? Number(groups.get(group)?.[2] ?? group) : group === '' && user ? Number(users.get(user)?.[3]) : undefined;
      }
      for (const typed of paths) {
        const targets = flags.has('R') ? await tree(request, absolute(typed), options) : [absolute(typed)];
        for (const path of targets) {
          if (name === 'chmod') {
            const [found] = await sys(request, [{ stat: { path } }], options);
            if (found.err) { failure ??= found.err.errno; continue; }
            await run({ chmod: { path, mode: changed(found.ok.mode & 0o7777, spec, (found.ok.mode & 0o170000) === 0o040000) } });
          } else await run({ chown: { path, uid, gid, nofollow: flags.has('h') } });
        }
      }
      break;
    }
    case 'touch':
      for (const typed of operands) {
        const path = absolute(typed);
        const e = await errno(request, { utimens: { path } }, options);
        if (e === 'ENOENT' && !flags.has('c')) await run({ mknod: { path, mode: 0o666 } });
        else if (e !== 'ENOENT') failure ??= e;
      }
      break;
    case 'rm':
      for (const typed of operands) {
        const e = flags.has('r') || flags.has('R') ? await rmrf(request, absolute(typed)) : await errno(request, { unlink: { path: absolute(typed) } }, options);
        if (!(flags.has('f') && e === 'ENOENT')) failure ??= e;
      }
      break;
    case 'mkdir':
      for (const typed of operands) {
        if (flags.has('p')) { try { await mkdirp(request, absolute(typed)); } catch (error) { failure ??= error.errno; } }
        else await run({ mkdir: { path: absolute(typed), mode: 0o777 } });
      }
      break;
    case 'mv': {
      const sources = operands.slice(0, -1).map(absolute), to = absolute(operands.at(-1));
      const [found] = await sys(request, [{ stat: { path: to } }], options);
      const into = found.ok && (found.ok.mode & 0o170000) === 0o040000;
      if (!into && (sources.length > 1 || (found.err && found.err.errno !== 'ENOENT'))) { failure = found.err?.errno ?? 'ENOTDIR'; break; }
      for (const from of sources) await run({ rename: { from, to: into ? to.replace(/\/$/, '') + '/' + from.split('/').filter(Boolean).pop() : to } });
      break;
    }
    case 'ln':
      await run({ symlink: { target: operands[0], path: absolute(operands[1]) } });
      break;
    case 'setfacl': {
      const [spec, ...paths] = operands;
      const users = await idsOf(request, '/etc/passwd'), groups = await idsOf(request, '/etc/group');
      for (const typed of paths) {
        const path = absolute(typed);
        const [found, current] = await sys(request, [{ stat: { path } }, { getxattr: { path, name: 'system.posix_acl_access' } }], options);
        if (found.err) { failure ??= found.err.errno; continue; }
        const mode = found.ok.mode;
        const entries = current.ok ? JSON.parse(Buffer.from(current.ok).toString()) : { user: mode >> 6 & 7, group: mode >> 3 & 7, other: mode & 7, mask: null, users: {}, groups: {} };
        entries.users ??= {}; entries.groups ??= {};
        let masked = false;
        for (const entry of spec.split(',')) {
          const [tag, who, perms] = entry.split(':');
          const bits = [...perms].reduce((sum, ch) => sum | ({ r: 4, w: 2, x: 1 }[ch] ?? 0), 0);
          if (tag.startsWith('u')) { if (who) entries.users[users.get(who)?.[2] ?? who] = bits; else entries.user = bits; }
          else if (tag.startsWith('g')) { if (who) entries.groups[groups.get(who)?.[2] ?? who] = bits; else entries.group = bits; }
          else if (tag.startsWith('m')) { entries.mask = bits; masked = true; }
          else entries.other = bits;
        }
        if (!masked) entries.mask = [entries.group, ...Object.values(entries.users), ...Object.values(entries.groups)].reduce((a, b) => a | b, 0);
        await run({ setxattr: { path, name: 'system.posix_acl_access', value: [...Buffer.from(JSON.stringify(entries))] } });
      }
      break;
    }
  }
  const status = failure === null ? 200
    : ['EACCES', 'EPERM'].includes(failure) ? (creds.uid === 65534 ? 401 : 403)
    : failure === 'ENOENT' ? 404 : ['EEXIST', 'ENOTEMPTY', 'EISDIR', 'EBUSY'].includes(failure) ? 409 : 400;
  return { status: () => status, ok: () => status === 200, json: async () => ({ errno: failure }), text: async () => failure ?? '' };
}

// A kernel for a test's page that runs no server: it answers the system calls the page makes
// about the files `files` gives (a map, or a function making one, of absolute path to {size,
// mode, user}), all in `/`, and the directory itself.
export function fakeKernel(files) {
  const stat = (path, { size = 0, mode = 0o644, user = 'guest' } = {}, directory = false) => ({ ino: 1, mode: (directory ? 0o040000 : 0o100000) | mode,
    nlink: 1, uid: 1000, gid: 1000, user, group: user, size, mtime: 0, ctime: 0 });
  return route => {
    const known = typeof files === 'function' ? files() : files;
    const { calls } = JSON.parse(route.request().postData());
    const answer = call => {
      if (call === 'getcreds') return { ok: { uid: 65534, gid: 65534, groups: [65534], user: 'guest', group: 'guest', group_names: ['guest'], home: '/', signed_in: false, can_sudo: false, sudo_active: false } };
      const [name, args] = Object.entries(call)[0];
      const path = args.path;
      if (name === 'getdents') return { ok: Object.entries(known).filter(([p]) => p.lastIndexOf('/') === 0).map(([p, f]) => ({ name: p.slice(1), stat: stat(p, f) })) };
      if (path === '/') return { ok: name === 'access' ? null : stat('/', { mode: 0o755 }, true) };
      if (!(path in known)) return { err: { errno: 'ENOENT' } };
      return { ok: name === 'access' ? null : ['stat', 'lstat'].includes(name) ? stat(path, known[path]) : null };
    };
    return route.fulfill({ json: { results: calls.map(answer) } });
  };
}
