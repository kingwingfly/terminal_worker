import { test } from 'node:test';
import assert from 'node:assert/strict';
import { inspectFaststart } from '../web/faststart.mjs';
function box(type, data = Buffer.alloc(0), extended = false) {
  const head = Buffer.alloc(extended ? 16 : 8); head.writeUInt32BE(extended ? 1 : head.length + data.length); head.write(type, 4);
  if (extended) head.writeBigUInt64BE(BigInt(head.length + data.length), 8);
  return Buffer.concat([head, data]);
}
test('faststart recognizes atom order, including large extended-size boxes', async () => {
  const ftyp = box('ftyp', Buffer.alloc(12)), moov = box('moov', Buffer.alloc(20), true), mdat = box('mdat', Buffer.alloc(100));
  assert.equal((await inspectFaststart(new Blob([ftyp, moov, mdat]))).faststart, true);
  assert.equal((await inspectFaststart(new Blob([ftyp, mdat, moov]))).faststart, false);
});
test('rejects truncated, oversized and incomplete containers instead of claiming faststart', async () => {
  const bad = box('moov'); bad.writeUInt32BE(10000);
  for (const bytes of [bad, Buffer.alloc(4), box('ftyp'), Buffer.from('not an mp4 video')]) {
    await assert.rejects(inspectFaststart(new Blob([bytes])));
  }
});
