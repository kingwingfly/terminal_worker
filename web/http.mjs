// What a request of the terminal's API answered: its JSON, or, refused, an error that carries the
// API's message and the status, which callers tell apart (a 409 from a 404, say).
export async function json(response) {
  let body = {}; try { body = await response.json(); } catch {}
  if (!response.ok) { const error = new Error(body.error || `Request failed (${response.status})`); error.status = response.status; throw error; }
  return body;
}

// System calls (owner's design, 2026-10-29: the page is user space): `calls`, one batch, each
// answered on its own, `{ok}` or `{err: {errno, detail}}`; `options` are the process's (umask,
// sudo).
export async function sys(calls, options = {}) {
  const response = await fetch('/api/sys', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ calls, ...options }) });
  return (await json(response)).results;
}
