// Recording sound into a file that grows as it is heard (`stream FILE`): Opus in WebM, which
// every browser records. The authority makes the file at once, empty, readable while it grows,
// with chunks smaller than an upload's (api::RECORDING_CHUNK); this page holds it as it holds an
// upload (upload.mjs), and sends each chunk in order as soon as it is whole. Stopping sends the
// rest as the last, short, chunk and completes the file. If the page goes, the authority keeps
// what arrived (api::RECORDING_IDLE).
const HEARTBEAT = 5000;
const RETRIES = 3;
export const RECORDED = 'audio/webm;codecs=opus';

async function json(response) {
  let body = {}; try { body = await response.json(); } catch {}
  if (!response.ok) { const error = new Error(body.error || `Request failed (${response.status})`); error.status = response.status; throw error; }
  return body;
}
// `bytes`, in the unit they make at least one of.
const size = bytes => bytes < 1e3 ? `${bytes} B` : bytes < 1e6 ? `${(bytes / 1e3).toFixed(1)} KB` : `${(bytes / 1e6).toFixed(1)} MB`;
const clock = seconds => `${String(Math.floor(seconds / 60)).padStart(2, '0')}:${String(Math.floor(seconds % 60)).padStart(2, '0')}`;

/** Record `microphone`, a MediaStream, into a new file at `destination` until `signal` stops it,
 * reporting progress; `begun` hears once the file is there. Settles with what was saved. */
export async function record(destination, microphone, { progress = () => {}, begun: there = () => {}, signal }) {
  if (!window.MediaRecorder?.isTypeSupported(RECORDED)) throw new Error('This browser cannot record Opus in WebM');
  const rust = window.terminalUploads, whole = rust.hasher();
  const begun = await json(await fetch('/api/uploads?path=' + encodeURIComponent(destination), { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ record: true }) }));
  const held = `id=${encodeURIComponent(begun.id)}&hold=${encodeURIComponent(begun.hold)}`;
  const chunkSize = begun.chunk_size;
  there(begun);
  let failure = null, sent = 0, index = 0, pending = new Uint8Array(0);
  // Chunks go one at a time, in order: the file is what arrived, from its start.
  let queue = Promise.resolve();
  const send = bytes => {
    whole.update(bytes);
    const at = index++;
    queue = queue.then(async () => {
      for (let attempt = 0; ; attempt++) {
        // Failed meanwhile (the hold lost, say): nothing more goes.
        if (failure) return;
        try {
          const response = await fetch(`/api/uploads/chunk?id=${encodeURIComponent(begun.id)}&index=${at}`, { method: 'PUT', headers: { 'X-Chunk-SHA256': rust.sha256(bytes), 'X-Upload-Hold': begun.hold }, body: bytes });
          await json(response); sent += bytes.length; return;
        } catch (error) {
          // Refused, it is over. Unreachable, it goes again, for as long as the network is down,
          // while what is recorded meanwhile waits its turn; once stopped, a few times more.
          if ((error.status >= 400 && error.status < 500) || (signal.aborted && attempt + 1 >= RETRIES)) { failure = error; return; }
          await new Promise(resolve => setTimeout(resolve, Math.min(10000, 1000 * 2 ** attempt)));
        }
      }
    });
  };
  const recorder = new MediaRecorder(microphone, { mimeType: RECORDED, audioBitsPerSecond: 64000 });
  const stopped = new Promise(resolve => { recorder.onstop = resolve; });
  // Read in the order recorded, as each read waits.
  let reading = Promise.resolve();
  recorder.ondataavailable = event => {
    reading = reading.then(async () => {
      const bytes = new Uint8Array(await event.data.arrayBuffer());
      const joined = new Uint8Array(pending.length + bytes.length); joined.set(pending); joined.set(bytes, pending.length);
      pending = joined;
      while (pending.length >= chunkSize) { send(pending.slice(0, chunkSize)); pending = pending.slice(chunkSize); }
    });
  };
  const started = performance.now();
  const say = () => progress(`● ${clock((performance.now() - started) / 1000)} · ${size(sent + pending.length)}`);
  const ticking = setInterval(say, 1000); say();
  // Still here, while it records: the page keeps its hold.
  const heartbeat = setInterval(async () => {
    try { await json(await fetch('/api/uploads/hold?' + held, { method: 'POST' })); }
    catch (error) { if ([401, 403, 409].includes(error.status)) failure ??= error; }
  }, HEARTBEAT);
  try {
    recorder.start(1000);
    // Until stopped, or a chunk or the hold fails.
    await new Promise(resolve => { const check = setInterval(() => { if (failure || signal.aborted) { clearInterval(check); resolve(); } }, 250); });
    if (recorder.state !== 'inactive') recorder.stop();
    await stopped; await reading;
    // What is left is the last chunk, short.
    if (pending.length) { send(pending); pending = new Uint8Array(0); }
    await queue;
    if (failure) throw failure;
    const done = await json(await fetch(`/api/uploads/complete?${held}&sha256=${whole.hex()}`, { method: 'POST' }));
    return { ...done, seconds: (performance.now() - started) / 1000 };
  } finally {
    clearInterval(ticking); clearInterval(heartbeat);
    if (recorder.state !== 'inactive') recorder.stop();
  }
}
