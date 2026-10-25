import { readFile, readdir } from 'node:fs/promises';
const port = process.env.TERMINAL_PORT || '8788';
if (!/^\d+$/.test(port)) throw new Error('Invalid local port');
const origin = `http://127.0.0.1:${port}`; // Deliberately local-only; never seed a remote account.
const vars = await readFile('.dev.vars', 'utf8');
const password = process.env.TERMINAL_PASSWORD || vars.match(/^ADMIN_PASSWORD=(.+)$/m)?.[1];
if (!password) throw new Error('Run npm run setup first');
const login = await fetch(origin + '/api/login', { method: 'POST', headers: { Origin: origin, 'Content-Type': 'application/json' }, body: JSON.stringify({ password }) });
if (!login.ok) throw new Error(`Local login failed: ${login.status} ${await login.text()}`);
const cookie = login.headers.get('set-cookie').split(';')[0];
const headers = { Origin: origin, Cookie: cookie };
async function upload(path, body) {
  const response = await fetch(origin + '/api/file?path=' + encodeURIComponent(path), { method: 'PUT', headers, body });
  if (!response.ok && response.status !== 409) throw new Error(`${path}: ${response.status} ${await response.text()}`);
  console.log(`${response.status === 409 ? 'Keeping existing' : 'Wrote'} ${path}`);
}
for (const name of await readdir('seed')) await upload('/' + name, await readFile('seed/' + name));
const directory = await fetch(origin + '/api/mkdir?path=' + encodeURIComponent('/media'), { method: 'POST', headers });
if (!directory.ok) throw new Error(await directory.text());
// A generated, quiet two-second sine wave; no external media or network dependencies.
const sampleRate = 22050, count = sampleRate * 2, wav = Buffer.alloc(44 + count * 2);
wav.write('RIFF', 0); wav.writeUInt32LE(wav.length - 8, 4); wav.write('WAVEfmt ', 8); wav.writeUInt32LE(16, 16);
wav.writeUInt16LE(1, 20); wav.writeUInt16LE(1, 22); wav.writeUInt32LE(sampleRate, 24); wav.writeUInt32LE(sampleRate * 2, 28);
wav.writeUInt16LE(2, 32); wav.writeUInt16LE(16, 34); wav.write('data', 36); wav.writeUInt32LE(count * 2, 40);
for (let i = 0; i < count; i++) wav.writeInt16LE(Math.round(Math.sin(i / sampleRate * Math.PI * 220 * 2) * 1000 * Math.sin(Math.PI * i / count)), 44 + i * 2);
await upload('/media/signal.wav', wav);
console.log('Local examples are ready.');
