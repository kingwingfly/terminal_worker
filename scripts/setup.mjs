import { randomBytes } from 'node:crypto';
import { writeFile, access } from 'node:fs/promises';
try { await access('.dev.vars'); console.log('.dev.vars already exists; keeping the bootstrap root password configuration.'); }
catch {
  const password = randomBytes(18).toString('base64url');
  await writeFile('.dev.vars', `ADMIN_PASSWORD=${password}\nSESSION_SECRET=${randomBytes(32).toString('hex')}\n`, { mode: 0o600, flag: 'wx' });
  await writeFile('.local-password', password + '\n', { mode: 0o600, flag: 'wx' });
  console.log('Bootstrap root password written to .local-password (gitignored). For local development only.');
}
