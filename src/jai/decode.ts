// The decoding algorithm and URL carriers (data: URLs, query forms).
import { MEDIA_TYPE, asciiLower, diag, isAsciiSpace, trimBy, type Diagnostic, type JaiDocument } from './chars.ts';
import { createDocument } from './document.ts';
import { gunzipOne } from './gzip.ts';
import { readJSON } from './json.ts';
import { parse, serialize, type ParseInfo, type ParseResult } from './text.ts';

const JSON_WS = /^[\t\n\r ]*/;

export interface DecodeResult { document: JaiDocument; diagnostics: Diagnostic[]; form: 'gzip' | 'json' | 'text' | null; info?: ParseInfo }
export interface DecodeOptions { inContext?: boolean; maxInflate?: number; maxText?: number }

/** Decoding algorithm (spec §Decoding). Layered, total, never falls back between forms. */
export function decode(input: Uint8Array, { inContext = true, maxInflate = 16 * 1024 * 1024, maxText = 1024 * 1024 }: DecodeOptions = {}): DecodeResult {
  const diagnostics: Diagnostic[] = [];
  const empty = (): DecodeResult => ({ document: createDocument(), diagnostics, form: null });
  let bytes = input;
  let fromGzip = false;
  if (bytes.length >= 2 && bytes[0] === 0x1f && bytes[1] === 0x8b) {
    try { bytes = gunzipOne(bytes, maxInflate).content; fromGzip = true; }
    catch (e) { diagnostics.push(diag('malformed-container', null, { reason: (e as Error).message })); return empty(); }
  }
  if (bytes.length > maxText && !fromGzip) { diagnostics.push(diag('undecodable', null, { reason: 'bound' })); return empty(); }
  // Byte input never contains 00 (binary such as headerless UTF-16 must not become a prompt).
  let text: string | null = null;
  if (!bytes.includes(0)) {
    try { text = new TextDecoder('utf-8', { fatal: true }).decode(bytes); } catch { text = null; }
  }
  if (text === null) { diagnostics.push(fromGzip ? diag('malformed-container', null, { reason: 'not-utf8' }) : diag('undecodable')); return empty(); }
  let i = 0;
  while (text.charCodeAt(i) === 0xfeff) i++;
  text = text.slice(i);
  const ws = (JSON_WS.exec(text)?.[0] ?? '').length;
  if (text.charAt(ws) === '{') {
    const after = (JSON_WS.exec(text.slice(ws + 1))?.[0] ?? '').length;
    const next = text.charAt(ws + 1 + after);
    if (next === '"' || next === '}') {
      const r = readJSON(text, { inContext });
      diagnostics.push(...r.diagnostics);
      return { document: r.document, diagnostics, form: fromGzip ? 'gzip' : 'json' };
    }
  }
  if (fromGzip) { diagnostics.push(diag('malformed-container', null, { reason: 'not-json' })); return empty(); }
  const r = parse(text);
  diagnostics.push(...r.diagnostics);
  return { document: r.document, diagnostics, form: 'text', info: r.info };
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

/** Decode a data:image/jai URL; null when it is not one. */
export function decodeDataURL(url: string, opts?: DecodeOptions): DecodeResult | null {
  const d = parseDataURL(url);
  if (!d || d.essence !== MEDIA_TYPE) return null;
  return decode(d.bytes, { inContext: true, ...opts });
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

/** Canonical URL form: data:image/jai, + encoded bare serialisation. */
export function toDataURL(doc: JaiDocument): string { return 'data:image/jai,' + encodeURLBody(serialize(doc, { bare: true })); }

/** Query forms: 'query' = whole query is the document; 'param' = first jai pair. Null when the URL carries no document. */
export function fromQuery(url: string, kind: 'query' | 'param' = 'query'): ParseResult | null {
  let href: string;
  try { href = new URL(String(url)).href; } catch { return null; }
  const hash = href.indexOf('#'); if (hash >= 0) href = href.slice(0, hash);
  const q = href.indexOf('?');
  if (q < 0) return null; // the query is null
  const query = href.slice(q + 1);
  if (kind === 'param') {
    const v = new URLSearchParams(query).get('jai');
    return v === null ? null : parse(v);
  }
  return parse(new TextDecoder().decode(percentDecode(query)));
}
