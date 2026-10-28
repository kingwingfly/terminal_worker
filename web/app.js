import { inspectFaststart } from './faststart.mjs';
import { uploadFile, Cancelled } from './upload.mjs';
import { record } from './record.mjs';
import { speak, listen as hearLive } from './sound.mjs';

let configuration;
export const ready = fetch('/terminal-config.json').then(async response => {
  if (!response.ok) throw new Error('Cannot read terminal configuration');
  configuration = await response.json();
  document.title = configuration.branding.name + configuration.branding.subtitle;
  document.querySelector('meta[name="description"]').content = configuration.branding.description;
});

const $ = id => document.getElementById(id);
const fileUrl = path => '/api/file?path=' + encodeURIComponent(path);
// Previews render in Rust (crates/frontend/src/viewer.rs), through window.terminalPreview.
let directory = '/', opener, viewerVersion = 0, releasePreview;
// Lists and the account may have changed: the tabs ask again. With a reason, the account changed
// from elsewhere, which the tabs say: 'ended', this session ended; 'window', another window
// of this browser signed in or out.
const changed = (reason = null) => window.dispatchEvent(new CustomEvent('terminal:changed', { detail: reason }));
async function api(url, options) {
  const response = await fetch(url, options);
  if (!response.ok) { let error; try { error = (await response.json()).error; } catch {} const failure = new Error(error || `Request failed (${response.status})`); failure.status = response.status; throw failure; }
  return response;
}
async function session(path = directory) { return (await api('/api/session?path=' + encodeURIComponent(path))).json(); }
// Endpoints that change this browser's session. The session one replaces ends as it answers,
// which this page hears of before its own identityChanged: until then, that is not news.
const SESSION_CHANGES = new Set(['login', 'logout', 'passwd', 'register']);
let changingSession = 0;
async function post(endpoint, data) {
  const changes = SESSION_CHANGES.has(endpoint);
  if (changes) changingSession++;
  try { return await postJson(endpoint, data); } catch (error) { if (changes) changingSession--; throw error; }
}
async function postJson(endpoint, data) {
  return (await api('/api/' + endpoint, { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(data) })).json();
}
// One dialog at a time: a request made while another is open, as from another tab, waits for it
// to close and then asks its own question, rather than receiving the other's answer.
let dialogs = Promise.resolve();
function credentials(options) {
  const answer = dialogs.then(() => askCredentials(options));
  dialogs = answer.catch(() => {});
  return answer;
}
function askCredentials({ title, description, username, chooseUser = false, newPassword = false, currentPassword = false, action, submit }) {
  return new Promise(resolve => {
    const dialog = $('auth'), form = $('auth-form'), password = $('password'), user = $('username');
    $('auth-title').textContent = title; $('auth-description').textContent = description; $('auth-error').textContent = '';
    $('username-field').hidden = !chooseUser; user.value = username || 'root'; user.required = chooseUser;
    $('current-password-field').hidden = !currentPassword; $('current-password').required = currentPassword;
    $('confirm-password-field').hidden = !newPassword; $('confirm-password').required = newPassword;
    $('password-label').textContent = newPassword ? 'New password' : 'Password';
    password.autocomplete = newPassword ? 'new-password' : 'current-password';
    for (const id of ['password', 'current-password', 'confirm-password']) $(id).value = '';
    const button = form.querySelector('[type=submit]'); button.textContent = action || (newPassword ? 'Save password' : 'Confirm'); button.disabled = false;
    let completed = false;
    const finish = result => {
      if (completed) return; completed = true;
      for (const id of ['password', 'current-password', 'confirm-password']) $(id).value = '';
      dialog.close(); form.onsubmit = null; dialog.oncancel = null; $('cancel-auth').onclick = null;
      resolve(result);
    };
    form.onsubmit = async event => {
      event.preventDefault(); button.disabled = true;
      try {
        if (newPassword && password.value !== $('confirm-password').value) throw new Error('New passwords do not match');
        const result = await submit({ username: user.value.trim(), password: password.value, current_password: $('current-password').value });
        finish(result);
      } catch (error) { $('auth-error').textContent = error.message; }
      finally { for (const id of ['password', 'current-password', 'confirm-password']) $(id).value = ''; button.disabled = false; }
    };
    dialog.oncancel = event => { event.preventDefault(); if (!button.disabled) finish(null); };
    $('cancel-auth').onclick = () => { if (!button.disabled) finish(null); };
    dialog.showModal(); (currentPassword ? $('current-password') : password).focus();
  });
}
// What others change, as it happens: the authority tells this page which directories changed
// that its account may list (/api/events), and the tabs that show one list it again. A burst of
// changes comes as one. Pings keep proxies and tunnels from closing a quiet line; a line that
// drops opens again, and what was missed meanwhile is listed again.
let events, eventsRetry = 0, pendingDirs = new Set(), pendingTimer;
function listen() {
  events?.close();
  const socket = events = new WebSocket(`${location.protocol === 'https:' ? 'wss' : 'ws'}://${location.host}/api/events`);
  let ping;
  socket.onopen = () => {
    const reconnected = eventsRetry > 0;
    if (eventsRetry) changed(); eventsRetry = 0; ping = setInterval(() => socket.send('ping'), 30000);
    // A new line follows what the page follows (and hears where it is), and asks what it waited
    // to ask.
    if (following || hearing) socket.send(JSON.stringify({ follow: followed() }));
    // A deck shown here asks where its account is now, as turns made elsewhere while the line
    // was down were not heard; unless it turned itself meanwhile, which goes out below.
    const turned = outbox.some(message => message.present?.path === shown?.path);
    if (reconnected && shown && !following && !turned) outbox.push({ where: shown.path });
    for (const message of outbox.splice(0)) socket.send(JSON.stringify(message));
  };
  socket.onmessage = event => {
    // A line this page replaced, as its account changed, has nothing more to say.
    if (events !== socket) return;
    let message; try { message = JSON.parse(event.data); } catch { return; }
    // This page's session ended elsewhere, not by its own sign in or out: it is a guest's now.
    if (message.session === 'ended') { if (!changingSession) void identityChanged('ended'); return; }
    if (message.present) { presented(message.present); return; }
    // A device followed shows something else now: followed again, if it still may be.
    if (message.device?.replaced) {
      const { name } = message.device;
      const again = error => window.dispatchEvent(new CustomEvent('terminal:device', { detail: { name, error: error.message } }));
      if (following?.device === name) attend(name).catch(again);
      if (hearing?.device === name) hear(name).catch(again);
      return;
    }
    // The presentation followed ended, or may not be followed.
    if (message.device) {
      if (following?.device === message.device.name) following.ended = true;
      if (hearing?.device === message.device.name) stopHearing();
      window.dispatchEvent(new CustomEvent('terminal:device', { detail: message.device }));
      return;
    }
    const dirs = message.changed;
    if (!Array.isArray(dirs)) return;
    dirs.forEach(dir => pendingDirs.add(dir));
    clearTimeout(pendingTimer);
    pendingTimer = setTimeout(() => { const detail = [...pendingDirs]; pendingDirs = new Set(); window.dispatchEvent(new CustomEvent('terminal-remote', { detail })); }, 250);
  };
  socket.onclose = () => { clearInterval(ping); if (events === socket) setTimeout(listen, Math.min(30000, 1000 * 2 ** eventsRetry++)); };
}
listen();
// Slides and PDFs presented follow each other (owner's request, 2026-10-28): the place a page
// presents (viewer.rs calls present) shows in this browser's other windows showing that file,
// and in the account's pages signed in elsewhere, which the authority tells (store.rs relay).
const presenting = 'BroadcastChannel' in window ? new BroadcastChannel('terminal-present') : null;
// The last place this page heard of or showed, by path, which a deck shown later starts at
// (viewer.rs follow_presenting): one may come while its file is still being read.
const heard = new Map();
const presented = detail => { heard.set(detail.path, detail.at); window.dispatchEvent(new CustomEvent('terminal:present', { detail })); };
if (presenting) presenting.onmessage = event => presented(event.data);
function present(path, column, row) {
  const place = { path, at: [column, row] };
  heard.set(path, place.at);
  presenting?.postMessage(place);
  // While the line is down, only the last place of each file waits for it.
  const waiting = outbox.findIndex(message => message.present?.path === path);
  if (waiting >= 0) outbox.splice(waiting, 1);
  say({ present: place });
}
// Said once the line is open, if it is not yet: what this page asks, as it opens a deck.
const outbox = [];
function say(message) { if (events?.readyState === WebSocket.OPEN) events.send(JSON.stringify(message)); else outbox.push(message); }
// Where the account last showed the file at `path`, on any device, which the authority answers
// with a present message; a page opens a deck there (viewer.rs).
function ask(path) { say({ where: path }); }
// The presentation this page follows (devices.rs), as /api/device described it: /dev/present/NAME,
// the file it shows, whose it is, and whether this page's account turns it.
let following = null;
// The devices this page follows, which the authority tells it the end of.
const followed = () => [following?.device, hearing?.device].filter(Boolean);
const follows = () => say({ follow: followed() });
async function attend(device) {
  const info = await (await api('/api/device?path=' + encodeURIComponent(device))).json();
  following = info; follows();
  // Slides and sound together: the presenter's live sound of the same name, if any.
  if (info.sound && hearing?.device !== info.sound) hear(info.sound).catch(error => console.warn('No live sound', error));
  // Shown as the presenter shows it, from where they are.
  if (info.kind === 'pdf') setPlace(info.path, { present: true, ...(info.place ? { page: info.place[0] } : {}) });
  else setPlace(info.path, { slides: true, ...(info.place ? { slide: info.place } : {}) });
  return open(info.path, info.kind, false, true);
}
function unfollow() { if (!following) return; following = null; follows(); }
// A speaker view, which a deck's S opened (viewer.rs): it shows the deck its address names once
// the terminal runs.
export function started() {
  const deck = new URLSearchParams(location.search).get('speaker');
  if (!deck) return;
  document.title = 'Speaker view · ' + deck.split('/').pop();
  void open(deck, /\.slides\.md$/i.test(deck) ? 'slides' : 'markdown');
}
// Windows of this browser share its session cookie: each tells the others when it changes it,
// so that none goes on showing one account while its requests are another's.
const windows = 'BroadcastChannel' in window ? new BroadcastChannel('terminal-session') : null;
if (windows) windows.onmessage = () => void identityChanged('window');
async function identityChanged(reason = null) {
  if (!reason) changingSession = 0;
  // A new account hears of what it may list: the line opens again with its session.
  listen();
  if (!reason) windows?.postMessage('changed');
  // A private preview must not survive switching to a less privileged account.
  if ($('viewer').open) await closeViewer();
  changed(reason);
}
async function signIn(username, switching = false) {
  let result;
  if (switching && (await session()).uid === 0) result = await post('login', { username, switch: true });
  else result = await credentials({ title: switching ? `Switch to ${username}` : 'Sign in', description: 'Access is determined by the file owner, group, and rwx permissions.', username, chooseUser: !switching,
    submit: fields => post('login', { username: switching ? username : fields.username, password: fields.password, switch: switching }) });
  if (result) await identityChanged();
  return result;
}
async function unlock(path = directory) {
  const current = await session(path);
  if (current.authenticated || current.can_write) return true;
  throw new Error('Permission denied: sign in with su USER before retrying; your account will not change automatically');
}

async function sudoAuthenticate() {
  // A sudo another tab is asking for may make this one unnecessary.
  await dialogs;
  const current = await session();
  if (!current.authenticated) throw new Error('guest cannot use sudo; sign in to a wheel account with su USER first');
  if (!current.can_sudo) throw new Error('Current account is not in the wheel group');
  if (current.sudo_active) return true;
  return !!await credentials({ title: `sudo · ${current.username}`, description: 'Verify your own password. sudo authorization lasts 5 minutes; ordinary operations still use your current account.',
    submit: fields => post('sudo', { password: fields.password }) });
}
async function accountCommand(args, cwd) {
  // Help (-h, help) is the terminal's own, from the commands' definitions in Rust, as are the
  // manual's pages; this shows them.
  if (args[0] === 'man') {
    if (args.length > 2) throw new Error('Usage: man [COMMAND]');
    const text = args[1] ? window.terminalManual.page(args[1]) : window.terminalManual.index();
    if (text === undefined || text === null) throw new Error(`No manual entry for ${args[1]}. Enter man for the index.`);
    const version = await cleanupViewer(); if (version !== viewerVersion) return {};
    trail.length = 0; showTrail();
    viewerTitle(args[1] ? 'man · ' + args[1] : 'Terminal Worker · Manual'); $('download').hidden = true;
    const article = document.createElement('article'); article.className = 'markdown manual'; article.innerHTML = window.terminalPreview.markdown(text, '/'); $('viewer-content').replaceChildren(article);
    if (!$('viewer').open) { opener = document.activeElement; $('viewer').showModal(); placePlayer(); }
    return {};
  }
  if (args[0] === 'vim') {
    if (args.length !== 2) throw new Error('Usage: vim <PATH>');
    let value = args[1]; const current = await session();
    if (value === '~') value = current.home; else if (value.startsWith('~/')) value = current.home + value.slice(1);
    if (/[\\\x00-\x1f]/.test(value)) throw new Error('Invalid file path');
    const parts = []; for (const part of (value.startsWith('/') ? value : cwd + '/' + value).split('/')) { if (part === '..') parts.pop(); else if (part && part !== '.') parts.push(part); }
    const { edit } = await import('./editor.mjs'); await edit('/' + parts.join('/'), { api, fileUrl, unlock, changed }); return {};
  }
  let sudo = args[0] === 'sudo';
  if (sudo) {
    args = args.slice(1);
    if (args.length === 1 && args[0] === '-k') { await post('sudo', { revoke: true }); return { output: 'sudo authorization revoked' }; }
    if (!args.length) throw new Error('Usage: sudo <COMMAND> / sudo -u USER / sudo -k');
    if (args[0] === '-u') {
      if (args.length !== 2 || args[1].startsWith('-')) throw new Error('Usage: sudo -u USER (switch accounts; exit returns to guest)');
      if (!await sudoAuthenticate()) throw new Error('sudo canceled');
      const result = await post('login', { username: args[1], sudo: true });
      await identityChanged();
      return { output: `Current account: ${result.username}`, home: result.home };
    }
    if (!await sudoAuthenticate()) throw new Error('sudo canceled');
  }
  if (args[0] === 'passwd') {
    const force = args.includes('--force');
    const rest = args.slice(1).filter(arg => arg !== '--force');
    if (rest.length > 1 || rest[0]?.startsWith('-') || args.filter(arg => arg === '--force').length > 1) throw new Error('Usage: passwd [--force] [USER]; enter the password in the dialog');
    let current = await session();
    if (!current.authenticated) throw new Error('Sign in with su USER before changing a password');
    const username = rest[0] || current.username;
    const result = await credentials({ title: `Change password for ${username}`, description: (force ? '--force enabled: short passwords are allowed, but cannot be empty. ' : 'Use at least 12 characters, or run passwd --force to allow a short password. ') + 'Saving revokes all previous sessions for this account.', newPassword: true, currentPassword: !sudo && current.uid !== 0,
      submit: fields => post('passwd', { username, password: fields.password, current_password: fields.current_password, sudo, force }) });
    if (!result) throw new Error('Password change canceled');
    await identityChanged(); return { output: `Password updated for ${username}` };
  }
  // Signed out, useradd NAME makes an account of one's own, with the password given here.
  if (args[0] === 'useradd' && !sudo && !(await session()).authenticated) {
    const force = args.includes('--force');
    const rest = args.slice(1).filter(arg => arg !== '--force');
    if (rest.length !== 1 || rest[0].startsWith('-')) throw new Error('Usage: useradd [--force] NAME (signed out: an account of your own, home /guest/NAME); options need sudo');
    const username = rest[0];
    const result = await credentials({ title: `Create account ${username}`, description: (force ? '--force enabled: short passwords are allowed, but cannot be empty. ' : 'Use at least 12 characters, or run useradd --force NAME to allow a short password. ') + `Your home will be /guest/${username}, which only you may enter; you will be signed in.`, newPassword: true, action: 'Create account',
      submit: fields => post('register', { username, password: fields.password, force }) });
    if (!result) throw new Error('Account creation canceled');
    await identityChanged();
    return { output: `Created user ${username} and signed in; home ${result.home}`, home: result.home };
  }
  if (['su', 'login'].includes(args[0])) {
    if (sudo) throw new Error('Use su root to switch accounts');
    const loginShell = args[0] === 'login' || args[1] === '-';
    const rest = args.slice(args[1] === '-' ? 2 : 1);
    if (rest.length > 1 || rest[0]?.startsWith('-')) throw new Error('Usage: su [-] [USER] / login [USER]');
    const result = await signIn(rest[0] || 'root', args[0] === 'su');
    if (!result) throw new Error('Sign-in canceled');
    return { output: `Current account: ${result.username}`, home: loginShell ? result.home : undefined };
  }
  if (['logout', 'exit'].includes(args[0])) {
    if (sudo || args.length !== 1) throw new Error(`Usage: ${args[0]}`);
    await post('logout', {}); await identityChanged(); return { output: 'Signed out as guest (access depends on directory permissions)', home: '/' };
  }
  const result = await post('command', { args, cwd, sudo });
  changed(); return result;
}
let uploading = null;
// Upload `files` into `path`, one after another, in chunks that resume if interrupted;
// `progress` hears how each goes. Settles with a line per file. Cancelling (cancelUploads) stops
// the batch and removes the unfinished file.
async function uploadFiles(files, path, progress) {
  if (!files.length) return [];
  if (uploading) throw new Error('An upload is already in progress in another tab; wait for it, or cancel it there');
  const controller = uploading = new AbortController();
  const results = [];
  try {
    if (!await unlock(path)) return [];
    const rust = window.terminalUploads;
    for (const file of files) {
      if (controller.signal.aborted) { results.push(`✕ ${file.name}: Upload cancelled`); continue; }
      try {
        if (file.size > rust.maxFile) throw new Error(`Exceeds the ${rust.maxFile / 1e9} GB limit`);
        // Not faststart is only a warning: such a file uploads, and plays once its index has.
        let warning = null;
        if (/\.(mp4|m4v|mov|m4a)$/i.test(file.name)) {
          progress(`↑ ${file.name} · checking faststart`);
          warning = await faststartWarning(file);
        }
        const flagged = warning ? ' · ⚠ not faststart' : '';
        const destination = (path === '/' ? '' : path.replace(/\/$/, '')) + '/' + file.name;
        const done = await uploadFile(file, destination, { signal: controller.signal,
          progress: state => { progress(`↑ ${file.name} · ${state}${flagged}`); if (!state.startsWith('hashing')) changed(); } });
        results.push(`✓ ${file.name}${done.resumed ? ' (resumed)' : ''}`);
        if (warning) results.push(`  ⚠ ${warning}`);
      } catch (error) { results.push(`✕ ${file.name}: ${error instanceof Cancelled ? 'Upload cancelled' : error.message}`); }
    }
    return results;
  } finally { uploading = null; changed(); }
}
// Why `file`, an MP4-family video or audio, will not stream, if it will not: its index (moov)
// after its media (mdat) makes players fetch the end before they start, which a file still
// uploading does not have yet. Null when it is faststart.
async function faststartWarning(file) {
  const fix = `to stream it, run faststart '${file.name.replaceAll("'", "'\\''")}' once it has uploaded`;
  try {
    if ((await inspectFaststart(file)).faststart) return null;
    return `not faststart: its index (moov) is after its media, so it plays only once the end has downloaded, and not until its upload completes; ${fix}`;
  } catch (error) {
    return `faststart could not be checked (${error.message}); if it does not play while downloading, ${fix}`;
  }
}
// Cancel the uploads in progress, if any; whether there were.
function cancelUploads() { if (!uploading) return false; uploading.abort(); return true; }
// The microphone this page records (stream FILE), into `path`, and sends live as /dev/snd/NAME
// when it may (sound.mjs); `progress` hears how it goes. Ctrl+C in its tab stops it
// (stopRecording), which saves what was recorded. Settles with what was saved.
let recording = null;
async function recordSound(path, name, progress) {
  if (recording) throw new Error('This page is recording already; stop it with Ctrl+C in its tab');
  const controller = recording = new AbortController();
  let microphone, live = null, liveNote = '', speaking = Promise.resolve();
  try {
    try { microphone = await navigator.mediaDevices.getUserMedia({ audio: { echoCancellation: true, noiseSuppression: true, autoGainControl: true } }); }
    catch (error) { throw new Error('No microphone: ' + error.message); }
    const saved = await record(path, microphone, { signal: controller.signal, progress: text => progress(text + liveNote),
      // Live once the file is there, which its device links to; a recording all the same if not.
      begun: () => { speaking = speak(path, name, microphone).then(sent => { live = sent; liveNote = ` · live as ${sent.device}`; }, error => { liveNote = ` · not live: ${error.message}`; }); } });
    await speaking;
    return { ...saved, device: live?.device ?? null, note: liveNote + (saved.stopped ? ` · ${saved.stopped}` : '') };
  } finally {
    recording = null;
    microphone?.getTracks().forEach(track => track.stop());
    // A recording that failed may still be connecting its live sound, which ends with it.
    await speaking;
    await live?.end();
    changed();
  }
}
function stopRecording() { if (!recording) return false; recording.abort(); return true; }
// What this page hears live (/dev/snd/NAME), in a small player of its own, which goes on playing
// whatever the viewer shows; one sound at a time.
let hearing = null;
// Which hear() is the latest: one stopped, or replaced by another, while it connects is let go.
let hearings = 0;
async function hear(device) {
  stopHearing();
  const turn = ++hearings;
  // The sound plays from outside the player and the viewer, so that moving them never stops it.
  const player = $('listening'), audio = Object.assign(document.createElement('audio'), { id: 'listening-audio', autoplay: true, hidden: true });
  document.body.append(audio);
  player.hidden = false; player.dataset.state = 'connecting'; $('listening-mute').textContent = 'Mute';
  $('listening-name').textContent = device;
  const stop = await hearLive(device, audio).catch(error => { if (turn === hearings) stopHearing(); throw error; });
  if (turn !== hearings) { stop(); audio.remove(); return; }
  hearing = { device, stop }; player.dataset.state = 'live'; follows();
  // A browser that wants a tap to play sound shows Listen.
  try { await audio.play(); } catch { if (turn === hearings) player.dataset.state = 'paused'; }
}
function stopHearing() {
  hearings++;
  hearing?.stop(); hearing = null; follows();
  $('listening-audio')?.remove();
  const player = $('listening'); player.hidden = true; delete player.dataset.state;
}
$('listening-play').onclick = () => { $('listening-audio')?.play().then(() => { $('listening').dataset.state = 'live'; }, () => {}); };
$('listening-mute').onclick = () => { const audio = $('listening-audio'); if (!audio) return; audio.muted = !audio.muted; $('listening-mute').textContent = audio.muted ? 'Unmute' : 'Mute'; };
$('listening-stop').onclick = stopHearing;
// The player goes where it can be used: in the viewer while it is open, as the page around a
// modal dialog takes no clicks, and full screen with it. The sound plays from outside both.
function placePlayer() { ($('viewer').open ? $('viewer-surface') : document.body).append($('listening')); }
// Open the file chooser now, while the tap or the key that asked for it lets a page open one:
// phones refuse once a request has been awaited. Settles with the files chosen; none if closed.
const chooser = Object.assign(document.createElement('input'), { type: 'file', multiple: true });
let choosing = null;
// Settle the chooser with no files, if it is open: Ctrl+C, as a browser may close it without
// saying so (no cancel event), or not open it at all, which would leave its command waiting.
function cancelChoosing() { if (!choosing) return false; choosing([]); return true; }
function chooseFiles() {
  choosing?.([]);
  return new Promise(resolve => {
    choosing = files => { choosing = null; chooser.onchange = chooser.oncancel = null; resolve(files); };
    chooser.value = '';
    chooser.onchange = () => choosing?.([...chooser.files]);
    chooser.oncancel = () => choosing?.([]);
    chooser.click();
  });
}
let depth = 0;
window.addEventListener('dragenter', e => { if (e.dataTransfer?.types.includes('Files')) { e.preventDefault(); depth++; $('dropzone').hidden = false; } });
window.addEventListener('dragover', e => { if (e.dataTransfer?.types.includes('Files')) e.preventDefault(); });
window.addEventListener('dragleave', () => { if (--depth <= 0) { depth = 0; $('dropzone').hidden = true; } });
// Dropped files go to the active tab, which uploads them as its upload command would.
window.addEventListener('drop', e => { e.preventDefault(); depth = 0; $('dropzone').hidden = true; if (e.dataTransfer?.files.length) window.dispatchEvent(new CustomEvent('terminal-files', { detail: [...e.dataTransfer.files] })); });

// Where each file's preview was left, by path, kept in this browser for the last 200 files, so
// that opening one again continues there: the page code records how far it was scrolled and
// played; slides and PDFs record their slide, page and zoom (viewer.rs).
const PLACES = 'terminal-places';
function places() { try { return JSON.parse(localStorage.getItem(PLACES) || '[]'); } catch { return []; } }
function place(path) { return places().find(([p]) => p === path)?.[1] ?? null; }
function setPlace(path, value) {
  try {
    const list = places(), old = list.find(([p]) => p === path)?.[1];
    localStorage.setItem(PLACES, JSON.stringify([[path, { ...old, ...value }], ...list.filter(([p]) => p !== path)].slice(0, 200)));
  } catch {}
}
// The file the viewer shows, and those it showed before, as links led from one to the next:
// Back returns to them. A file opened from the terminal starts afresh.
let shown = null;
const trail = [];
function showTrail() { $('viewer-back').hidden = !trail.length; $('viewer-back').title = trail.length ? `Back to ${trail.at(-1).path}` : ''; }
async function follow(path, kind) { if (shown) trail.push(shown); return open(path, kind, true); }
$('viewer-back').onclick = () => { const previous = trail.pop(); if (previous) void open(previous.path, previous.kind, true); };
// The viewer's title. A path too long for the bar loses its start, not its file name: the title
// overflows right to left (style.css), and <bdi> keeps the path's own order.
function viewerTitle(text) { const title = document.createElement('bdi'); title.textContent = text; $('viewer-title').replaceChildren(title); }
async function cleanupViewer() {
  const version = ++viewerVersion;
  const releases = [];
  if (shown) {
    const media = $('viewer-content').querySelector('video,audio');
    setPlace(shown.path, { scroll: $('viewer-content').scrollTop, ...(media && media.currentTime > 1 ? { time: media.currentTime } : {}) });
    // Kept once: the dialog's close cleans up again, after the content is gone.
    shown = null;
  }
  for (const frame of $('viewer-content').querySelectorAll('iframe')) {
    try { releases.push(frame.contentWindow?.signal?.dispose?.()); } catch (error) { console.warn('Failed to release game audio', error); }
  }
  for (const media of $('viewer-content').querySelectorAll('video,audio')) { media.pause(); media.removeAttribute('src'); media.load(); }
  if (releasePreview) { releasePreview(); releasePreview = null; }
  // AudioContext.close() completes asynchronously. Keep the frame alive until
  // its audio resources are released; an old cleanup must not erase a new open.
  await Promise.allSettled(releases);
  if (version === viewerVersion) {
    $('viewer-content').replaceChildren(); $('viewer').classList.remove('slides-view', 'game-view'); $('viewer-content').classList.remove('viewer-error');
    $('fullscreen-error').hidden = true;
  }
  return version;
}
async function closeViewer() {
  // Release a game's audio while its iframe is still visible and active.
  // Waiting for the dialog's queued close event can be too late in Chromium.
  const exit = document.fullscreenElement && $('viewer-surface').contains(document.fullscreenElement)
    ? document.exitFullscreen().catch(error => console.warn('Failed to exit preview fullscreen', error)) : Promise.resolve();
  const version = await cleanupViewer();
  await exit;
  if (version === viewerVersion) $('viewer').close();
}
$('viewer').addEventListener('cancel', event => { event.preventDefault(); void closeViewer(); });
// Escape closes a dialog, and nothing else. Left to the browser, Safari also leaves its full-screen
// window, so the page takes the key and asks the dialog to cancel, which each one handles as its
// own Escape (the viewer closes, the editor asks about unsaved changes). Keys handled inside a
// dialog first, as the preview search's and the editor's, are left alone.
document.addEventListener('keydown', event => {
  // A preview shown full screen leaves that first, as the browser does with Escape.
  if (event.key !== 'Escape' || event.defaultPrevented || event.isComposing || document.fullscreenElement) return;
  const dialog = document.querySelector('dialog[open]');
  if (!dialog) return;
  event.preventDefault();
  if (dialog.dispatchEvent(new Event('cancel', { cancelable: true }))) dialog.close();
});
$('viewer').addEventListener('close', async () => {
  unfollow(); placePlayer();
  const returnFocus = opener;
  // Start exiting before removing a fullscreen child (e.g. a video or iframe),
  // and restore focus after the browser finishes its fullscreen transition.
  const exit = document.fullscreenElement && $('viewer-surface').contains(document.fullscreenElement)
    ? document.exitFullscreen() : Promise.resolve();
  const closedVersion = await cleanupViewer();
  await exit.catch(error => console.warn('Failed to exit preview fullscreen', error));
  if (viewerVersion === closedVersion && !$('viewer').open && !$('auth').open) {
    (returnFocus?.isConnected ? returnFocus : $('command'))?.focus();
  }
});
$('close-viewer').onclick = closeViewer;
$('fullscreen').onclick = async () => {
  const status = $('fullscreen-error'); status.hidden = true;
  try {
    if (document.fullscreenElement) await document.exitFullscreen();
    else {
      // A dialog itself is not an eligible fullscreen element. Its ordinary
      // descendant can enter fullscreen while retaining modal focus handling.
      const surface = $('viewer-surface');
      if (typeof surface.requestFullscreen !== 'function') throw new Error('Fullscreen API is unavailable in this environment');
      if (document.fullscreenEnabled === false) throw new Error('Fullscreen is not permitted for this page');
      await surface.requestFullscreen();
    }
  } catch (error) { status.textContent = `Fullscreen failed: ${error.message}`; status.hidden = false; }
};
// Full screen, the preview's bars float over the content, hidden until the pointer moves, as a
// video player's controls are; they stay while the pointer, or the keyboard's focus, is on one
// (a button clicked keeps the focus, but not :focus-visible).
const bars = ':is(.viewer-bar, .preview-bar, .slides-bar, .pdf-toolbar)';
let chromeTimer;
function showChrome() {
  const surface = $('viewer-surface');
  surface.classList.add('chrome'); clearTimeout(chromeTimer);
  chromeTimer = setTimeout(() => {
    if (surface.querySelector(`${bars}:hover, ${bars}:has(:focus-visible)`)) showChrome();
    else surface.classList.remove('chrome');
  }, 2000);
}
const fullscreen = () => document.fullscreenElement === $('viewer-surface');
// A button clicked in the viewer leaves the focus where it was, so that keys go on turning slides
// and pages: Space would press it again (Fullscreen, say). One tabbed to takes Space and Enter.
$('viewer-surface').addEventListener('mousedown', event => { if (event.target instanceof Element && event.target.closest('button')) event.preventDefault(); });
// A tap moves no pointer: pointerdown shows them on touch screens.
for (const type of ['pointermove', 'pointerdown']) $('viewer-surface').addEventListener(type, () => { if (fullscreen()) showChrome(); });
$('viewer-surface').addEventListener('focusin', event => { if (fullscreen() && event.target.closest(bars)) showChrome(); });
document.addEventListener('fullscreenchange', () => {
  const active = !!document.fullscreenElement;
  $('fullscreen').setAttribute('aria-label', active ? 'Exit fullscreen' : 'Fullscreen');
  $('fullscreen').setAttribute('aria-pressed', String(active));
  // Shown a moment on entering, so that the way out is seen.
  if (fullscreen()) showChrome();
  else { clearTimeout(chromeTimer); $('viewer-surface').classList.remove('chrome'); }
});
window.addEventListener('message', event => { if (event.origin === location.origin && event.source === $('viewer-content').querySelector('iframe')?.contentWindow && event.data === 'close-game') closeViewer(); });
async function open(path, kind, followed = false, attending = false) {
  if (/^\/dev\/present\/[^/]+$/.test(path)) return attend(path);
  if (/^\/dev\/snd\/[^/]+$/.test(path)) return hear(path);
  if (!attending && following?.path !== path) unfollow();
  if (!followed) trail.length = 0;
  const version = await cleanupViewer();
  if (version !== viewerVersion) return;
  shown = { path, kind }; showTrail();
  const content = $('viewer-content'), dialog = $('viewer');
  $('download').hidden = false;
  viewerTitle(path); $('download').href = fileUrl(path); $('download').download = path.split('/').pop();
  if (!dialog.open) { opener = document.activeElement; dialog.showModal(); placePlayer(); }
  content.textContent = 'Loading...';
  try {
    if (kind === 'game') {
      const meta = await api(fileUrl(path), { method: 'HEAD' });
      if (Number(meta.headers.get('Content-Length')) > 2_000_000) throw new Error('Text exceeds 2 MB; use the download button in the top right.');
      const text = await (await api(fileUrl(path))).text();
      if (version !== viewerVersion) return;
      const manifest = JSON.parse(text);
      if (manifest.engine !== 'bevy' || !Object.hasOwn(configuration.games, manifest.entry)) throw new Error('Game entry is not registered; add it to the site content configuration and rebuild');
      content.replaceChildren(); dialog.classList.add('game-view');
      const iframe = document.createElement('iframe'); iframe.src = configuration.games[manifest.entry]; iframe.title = manifest.title || manifest.entry; iframe.allow = 'autoplay; fullscreen'; content.append(iframe); iframe.focus();
      return;
    }
    if (kind === 'slides') dialog.classList.add('slides-view');
    // The preview mounts only while the viewer still shows this file.
    content.dataset.version = String(version);
    const release = await window.terminalPreview.show(content, path, kind, String(version));
    if (version === viewerVersion) releasePreview = release; else return release();
    // Back where it was left.
    const saved = place(path);
    if (saved?.scroll) requestAnimationFrame(() => { content.scrollTop = saved.scroll; });
    const media = content.querySelector('video,audio');
    if (media && saved?.time) media.addEventListener('loadedmetadata', () => { if (saved.time < media.duration - 1) media.currentTime = saved.time; }, { once: true });
  } catch (error) { if (version === viewerVersion) { content.textContent = error.message; content.classList.add('viewer-error'); } }
}
// Leptos updates the DOM asynchronously. Observe committed output, then scroll
// at the next layout frame instead of measuring the previous command's height.
// Every tab's transcript follows its output; a tab shown again scrolls to its end, as does one
// that the on-screen keyboard shrinks.
const observedTranscripts = new WeakSet();
function observeTranscript() {
  for (const log of document.querySelectorAll('.transcript')) {
    let frame;
    const scroll = () => { cancelAnimationFrame(frame); frame = requestAnimationFrame(() => { log.scrollTop = log.scrollHeight; }); };
    if (!observedTranscripts.has(log)) {
      observedTranscripts.add(log);
      new MutationObserver(scroll).observe(log, { childList: true, subtree: true, characterData: true });
      new ResizeObserver(scroll).observe(log);
    }
    scroll();
  }
}
// Phones: the app takes the part of the screen that is visible, which the on-screen keyboard
// shrinks, so that the command line stays above it (--app-height and --app-top, style.css); while
// the keyboard is open, the page is marked .keyboard, and the header and the files make room.
if (window.visualViewport) {
  const viewport = window.visualViewport;
  let tallest = 0, width = 0;
  const fit = () => {
    const root = document.documentElement;
    if (viewport.width !== width) { width = viewport.width; tallest = 0; }
    tallest = Math.max(tallest, viewport.height);
    root.style.setProperty('--app-height', `${viewport.height}px`);
    root.style.setProperty('--app-top', `${viewport.offsetTop}px`);
    root.classList.toggle('keyboard', viewport.height < tallest * 0.8);
    if (document.activeElement?.classList.contains('command-input')) document.activeElement.scrollIntoView({ block: 'nearest' });
  };
  viewport.addEventListener('resize', fit); viewport.addEventListener('scroll', fit); fit();
}
window.terminal = { config: () => configuration.branding, open, follow, place, setPlace, present, presented: path => heard.get(path) ?? null, ask, recordSound, stopRecording, attending: path => following?.path === path ? following : null, unlock, chooseFiles, cancelChoosing, uploadFiles, cancelUploads, accountCommand, completionFocus: (value, insideQuote) => {
  requestAnimationFrame(() => {
    const input = $('command');
    if (input?.value !== value || document.querySelector('dialog[open]')) return;
    input.focus({ preventScroll: true });
    const end = value.length - (insideQuote && value.endsWith('"') ? 1 : 0);
    input.setSelectionRange(end, end);
    document.querySelector('.completion-options [aria-selected="true"]')?.scrollIntoView({ block: 'nearest', inline: 'nearest' });
  });
}, setDirectory: path => { directory = path; }, focusInput: () => {
  observeTranscript();
  restoreTerminalFocus();
} };

// A file name keeps to one line; one that does not fit slides to show the rest (style.css). Each
// gets how far it overflows, measured as the list changes, a pane is shown or hidden, or the
// window resizes.
let namesFrame;
function measureNames() {
  cancelAnimationFrame(namesFrame);
  namesFrame = requestAnimationFrame(() => {
    for (const name of document.querySelectorAll('.files .filename')) {
      const overflow = name.scrollWidth - name.clientWidth;
      if (overflow > 2) {
        name.dataset.overflow = '';
        name.style.setProperty('--overflow', `-${overflow}px`);
        name.style.setProperty('--duration', `${Math.max(3, overflow / 40)}s`);
      } else delete name.dataset.overflow;
    }
  });
}
new MutationObserver(changes => { if (changes.some(change => change.target.closest?.('.files') || change.attributeName)) measureNames(); })
  .observe($('app'), { childList: true, subtree: true, characterData: true, attributeFilter: ['data-files', 'data-terminal', 'data-layout'] });
window.addEventListener('resize', measureNames);

// Dialog focus restoration and Leptos updates may finish after the command promise.
// Wait for the browser's next layout; never steal focus from another open modal.
let resetTerminalFocus = false;
function restoreTerminalFocus() {
  requestAnimationFrame(() => {
    const input = $('command');
    if (document.querySelector('dialog[open]') || !['↵', 'Answer ↵'].includes(input?.closest('form')?.querySelector('.busy')?.textContent)) return;
    // After closing a modal Chromium can report this input as active while its
    // native text-editing focus still belongs to the closed dialog. Reacquire it.
    if (resetTerminalFocus && document.activeElement === input) input.blur();
    resetTerminalFocus = false;
    input?.focus({ preventScroll: true });
  });
}
for (const id of ['auth', 'editor']) {
  $(id).addEventListener('close', () => { resetTerminalFocus = true; restoreTerminalFocus(); });
}

// Redirect ordinary typing only when the surrounding shell is idle. Preserve
// selection, shortcuts, button activation, IME and every modal/editor input.
document.addEventListener('keydown', event => {
  if (event.defaultPrevented || event.isComposing || event.ctrlKey || event.metaKey || event.altKey || document.querySelector('dialog[open]')) return;
  const target = event.target;
  if (!(target instanceof Element) || (target.closest('input,textarea,select,[contenteditable="true"]') && target.getClientRects().length) || window.getSelection()?.toString()) return;
  if (target.closest('button,a') && ['Enter', ' ', 'Tab'].includes(event.key)) return;
  const input = $('command'); if (!input || !['↵', 'Answer ↵'].includes(input.closest('form')?.querySelector('.busy')?.textContent)) return;
  if (event.key.length === 1) {
    event.preventDefault(); input.focus({ preventScroll: true });
    input.setRangeText(event.key, input.selectionStart ?? input.value.length, input.selectionEnd ?? input.value.length, 'end');
    input.dispatchEvent(new Event('input', { bubbles: true }));
  } else if (['ArrowUp','ArrowDown','Backspace'].includes(event.key)) {
    event.preventDefault(); input.focus({ preventScroll: true });
    if (event.key === 'Backspace') { const end=input.selectionEnd??input.value.length,start=input.selectionStart??end; input.setRangeText('',start===end?Math.max(0,start-1):start,end,'end');input.dispatchEvent(new Event('input',{bubbles:true})); }
    else input.dispatchEvent(new KeyboardEvent('keydown',{key:event.key,bubbles:true,cancelable:true}));
  }
});
document.addEventListener('click', event => {
  if (!document.querySelector('dialog[open]') && !window.getSelection()?.toString() && event.target instanceof Element && event.target.closest('.terminal-panel') && !event.target.closest('button,a,input,textarea,select,[contenteditable="true"]')) $('command')?.focus({preventScroll:true});
});
