// Chunked, resumable uploads. A file is hashed first: its SHA-256 and its destination name the
// upload in this browser's storage, so choosing the same file again resumes it from the chunks
// the server already has. Each chunk carries its own SHA-256, which R2 checks. Others can read
// a file while it uploads.
const STORE = 'terminal-upload:';
const PARALLEL = 3;
const RETRIES = 3;

export class Cancelled extends Error { constructor() { super('Upload cancelled'); this.name = 'Cancelled'; } }

// Browser storage may be unavailable (private windows, blocked site data): uploads still work,
// they only do not resume.
function recall(key) { try { return JSON.parse(localStorage.getItem(STORE + key) || 'null'); } catch { return null; } }
function remember(key, value) {
  try { if (value) localStorage.setItem(STORE + key, JSON.stringify(value)); else localStorage.removeItem(STORE + key); } catch {}
}
async function json(response) {
  let body = {}; try { body = await response.json(); } catch {}
  if (!response.ok) { const error = new Error(body.error || `Request failed (${response.status})`); error.status = response.status; throw error; }
  return body;
}
const percent = (done, total) => total ? Math.floor(done / total * 100) + '%' : '100%';

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

function send(id, index, blob, sha256, requests) {
  return new Promise((resolve, reject) => {
    const xhr = new XMLHttpRequest(); requests.add(xhr);
    xhr.open('PUT', `/api/uploads/chunk?id=${encodeURIComponent(id)}&index=${index}`);
    xhr.setRequestHeader('X-Chunk-SHA256', sha256);
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
async function sendWithRetries(id, index, blob, sha256, requests, signal, halted) {
  for (let attempt = 0; ; attempt++) {
    try { return await send(id, index, blob, sha256, requests); }
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
  const key = sha256 + ':' + destination;
  const committed = new Set();
  let upload;
  const saved = recall(key);
  if (saved) {
    try {
      const status = await json(await fetch('/api/uploads?id=' + encodeURIComponent(saved.id)));
      if (status.size === file.size && status.sha256 === sha256 && status.path === destination) { upload = status; status.committed.forEach(i => committed.add(i)); }
    } catch {}
    if (!upload) remember(key, null);
  }
  if (signal.aborted) throw new Cancelled();
  // Without a record, the server still resumes the same file to the same path.
  if (!upload) {
    upload = await json(await fetch('/api/uploads?path=' + encodeURIComponent(destination), { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ size: file.size, sha256 }) }));
    (upload.committed || []).forEach(i => committed.add(i));
  }
  const resumed = committed.size;
  const next = () => { let index = 0; while (committed.has(index)) index++; return index; };
  remember(key, { id: upload.id, next: next() });
  const pending = []; for (let index = 0; index < count; index++) if (!committed.has(index)) pending.push(index);
  progress(percent(committed.size, count));
  // Once one chunk fails for good, the others stop too, rather than go on sending after the
  // upload has been reported as failed.
  let halted = false, failure;
  const requests = new Set(), abort = () => { halted = true; requests.forEach(xhr => xhr.abort()); };
  signal.addEventListener('abort', abort, { once: true });
  try {
    await Promise.all(Array.from({ length: Math.min(PARALLEL, pending.length) }, async () => {
      while (pending.length && !halted) {
        if (signal.aborted) throw new Cancelled();
        const index = pending.shift();
        try {
          await sendWithRetries(upload.id, index, file.slice(index * size, Math.min((index + 1) * size, file.size)), parts[index], requests, signal, () => halted);
        } catch (error) { if (!(error instanceof Cancelled)) failure ??= error; abort(); throw error; }
        committed.add(index); remember(key, { id: upload.id, next: next() });
        progress(percent(committed.size, count));
      }
    }));
    if (signal.aborted) throw new Cancelled();
  } catch (error) {
    abort();
    // Only Cancel cancels the upload; chunks stopped because another failed keep it to resume.
    if (signal.aborted || (error instanceof Cancelled && !failure)) {
      await fetch('/api/uploads?id=' + encodeURIComponent(upload.id), { method: 'DELETE' }).catch(() => {});
      remember(key, null); throw new Cancelled();
    }
    // The upload is gone on the server: someone removed the file, cancelled the upload, or it
    // expired. There is nothing to resume.
    if ((failure || error).status === 409) { remember(key, null); throw new Error('stopped: the file was removed, or its upload cancelled, elsewhere'); }
    throw new Error((failure || error).message + '; choose the same file again to resume');
  } finally { signal.removeEventListener('abort', abort); }
  const done = await json(await fetch('/api/uploads/complete?id=' + encodeURIComponent(upload.id), { method: 'POST' }));
  remember(key, null);
  return { ...done, resumed };
}
