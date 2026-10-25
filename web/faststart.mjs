// Read only ISO BMFF box headers, including 64-bit sizes; never buffer the video.
export async function inspectFaststart(file) {
  let offset = 0, moov = -1, mdat = -1, count = 0;
  while (offset < file.size) {
    if (++count > 10000) throw new Error('Too many MP4 boxes to inspect');
    const bytes = new Uint8Array(await file.slice(offset, offset + 16).arrayBuffer());
    if (bytes.length < 8) throw new Error('Incomplete MP4 box header');
    const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
    let length = view.getUint32(0), header = 8;
    const type = String.fromCharCode(...bytes.subarray(4, 8));
    if (length === 1) {
      if (bytes.length < 16) throw new Error('Incomplete extended MP4 box');
      const big = view.getBigUint64(8);
      if (big > BigInt(Number.MAX_SAFE_INTEGER)) throw new Error('MP4 box is too large');
      length = Number(big); header = 16;
    } else if (length === 0) length = file.size - offset;
    if (length < header || offset + length > file.size) throw new Error('Invalid MP4 box length');
    if (type === 'moov' && moov < 0) moov = offset;
    if (type === 'mdat' && mdat < 0) mdat = offset;
    offset += length;
  }
  if (moov < 0 || mdat < 0) throw new Error('Complete moov / mdat boxes were not found; cannot verify faststart');
  return { faststart: moov < mdat, moov, mdat };
}
