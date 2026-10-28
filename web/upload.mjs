// Chunked, resumable uploads. A file is hashed first: its SHA-256, size and destination name the
// upload on the server, so choosing the same file again, on this device or another signed in to
// the same account, resumes it from the chunks the server already has. One page sends an upload
// at a time: it holds it, saying every few seconds that it is still there, and no other page
// resumes it until it has not for a while (api::HOLD). Each chunk carries its own SHA-256, which
// R2 checks. Others can read a file while it uploads.
const PARALLEL = 3;
const RETRIES = 3;
const HEARTBEAT = 5000;

// The uploads this page holds, which it lets go of as it closes, so that another may resume them.
const holding = new Set();
addEventListener('pagehide', () => { for (const query of holding) navigator.sendBeacon('/api/uploads/release?' + query); });

// Records of the uploads this browser made, kept before the server resumed by itself.
try { for (const key of Object.keys(localStorage)) if (key.startsWith('terminal-upload:')) localStorage.removeItem(key); } catch {}

export class Cancelled extends Error { constructor() { super('Upload cancelled'); this.name = 'Cancelled'; } }

async function json(response) {
  let body = {}; try { body = await response.json(); } catch {}
  if (!response.ok) { const error = new Error(body.error || `Request failed (${response.status})`); error.status = response.status; throw error; }
  return body;
}
// As the file panel counts it (lib.rs upload_progress): whole percents of the bytes arrived.
const percent = (done, total) => total ? Math.floor(done * 100 / total) + '%' : '100%';

// The SHA-256 of `file`, and of each of its chunks.
async function hash(file, size, progress, signal) {
  const rust = window.terminalUploads, whole = rust.hasher(), parts = [];
  const count = Math.ceil(file.size / size);
  for (let index = 0; index < count; index++) {
    if (signal.aborted) throw new Cancelled();
    const bytes = new Uint8Array(await file.slice(index * size, Math.min((index + 1) * size, file.size)).arrayBuffer());
    whole.update(bytes); parts.push(rust.sha256(bytes));
    progress('hashing ' + percent(index + 1, count));
  }
  return { sha256: whole.hex(), parts };
}

function send(id, hold, index, blob, sha256, requests) {
  return new Promise((resolve, reject) => {
    const xhr = new XMLHttpRequest(); requests.add(xhr);
    xhr.open('PUT', `/api/uploads/chunk?id=${encodeURIComponent(id)}&index=${index}`);
    xhr.setRequestHeader('X-Chunk-SHA256', sha256);
    xhr.setRequestHeader('X-Upload-Hold', hold);
    const done = () => requests.delete(xhr);
    xhr.onload = () => {
      done();
      if (xhr.status >= 200 && xhr.status < 300) return resolve();
      let message = `Chunk ${index} failed (${xhr.status})`; try { message = JSON.parse(xhr.responseText).error || message; } catch {}
      const error = new Error(message); error.status = xhr.status; reject(error);
    };
    xhr.onerror = () => { done(); reject(new Error('Network connection failed')); };
    xhr.onabort = () => { done(); reject(new Cancelled()); };
    xhr.send(blob);
  });
}
// Send a chunk, again after a moment if the network or the server failed; a refusal is final.
// `halted` says whether the upload stopped meanwhile, as another chunk failed.
async function sendWithRetries(id, hold, index, blob, sha256, requests, signal, halted) {
  for (let attempt = 0; ; attempt++) {
    try { return await send(id, hold, index, blob, sha256, requests); }
    catch (error) {
      if (signal.aborted || error instanceof Cancelled) throw new Cancelled();
      if (attempt + 1 >= RETRIES || (error.status >= 400 && error.status < 500 && error.status !== 400)) throw error;
      await new Promise(resolve => setTimeout(resolve, 1000 * 2 ** attempt));
      if (halted()) throw new Cancelled();
    }
  }
}

/** Upload `file` to `destination`, reporting progress, until done or `signal` cancels it. */
export async function uploadFile(file, destination, { progress = () => {}, signal = new AbortController().signal } = {}) {
  const size = window.terminalUploads.chunkSize;
  const count = Math.ceil(file.size / size);
  const { sha256, parts } = await hash(file, size, progress, signal);
  if (signal.aborted) throw new Cancelled();
  // A new upload, or the same file's, resumed: the server refuses one another page is sending.
  const upload = await json(await fetch('/api/uploads?path=' + encodeURIComponent(destination), { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ size: file.size, sha256 }) }));
  const id = encodeURIComponent(upload.id), hold = upload.hold || '';
  const held = `id=${id}&hold=${encodeURIComponent(hold)}`;
  const committed = new Set(upload.committed || []);
  const resumed = committed.size;
  const length = index => Math.min(size, file.size - index * size);
  let arrived = 0; committed.forEach(index => { arrived += length(index); });
  progress(percent(arrived, file.size));
  const pending = []; for (let index = 0; index < count; index++) if (!committed.has(index)) pending.push(index);
  // Once one chunk fails for good, the others stop too, rather than go on sending after the
  // upload has been reported as failed.
  let halted = false, failure;
  const requests = new Set(), abort = () => { halted = true; requests.forEach(xhr => xhr.abort()); };
  signal.addEventListener('abort', abort, { once: true });
  holding.add(held);
  // Still here, while it sends: the page keeps its hold, or stops once the upload went on
  // elsewhere or was cancelled (jobs cancel).
  const heartbeat = setInterval(async () => {
    try { await json(await fetch('/api/uploads/hold?' + held, { method: 'POST' })); }
    catch (error) { if (error.status === 409 || error.status === 403) { failure ??= error; abort(); } }
  }, HEARTBEAT);
  try {
    await Promise.all(Array.from({ length: Math.min(PARALLEL, pending.length) }, async () => {
      while (pending.length && !halted) {
        if (signal.aborted) throw new Cancelled();
        const index = pending.shift();
        try {
          await sendWithRetries(upload.id, hold, index, file.slice(index * size, Math.min((index + 1) * size, file.size)), parts[index], requests, signal, () => halted);
        } catch (error) { if (!(error instanceof Cancelled)) failure ??= error; abort(); throw error; }
        committed.add(index); arrived += length(index);
        progress(percent(arrived, file.size));
      }
    }));
    if (signal.aborted) throw new Cancelled();
  } catch (error) {
    abort();
    // Only Cancel cancels the upload, and only the one this page holds; chunks stopped because
    // another failed keep it to resume.
    if (signal.aborted || (error instanceof Cancelled && !failure)) {
      await fetch('/api/uploads?' + held, { method: 'DELETE' }).catch(() => {});
      throw new Cancelled();
    }
    // Let go, so that choosing the file again resumes it now, here or elsewhere.
    await fetch('/api/uploads/release?' + held, { method: 'POST' }).catch(() => {});
    // Nothing to go on with here: the upload went on from another page, someone removed the
    // file or cancelled the upload, or it expired.
    const reason = failure || error;
    if (reason.status === 409) throw new Error('stopped: ' + (/another tab or device/.test(reason.message) ? reason.message : 'the file was removed, or its upload cancelled, elsewhere'));
    throw new Error(reason.message + '; choose the same file again to resume');
  } finally { clearInterval(heartbeat); holding.delete(held); signal.removeEventListener('abort', abort); }
  const done = await json(await fetch('/api/uploads/complete?' + held, { method: 'POST' }));
  return { ...done, resumed };
}
