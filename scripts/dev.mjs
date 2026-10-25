// Start `wrangler dev` with the arguments given. Archive jobs run in a container, which needs a
// Docker-compatible engine: where Docker is absent and Podman present, this starts Podman's API
// service for the session and points wrangler at it.
import { spawn, spawnSync } from 'node:child_process';
import { existsSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const has = command => spawnSync('sh', ['-c', `command -v ${command}`], { stdio: 'ignore' }).status === 0;
const env = { ...process.env };
let service;
if (!env.WRANGLER_DOCKER_HOST && !env.DOCKER_HOST && !has('docker') && has('podman')) {
  const socket = join(tmpdir(), `terminal-worker-podman-${process.getuid()}.sock`);
  // Another session's service may answer there already; a socket left from an ended one does not.
  const answers = () => spawnSync('podman', ['--url', 'unix://' + socket, 'version'], { stdio: 'ignore' }).status === 0;
  if (!existsSync(socket) || !answers()) {
    rmSync(socket, { force: true });
    // Without a systemd user session, Podman must manage cgroups itself.
    const cgroups = env.DBUS_SESSION_BUS_ADDRESS ? [] : ['--cgroup-manager=cgroupfs'];
    service = spawn('podman', [...cgroups, 'system', 'service', '--time=0', 'unix://' + socket], { stdio: 'ignore' });
    for (let i = 0; i < 100 && !existsSync(socket); i++) await new Promise(resolve => setTimeout(resolve, 100));
  }
  env.WRANGLER_DOCKER_BIN = join(dirname(fileURLToPath(import.meta.url)), 'podman-docker.sh');
  env.WRANGLER_DOCKER_HOST = 'unix://' + socket;
  // Podman resolves only qualified image names: name the registry of wrangler's egress sidecar.
  if (!env.MINIFLARE_CONTAINER_EGRESS_IMAGE) {
    const miniflare = readFileSync(join(dirname(fileURLToPath(import.meta.url)), '../node_modules/miniflare/dist/src/index.js'), 'utf8');
    const image = miniflare.match(/DEFAULT_CONTAINER_EGRESS_INTERCEPTOR_IMAGE = "([^"]+)"/)?.[1];
    if (image && !image.split('/')[0].includes('.')) env.MINIFLARE_CONTAINER_EGRESS_IMAGE = 'docker.io/' + image;
  }
  console.log(`Containers run with Podman, through ${socket}`);
}
const wrangler = spawn('npx', ['wrangler', 'dev', ...process.argv.slice(2)], { stdio: 'inherit', env });
const stop = signal => { wrangler.kill(signal); service?.kill(); };
process.on('SIGINT', () => stop('SIGINT'));
process.on('SIGTERM', () => stop('SIGTERM'));
wrangler.on('exit', code => { service?.kill(); process.exit(code ?? 0); });
