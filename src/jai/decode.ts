// The decoding algorithm for the file form, and data:image/jai URLs (the text form).
import { MEDIA_TYPE, asciiLower, diag, isAsciiSpace, trimBy, type Diagnostic, type JaiDocument } from './chars.ts';
import { createDocument } from './document.ts';
import { gunzipOne } from './gzip.ts';
import { readJSON } from './json.ts';
import { parse, serialize, type ParseResult } from './text.ts';

export interface DecodeResult { document: JaiDocument; diagnostics: Diagnostic[] }

/** Strict UTF-8 decoding that keeps a leading U+FEFF; null for bytes that are not well-formed UTF-8. */
function utf8OrNull(bytes: Uint8Array): string | null {
  try { return new TextDecoder('utf-8', { fatal: true, ignoreBOM: true }).decode(bytes); } catch { return null; }
}

/**
 * Decoding algorithm for the file form (spec §Decoding): the bytes of a .jai file or
 * resource are exactly one gzip member holding a UTF-8 JSON text with an object at the
 * top level. Nothing else is accepted and nothing is sniffed.
 */
export function decode(bytes: Uint8Array, { maxInflate = 16 * 1024 * 1024 }: { maxInflate?: number } = {}): DecodeResult {
  const diagnostics: Diagnostic[] = [];
  const empty = (code: string, reason: string): DecodeResult => { diagnostics.push(diag(code, null, { reason })); return { document: createDocument(), diagnostics }; };
  let content: Uint8Array;
  try { content = gunzipOne(bytes, maxInflate).content; } catch (e) { return empty('malformed-container', (e as Error).message); }
  const text = utf8OrNull(content);
  if (text === null) return empty('invalid-json', 'not-utf8');
  const r = readJSON(text);
  diagnostics.push(...r.diagnostics);
  return { document: r.document, diagnostics };
}

/** Lenient byte-level percent-decode (URL Standard): never fails. */
export function percentDecode(s: string): Uint8Array {
  const bytes = new TextEncoder().encode(s);
  const out = new Uint8Array(bytes.length);
  let o = 0;
  const hex = (c: number): number => (c >= 48 && c <= 57 ? c - 48 : c >= 65 && c <= 70 ? c - 55 : c >= 97 && c <= 102 ? c - 87 : -1);
  for (let i = 0; i < bytes.length; i++) {
    const b = bytes[i] ?? 0;
    if (b === 0x25 && i + 2 < bytes.length) {
      const a = hex(bytes[i + 1] ?? 0), c = hex(bytes[i + 2] ?? 0);
      if (a >= 0 && c >= 0) { out[o++] = (a << 4) | c; i += 2; continue; }
    }
    out[o++] = b;
  }
  return out.subarray(0, o);
}

function forgivingBase64(input: string): Uint8Array | null {
  let s = input.replace(/[\t\n\f\r ]/g, '');
  if (s.length % 4 === 0) s = s.replace(/={1,2}$/, '');
  if (s.length % 4 === 1 || /[^A-Za-z0-9+/]/.test(s)) return null;
  const A = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/';
  const out = new Uint8Array(Math.floor((s.length * 3) / 4));
  let o = 0, buf = 0, n = 0;
  for (const c of s) { buf = (buf << 6) | A.indexOf(c); n += 6; if (n >= 8) { n -= 8; out[o++] = (buf >> n) & 0xff; } }
  return out.subarray(0, o);
}

export interface DataURL { mime: string; essence: string; bytes: Uint8Array }

/** Essence of a MIME type as the Fetch Standard's MIME type parser computes it; text/plain on failure. */
function fetchMimeEssence(mimeType: string): string {
  const t = trimBy(mimeType, c => c === 0x09 || c === 0x0a || c === 0x0d || c === 0x20); // HTTP whitespace
  const m = /^([!#$%&'*+.^_`|~0-9A-Za-z-]+)\/([!#$%&'*+.^_`|~0-9A-Za-z-]+)[\t\n\r ]*(?:;|$)/.exec(t);
  return m ? asciiLower((m[1] ?? '') + '/' + (m[2] ?? '')) : 'text/plain';
}

/** data: URL processor (Fetch Standard) on the URL as the URL parser serialises it; null on failure. */
export function parseDataURL(url: string): DataURL | null {
  let href: string;
  try { href = new URL(String(url)).href; } catch { return null; }
  if (!/^data:/i.test(href)) return null;
  let s = href.slice(5);
  const hash = s.indexOf('#'); if (hash >= 0) s = s.slice(0, hash);
  const comma = s.indexOf(','); if (comma < 0) return null;
  let mimeType = trimBy(s.slice(0, comma), isAsciiSpace);
  let bytes = percentDecode(s.slice(comma + 1));
  const m = /;[ ]*base64$/i.exec(mimeType);
  if (m) {
    const b = forgivingBase64(new TextDecoder().decode(bytes));
    if (b === null) return null;
    bytes = b; mimeType = mimeType.slice(0, m.index);
  }
  if (mimeType.startsWith(';')) mimeType = 'text/plain' + mimeType;
  return { mime: mimeType, essence: fetchMimeEssence(mimeType), bytes };
}

/**
 * Decode a data:image/jai URL (spec §Decoding): the body, as the data: URL processor
 * leaves it (percent-decoded, or base64-decoded), is the text form. Null when the URL is
 * not a data:image/jai URL or the processor fails; the empty document with `undecodable`
 * when the body exceeds `maxText` bytes, is not well-formed UTF-8 or contains a 00 byte.
 */
export function decodeDataURL(url: string, { maxText = 1024 * 1024 }: { maxText?: number } = {}): (DecodeResult & Partial<Pick<ParseResult, 'info'>>) | null {
  const d = parseDataURL(url);
  if (!d || d.essence !== MEDIA_TYPE) return null;
  const undecodable = (reason: string): DecodeResult => ({ document: createDocument(), diagnostics: [diag('undecodable', null, { reason })] });
  if (d.bytes.length > maxText) return undecodable('bound');
  if (d.bytes.includes(0)) return undecodable('nul');
  const text = utf8OrNull(d.bytes);
  return text === null ? undecodable('not-utf8') : parse(text);
}

/** Percent-encode text for the body of a data:image/jai, URL (spec encoding profile). */
export function encodeURLBody(text: string): string {
  const cps = Array.from(text);
  return cps.map((ch, i) => {
    const c = ch.codePointAt(0) ?? 0;
    const last = i === cps.length - 1;
    return c < 0x21 || c === 0x7f || c === 0x25 || c === 0x23 || c === 0x22 || c === 0x26 || c === 0x27 || c === 0x3c || c === 0x3e || (last && c === 0x2c) || (i === 0 && c === 0x2f)
      ? '%' + c.toString(16).toUpperCase().padStart(2, '0')
      : ch;
  }).join('');
}

/** Canonical URL form: data:image/jai, + the percent-encoded canonical text. */
export function toDataURL(doc: JaiDocument): string { return 'data:image/jai,' + encodeURLBody(serialize(doc)); }
