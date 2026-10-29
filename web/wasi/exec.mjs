// exec(2), as the page does it: a program of /bin, built for wasm32-wasip1, runs as a process of
// its own (web/wasi/process.js, a Worker), with its arguments, environment, standard input and
// the terminal. The binary is compiled once, and each process instantiates it anew.
const BINARY = '/bin/coreutils.wasm';
let compiled = null;
const compile = () => compiled ??= WebAssembly.compileStreaming(fetch(BINARY)).catch(err => { compiled = null; throw err; });

// How many columns the terminal's transcript has, for programs that lay out their output.
function columns() {
  const transcript = document.querySelector('.transcript') ?? document.body;
  // A line of the transcript's own, measured.
  const probe = document.createElement('pre');
  probe.textContent = '0'.repeat(10);
  probe.style.cssText = 'position:absolute;visibility:hidden;white-space:pre;width:auto';
  transcript.append(probe);
  const width = probe.getBoundingClientRect().width / 10 || 12;
  probe.remove();
  const style = getComputedStyle(transcript);
  const inner = transcript.clientWidth - parseFloat(style.paddingLeft || 0) - parseFloat(style.paddingRight || 0);
  return Math.max(20, Math.floor((inner || 800) / width));
}

// Run `argv` as `options` say: {cwd, home, user, stdin (a string, or none for the terminal),
// sudo, umask, tty (whether it writes to the terminal), cancelled (asked now and then)}. Its exit
// status and what it wrote, as text.
export async function exec(argv, { cwd = '/', home = '/', user = 'guest', stdin = null, sudo = false, umask = 0o022, tty = false, cancelled = () => false } = {}) {
  const module = await compile();
  const env = {
    PWD: cwd, HOME: home, USER: user, LOGNAME: user, PATH: '/bin', SHELL: '/bin/sh', LANG: 'C.UTF-8',
    TERM: tty ? 'xterm-256color' : 'dumb', COLUMNS: String(columns()),
    TZ: Intl.DateTimeFormat().resolvedOptions().timeZone || 'UTC',
  };
  const worker = new Worker('/wasi-process.js');
  const decoders = { 1: new TextDecoder(), 2: new TextDecoder() };
  const text = { 1: '', 2: '' };
  return new Promise(resolve => {
    let truncated = false;
    const done = status => {
      clearInterval(watch);
      worker.terminate();
      resolve({ status, stdout: text[1] + decoders[1].decode(), stderr: text[2] + decoders[2].decode(), truncated });
    };
    // Ctrl+C: the process is killed, as SIGINT would.
    const watch = setInterval(() => { if (cancelled()) done(130); }, 100);
    worker.onmessage = ({ data }) => {
      if (data.out) text[data.fd] += decoders[data.fd].decode(data.out, { stream: true });
      else if (data.truncated) truncated = true;
      else if ('exit' in data) done(data.exit);
    };
    worker.onerror = event => { text[2] += `${argv[0]}: ${event.message}\n`; done(126); };
    worker.postMessage({ module, argv, env, stdin: stdin == null ? null : new TextEncoder().encode(stdin), umask, sudo, tty });
  });
}
