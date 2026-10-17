// The type and pixel size an image states in its header, for the raster types
// a `preview` may have (PNG, JPEG, WebP, GIF, AVIF). Nothing is decoded and no
// platform API is used, so it works in JAI Studio's QuickJS host as in Node and
// browsers. Orientation metadata (JPEG Exif, AVIF irot) is not applied.
import { parseDataURL } from '../jai/decode.ts';

export interface ImageSize { width: number; height: number }

/**
 * The width/height of a document's `preview`: the render ratio when there is
 * neither a governing ratio nor an `aspect` (spec: Size and aspect). The
 * preview is a raster `data:` URL and is never fetched; null when there is
 * none or its size cannot be read.
 */
export function previewRatio(preview: string | null): number | null {
  const d = preview ? parseDataURL(preview) : null;
  const size = d ? imageSize(d.bytes) : null;
  return size ? size.width / size.height : null;
}

/** Identify PNG, JPEG, WebP, GIF or AVIF by signature; null otherwise. */
export function sniffImage(b: Uint8Array): string | null {
  const at = (i: number, ...v: number[]): boolean => v.every((x, k) => b[i + k] === x);
  if (at(0, 0x89, 0x50, 0x4e, 0x47)) return 'image/png';
  if (at(0, 0xff, 0xd8, 0xff)) return 'image/jpeg';
  if (at(0, 0x52, 0x49, 0x46, 0x46) && at(8, 0x57, 0x45, 0x42, 0x50)) return 'image/webp';
  if (at(0, 0x47, 0x49, 0x46, 0x38)) return 'image/gif';
  if (at(4, 0x66, 0x74, 0x79, 0x70) && (at(8, 0x61, 0x76, 0x69, 0x66) || at(8, 0x61, 0x76, 0x69, 0x73))) return 'image/avif';
  return null;
}

/** Width and height from the header of a PNG, JPEG, WebP, GIF or AVIF; null for anything else or a header cut short. */
export function imageSize(b: Uint8Array): ImageSize | null {
  const v = new DataView(b.buffer, b.byteOffset, b.byteLength);
  let size: ImageSize | null = null;
  try {
    switch (sniffImage(b)) {
      case 'image/png': size = tag(b, 12) === 'IHDR' ? { width: v.getUint32(16), height: v.getUint32(20) } : null; break;
      case 'image/gif': size = { width: v.getUint16(6, true), height: v.getUint16(8, true) }; break;
      case 'image/webp': size = webpSize(b, v); break;
      case 'image/jpeg': size = jpegSize(v); break;
      case 'image/avif': size = avifSize(b, v); break;
    }
  } catch {
    return null; // DataView's RangeError: the header ends early
  }
  return size && size.width > 0 && size.height > 0 ? size : null;
}

const tag = (b: Uint8Array, i: number): string => String.fromCharCode(b[i] ?? 0, b[i + 1] ?? 0, b[i + 2] ?? 0, b[i + 3] ?? 0);

function webpSize(b: Uint8Array, v: DataView): ImageSize | null {
  switch (tag(b, 12)) {
    case 'VP8 ': // lossy: a key frame's start code, then two 14-bit sizes
      return b[23] === 0x9d && b[24] === 0x01 && b[25] === 0x2a ? { width: v.getUint16(26, true) & 0x3fff, height: v.getUint16(28, true) & 0x3fff } : null;
    case 'VP8L': { // lossless: a signature byte, then two 14-bit sizes minus one
      if (b[20] !== 0x2f) return null;
      const bits = v.getUint32(21, true);
      return { width: (bits & 0x3fff) + 1, height: ((bits >>> 14) & 0x3fff) + 1 };
    }
    case 'VP8X': // extended: the canvas size minus one, 24 bits each
      return { width: (v.getUint32(24, true) & 0xffffff) + 1, height: (v.getUint32(26, true) >>> 8) + 1 };
    default:
      return null;
  }
}

/** The first frame header (SOF0 to SOF15) before the scan data. */
function jpegSize(v: DataView): ImageSize | null {
  for (let i = 2; ;) {
    if (v.getUint8(i) !== 0xff) return null;
    const marker = v.getUint8(i + 1);
    if (marker === 0xff) { i++; continue; } // a fill byte
    if (marker === 0x01 || (marker >= 0xd0 && marker <= 0xd8)) { i += 2; continue; } // markers without a length
    if (marker === 0xd9 || marker === 0xda) return null; // the end of the image, or scan data
    // DHT (C4), JPG (C8) and DAC (CC) share the frame headers' range.
    if (marker >= 0xc0 && marker <= 0xcf && marker !== 0xc4 && marker !== 0xc8 && marker !== 0xcc) return { width: v.getUint16(i + 7), height: v.getUint16(i + 5) };
    i += 2 + v.getUint16(i + 2);
  }
}

/** The boxes of an ISO BMFF file in [start, end): type and contents. */
function* boxes(b: Uint8Array, v: DataView, start: number, end: number): Generator<{ type: string; start: number; end: number }> {
  for (let i = start; i + 8 <= end;) {
    let size = v.getUint32(i), head = 8;
    if (size === 1) { size = Number(v.getBigUint64(i + 8)); head = 16; } else if (size === 0) size = end - i;
    if (size < head || i + size > end) return;
    yield { type: tag(b, i + 4), start: i + head, end: i + size };
    i += size;
  }
}

/**
 * The largest `ispe` (image spatial extents) in meta/iprp/ipco: the primary
 * image's, since the other items a file may hold (grid tiles, alpha and depth
 * planes, thumbnails) are never larger.
 */
function avifSize(b: Uint8Array, v: DataView): ImageSize | null {
  const child = (parent: { start: number; end: number } | null, type: string, skip = 0): { start: number; end: number } | null => {
    if (!parent) return null;
    for (const x of boxes(b, v, parent.start + skip, parent.end)) if (x.type === type) return x;
    return null;
  };
  // meta is a full box: a version and flags come before its children.
  const ipco = child(child(child({ start: 0, end: b.length }, 'meta'), 'iprp', 4), 'ipco');
  let best: ImageSize | null = null;
  for (const p of ipco ? boxes(b, v, ipco.start, ipco.end) : []) {
    if (p.type !== 'ispe') continue;
    const s = { width: v.getUint32(p.start + 4), height: v.getUint32(p.start + 8) };
    if (!best || s.width * s.height > best.width * best.height) best = s;
  }
  return best;
}
