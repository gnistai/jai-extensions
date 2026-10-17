// The JSON form: reading, canonical JSON and identity.
import { asciiLower, orderedKeys, diag, isKey, normalizeValue, type Diagnostic, type JaiDocument } from './chars.ts';
import { addValue, createDocument } from './document.ts';

/** ECMAScript Number::toString, as RFC 8785 §3.2.2.3 requires. */
export function numberToString(n: number): string { return String(n); }

export interface ReadResult { document: JaiDocument; diagnostics: Diagnostic[] }

/** True when arrays/objects nest deeper than `max` anywhere (the top-level object has depth 1). */
function jsonDepthExceeds(s: string, max: number): boolean {
  let d = 0, inStr = false;
  for (let i = 0; i < s.length; i++) {
    const c = s.charCodeAt(i);
    if (inStr) { if (c === 92) i++; else if (c === 34) inStr = false; continue; }
    if (c === 34) inStr = true;
    else if (c === 123 || c === 91) { if (++d > max) return true; }
    else if (c === 125 || c === 93) d--;
  }
  return false;
}

/** JSON reading algorithm (spec §Reading) on a JSON text. */
export function readJSON(text: string): ReadResult {
  const diagnostics: Diagnostic[] = [];
  const doc = createDocument();
  const fail = (): ReadResult => { diagnostics.push(diag('invalid-json')); return { document: createDocument(), diagnostics }; };
  const s = String(text);
  if (jsonDepthExceeds(s, 32)) return fail();
  let obj: unknown;
  try {
    // A number that rounds to an infinite binary64 value makes the text invalid.
    obj = JSON.parse(s, (_k, v: unknown) => { if (typeof v === 'number' && !Number.isFinite(v)) throw new RangeError('overflow'); return v; });
  } catch { return fail(); }
  if (obj === null || typeof obj !== 'object' || Array.isArray(obj)) return fail();
  const rec = obj as Record<string, unknown>;
  // JSON.parse already applied "an exact repeat replaces in place"; readers do not diagnose it.
  if (!Object.prototype.hasOwnProperty.call(rec, 'jai')) {
    diagnostics.push(diag('missing-marker'));
  } else {
    const m = rec['jai'];
    if (typeof m === 'number' && Number.isInteger(m)) { if (m !== 1) diagnostics.push(diag('unsupported-version')); }
    else diagnostics.push(diag('invalid-marker'));
  }
  const seen = new Set<string>();
  for (const name of Object.keys(rec)) {
    if (name === 'jai') continue;
    const key = asciiLower(name);
    if (!isKey(key)) { diagnostics.push(diag('invalid-key', name)); continue; }
    if (key === 'jai') { diagnostics.push(diag('duplicate-member', 'jai')); continue; }
    if (seen.has(key)) diagnostics.push(diag('duplicate-key', key));
    seen.add(key);
    const raw = rec[name];
    const items: unknown[] = Array.isArray(raw) ? raw : [raw];
    for (const it of items) {
      let cand: string;
      if (typeof it === 'string') cand = it;
      else if (typeof it === 'number') { cand = numberToString(it); diagnostics.push(diag('number-value', key)); }
      else if (typeof it === 'boolean') { cand = it ? 'true' : 'false'; diagnostics.push(diag('number-value', key)); }
      else if (it === null) { diagnostics.push(diag('empty-value', key)); continue; }
      else { diagnostics.push(diag('unsupported-member', key)); continue; }
      const v = normalizeValue(cand);
      if (v === null) { diagnostics.push(diag('empty-value', key)); continue; }
      addValue(doc, key, v);
    }
  }
  return { document: doc, diagnostics };
}

/** Canonical JSON form (spec §Canonical JSON). */
export function canonicalJSON(doc: JaiDocument): string {
  const parts = ['"jai":1'];
  const enc = (v: string[]): string => (v.length === 1 ? JSON.stringify(v[0]) : JSON.stringify(v));
  const prompt = doc.get('prompt');
  if (prompt) parts.push('"prompt":' + enc(prompt));
  const keys = orderedKeys(doc);
  for (const k of keys) parts.push(JSON.stringify(k) + ':' + enc(doc.get(k) ?? []));
  return '{' + parts.join(',') + '}';
}

interface SubtleLike { digest(alg: string, data: Uint8Array): Promise<ArrayBuffer> }

export async function sha256Hex(bytes: Uint8Array): Promise<string> {
  let subtle: SubtleLike | undefined = (globalThis as { crypto?: { subtle?: SubtleLike } }).crypto?.subtle;
  if (!subtle) {
    const m = (await import('node:crypto' as string)) as { webcrypto: { subtle: SubtleLike } };
    subtle = m.webcrypto.subtle;
  }
  const digest = new Uint8Array(await subtle.digest('SHA-256', bytes));
  let hex = '';
  for (const b of digest) hex += b.toString(16).padStart(2, '0');
  return hex;
}

/** Identity: SHA-256 of the canonical JSON form, lowercase hex. */
export async function identity(doc: JaiDocument): Promise<string> {
  return sha256Hex(new TextEncoder().encode(canonicalJSON(doc)));
}
