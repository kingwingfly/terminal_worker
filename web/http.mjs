// What a request of the terminal's API answered: its JSON, or, refused, an error that carries the
// API's message and the status, which callers tell apart (a 409 from a 404, say).
export async function json(response) {
  let body = {}; try { body = await response.json(); } catch {}
  if (!response.ok) { const error = new Error(body.error || `Request failed (${response.status})`); error.status = response.status; throw error; }
  return body;
}
