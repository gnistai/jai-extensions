// gzip: CRC32, a strict single-member decoder with a pure inflater (RFC 1951/1952),
// and the encoder for the file form. Pure TypeScript so the spec's
// single-member, trailing-byte and output-bound rules can be enforced.
import type { JaiDocument } from './chars.ts';
import { canonicalJSON } from './json.ts';

const CRC_TABLE = (() => {
  const t = new Uint32Array(256);
  for (let n = 0; n < 256; n++) {
    let c = n;
    for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    t[n] = c >>> 0;
  }
  return t;
})();

export function crc32(bytes: Uint8Array): number {
  let c = 0xffffffff;
  for (let i = 0; i < bytes.length; i++) c = (CRC_TABLE[(c ^ (bytes[i] ?? 0)) & 0xff] ?? 0) ^ (c >>> 8);
  return (c ^ 0xffffffff) >>> 0;
}

export class InflateError extends Error {}

interface Tree { count: Uint16Array; symbol: Uint16Array }

const LBASE = [3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131, 163, 195, 227, 258];
const LEXT = [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0];
const DBASE = [1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537, 2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577];
const DEXT = [0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13];
const ORDER = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];

type TreeKind = 'codes' | 'lens' | 'dists' | 'fixed';

/** Canonical Huffman decoding table; rejects over-subscribed and incomplete codes as zlib's inflate_table does. */
function buildTree(lengths: ArrayLike<number>, kind: TreeKind): Tree {
  const count = new Uint16Array(16), symbol = new Uint16Array(lengths.length);
  let max = 0;
  for (let s = 0; s < lengths.length; s++) { const l = lengths[s] ?? 0; count[l] = (count[l] ?? 0) + 1; if (l > max) max = l; }
  count[0] = 0;
  if (max > 0 && kind !== 'fixed') { // zlib builds the fixed tables without these checks
    let left = 1;
    for (let len = 1; len < 16; len++) { left <<= 1; left -= count[len] ?? 0; if (left < 0) throw new InflateError('over-subscribed'); }
    if (left > 0 && (kind === 'codes' || max !== 1)) throw new InflateError('incomplete');
  }
  const offs = new Uint16Array(16);
  for (let i = 1; i < 16; i++) offs[i] = (offs[i - 1] ?? 0) + (count[i - 1] ?? 0);
  for (let s = 0; s < lengths.length; s++) {
    const l = lengths[s] ?? 0;
    if (l) { symbol[offs[l] ?? 0] = s; offs[l] = (offs[l] ?? 0) + 1; }
  }
  return { count, symbol };
}

let fixed: [Tree, Tree] | null = null;
function fixedTrees(): [Tree, Tree] {
  if (!fixed) {
    const l = new Uint8Array(288);
    l.fill(8, 0, 144); l.fill(9, 144, 256); l.fill(7, 256, 280); l.fill(8, 280, 288);
    fixed = [buildTree(l, 'fixed'), buildTree(new Uint8Array(30).fill(5), 'fixed')];
  }
  return fixed;
}

/** Inflate a raw deflate stream starting at `start`. Throws InflateError. */
export function inflateRaw(input: Uint8Array, start = 0, maxOutput = 16 * 1024 * 1024): { output: Uint8Array; consumed: number } {
  let pos = start, bitBuf = 0, bitCnt = 0;
  let out = new Uint8Array(Math.min(maxOutput, Math.max(1024, input.length * 4)));
  let outLen = 0;
  const ensure = (n: number): void => {
    if (outLen + n > maxOutput) throw new InflateError('bound');
    if (outLen + n > out.length) {
      const bigger = new Uint8Array(Math.min(maxOutput, Math.max(out.length * 2, outLen + n)));
      bigger.set(out.subarray(0, outLen)); out = bigger;
    }
  };
  const bits = (n: number): number => {
    while (bitCnt < n) {
      if (pos >= input.length) throw new InflateError('truncated');
      bitBuf |= (input[pos++] ?? 0) << bitCnt; bitCnt += 8;
    }
    const v = bitBuf & ((1 << n) - 1);
    bitBuf >>>= n; bitCnt -= n;
    return v;
  };
  const decodeSym = (t: Tree): number => {
    let code = 0, first = 0, index = 0;
    for (let len = 1; len < 16; len++) {
      code |= bits(1);
      const c = t.count[len] ?? 0;
      if (code - c < first) return t.symbol[index + (code - first)] ?? 0;
      index += c; first += c; first <<= 1; code <<= 1;
    }
    throw new InflateError('bad-code');
  };
  const inflateBlock = (lt: Tree, dt: Tree): void => {
    for (;;) {
      const sym = decodeSym(lt);
      if (sym < 256) { ensure(1); out[outLen++] = sym; }
      else if (sym === 256) return;
      else {
        const li = sym - 257;
        if (li >= 29) throw new InflateError('bad-length');
        const len = (LBASE[li] ?? 0) + bits(LEXT[li] ?? 0);
        const di = decodeSym(dt);
        if (di >= 30) throw new InflateError('bad-distance');
        const dist = (DBASE[di] ?? 0) + bits(DEXT[di] ?? 0);
        if (dist > outLen) throw new InflateError('distance-too-far');
        ensure(len);
        for (let k = 0; k < len; k++) { out[outLen] = out[outLen - dist] ?? 0; outLen++; }
      }
    }
  };
  let final: number;
  do {
    final = bits(1);
    const type = bits(2);
    if (type === 0) {
      bitBuf = 0; bitCnt = 0;
      if (pos + 4 > input.length) throw new InflateError('truncated');
      const len = (input[pos] ?? 0) | ((input[pos + 1] ?? 0) << 8), nlen = (input[pos + 2] ?? 0) | ((input[pos + 3] ?? 0) << 8);
      pos += 4;
      if ((len ^ 0xffff) !== nlen) throw new InflateError('bad-stored');
      if (pos + len > input.length) throw new InflateError('truncated');
      ensure(len); out.set(input.subarray(pos, pos + len), outLen); outLen += len; pos += len;
    } else if (type === 1) {
      const [lt, dt] = fixedTrees(); inflateBlock(lt, dt);
    } else if (type === 2) {
      const hlit = bits(5) + 257, hdist = bits(5) + 1, hclen = bits(4) + 4;
      if (hlit > 286 || hdist > 30) throw new InflateError('too-many-codes');
      const cl = new Uint8Array(19);
      for (let k = 0; k < hclen; k++) cl[ORDER[k] ?? 0] = bits(3);
      const ct = buildTree(cl, 'codes');
      const lens = new Uint8Array(hlit + hdist);
      for (let n = 0; n < hlit + hdist;) {
        const sym = decodeSym(ct);
        if (sym < 16) { lens[n++] = sym; continue; }
        let r: number, val = 0;
        if (sym === 16) { if (n === 0) throw new InflateError('bad-repeat'); val = lens[n - 1] ?? 0; r = 3 + bits(2); }
        else if (sym === 17) r = 3 + bits(3);
        else r = 11 + bits(7);
        if (n + r > hlit + hdist) throw new InflateError('bad-lengths');
        while (r--) lens[n++] = val;
      }
      if (lens[256] === 0) throw new InflateError('missing-end-of-block');
      inflateBlock(buildTree(lens.subarray(0, hlit), 'lens'), buildTree(lens.subarray(hlit), 'dists'));
    } else throw new InflateError('bad-block-type');
  } while (!final);
  pos -= bitCnt >> 3; // give back whole bytes pulled into the bit buffer but unused
  return { output: out.subarray(0, outLen), consumed: pos - start };
}

const u32 = (b: Uint8Array, p: number): number => ((b[p] ?? 0) | ((b[p + 1] ?? 0) << 8) | ((b[p + 2] ?? 0) << 16) | ((b[p + 3] ?? 0) << 24)) >>> 0;

/** Decode exactly one gzip member; anything after it is an error. Throws InflateError. */
export function gunzipOne(bytes: Uint8Array, maxOutput = 16 * 1024 * 1024): { content: Uint8Array } {
  if (bytes.length < 18 || bytes[0] !== 0x1f || bytes[1] !== 0x8b) throw new InflateError('not-gzip');
  if (bytes[2] !== 8) throw new InflateError('bad-method');
  const flg = bytes[3] ?? 0;
  if (flg & 0xe0) throw new InflateError('reserved-flag');
  let p = 10;
  if (flg & 0x04) { if (p + 2 > bytes.length) throw new InflateError('truncated'); const xlen = (bytes[p] ?? 0) | ((bytes[p + 1] ?? 0) << 8); p += 2 + xlen; }
  if (flg & 0x08) { while (p < bytes.length && bytes[p] !== 0) p++; p++; }
  if (flg & 0x10) { while (p < bytes.length && bytes[p] !== 0) p++; p++; }
  if (flg & 0x02) {
    if (p + 2 > bytes.length) throw new InflateError('truncated');
    const hcrc = (bytes[p] ?? 0) | ((bytes[p + 1] ?? 0) << 8);
    if ((crc32(bytes.subarray(0, p)) & 0xffff) !== hcrc) throw new InflateError('bad-header-crc');
    p += 2;
  }
  if (p > bytes.length) throw new InflateError('truncated');
  const { output, consumed } = inflateRaw(bytes, p, maxOutput);
  p += consumed;
  if (p + 8 > bytes.length) throw new InflateError('truncated');
  const crc = u32(bytes, p), isize = u32(bytes, p + 4);
  p += 8;
  if (crc !== crc32(output)) throw new InflateError('bad-crc');
  if (isize !== (output.length >>> 0)) throw new InflateError('bad-isize');
  if (p !== bytes.length) throw new InflateError('trailing-bytes');
  return { content: output };
}

async function readAll(stream: ReadableStream<Uint8Array>): Promise<Uint8Array> {
  const chunks: Uint8Array[] = [];
  const r = stream.getReader();
  for (;;) { const { value, done } = await r.read(); if (done) break; if (value) chunks.push(value); }
  const out = new Uint8Array(chunks.reduce((a, c) => a + c.length, 0));
  let o = 0;
  for (const c of chunks) { out.set(c, o); o += c.length; }
  return out;
}

function storedBlocks(bytes: Uint8Array): Uint8Array {
  const blocks: Uint8Array[] = [];
  let i = 0;
  do {
    const chunk = bytes.subarray(i, Math.min(bytes.length, i + 65535));
    const final = i + 65535 >= bytes.length ? 1 : 0;
    const b = new Uint8Array(5 + chunk.length);
    b[0] = final; b[1] = chunk.length & 0xff; b[2] = chunk.length >> 8; b[3] = ~chunk.length & 0xff; b[4] = (~chunk.length >> 8) & 0xff;
    b.set(chunk, 5); blocks.push(b);
    i += 65535;
  } while (i < bytes.length);
  const out = new Uint8Array(blocks.reduce((a, c) => a + c.length, 0));
  let o = 0;
  for (const c of blocks) { out.set(c, o); o += c.length; }
  return out;
}

async function deflateRawBytes(bytes: Uint8Array): Promise<Uint8Array> {
  const CS = (globalThis as { CompressionStream?: new (format: string) => { writable: WritableStream<Uint8Array>; readable: ReadableStream<Uint8Array> } }).CompressionStream;
  if (typeof CS === 'function') {
    try {
      const cs = new CS('deflate-raw');
      const w = cs.writable.getWriter();
      void w.write(bytes); void w.close();
      return await readAll(cs.readable);
    } catch { /* fall through */ }
  }
  try {
    const zlib = (await import('node:zlib' as string)) as { deflateRawSync(b: Uint8Array, o: { level: number }): Uint8Array };
    return new Uint8Array(zlib.deflateRawSync(bytes, { level: 9 }));
  } catch { /* fall through */ }
  return storedBlocks(bytes);
}

/** Encode a document as the file form: one gzip member, MTIME 0, no FNAME/FCOMMENT, OS 255. */
export async function encode(doc: JaiDocument): Promise<Uint8Array> {
  const content = new TextEncoder().encode(canonicalJSON(doc));
  const deflated = await deflateRawBytes(content);
  const out = new Uint8Array(10 + deflated.length + 8);
  out.set([0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 255], 0);
  out.set(deflated, 10);
  let p = 10 + deflated.length;
  const crc = crc32(content), n = content.length >>> 0;
  for (const v of [crc, n]) { out[p++] = v & 0xff; out[p++] = (v >>> 8) & 0xff; out[p++] = (v >>> 16) & 0xff; out[p++] = (v >>> 24) & 0xff; }
  return out;
}
