// The JSON form: reading, canonical JSON and identity.
import { asciiLower, byCodePoint, diag, isKey, normalizeValue, type Diagnostic, type JaiDocument } from './chars.ts';
import { addValue, createDocument } from './document.ts';

/** ECMAScript Number::toString, as RFC 8785 §3.2.2.3 requires. */
export function numberToString(n: number): string { return String(n); }

export interface ReadResult { document: JaiDocument; diagnostics: Diagnostic[]; isJAI: boolean }

/** JSON reading algorithm (spec §Reading). `text` may be a JSON string or a parsed value. */
export function readJSON(text: string | unknown, { inContext = true }: { inContext?: boolean } = {}): ReadResult {
  const diagnostics: Diagnostic[] = [];
  const doc = createDocument();
  let obj: unknown;
  if (typeof text === 'string') {
    try { obj = JSON.parse(text); } catch { diagnostics.push(diag('invalid-json')); return { document: doc, diagnostics, isJAI: false }; }
  } else obj = text;
  if (obj === null || typeof obj !== 'object' || Array.isArray(obj)) {
    diagnostics.push(diag('invalid-json')); return { document: doc, diagnostics, isJAI: false };
  }
  const rec = obj as Record<string, unknown>;
  if (!Object.prototype.hasOwnProperty.call(rec, 'jai')) {
    if (!inContext) return { document: doc, diagnostics, isJAI: false };
    diagnostics.push(diag('missing-marker'));
  } else {
    const m = rec['jai'];
    if (typeof m === 'number' && Number.isInteger(m)) { if (m !== 1) diagnostics.push(diag('unsupported-version')); }
    else diagnostics.push(diag('invalid-marker'));
  }
  for (const name of Object.keys(rec)) {
    if (name === 'jai') continue;
    const key = asciiLower(name);
    if (!isKey(key)) { diagnostics.push(diag('invalid-key', name)); continue; }
    if (key === 'jai') { diagnostics.push(diag('duplicate-member', 'jai')); continue; }
    const raw = rec[name];
    const items: unknown[] = Array.isArray(raw) ? raw : [raw];
    const existed = doc.has(key);
    let added = false;
    for (const it of items) {
      let cand: string;
      if (typeof it === 'string') cand = it;
      else if (typeof it === 'number') {
        if (!Number.isFinite(it)) { diagnostics.push(diag('invalid-json')); continue; }
        cand = numberToString(it); diagnostics.push(diag('number-value', key));
      } else if (typeof it === 'boolean') { cand = it ? 'true' : 'false'; diagnostics.push(diag('number-value', key)); }
      else if (it === null) { diagnostics.push(diag('empty-value', key)); continue; }
      else { diagnostics.push(diag('unsupported-member', key)); continue; }
      const v = normalizeValue(cand);
      if (v === null) { diagnostics.push(diag('empty-value', key)); continue; }
      addValue(doc, key, v); added = true;
    }
    if (existed && added) diagnostics.push(diag('duplicate-key', key));
  }
  return { document: doc, diagnostics, isJAI: true };
}

/** Canonical JSON form (spec §Canonical JSON). */
export function canonicalJSON(doc: JaiDocument): string {
  const parts = ['"jai":1'];
  const enc = (v: string[]): string => (v.length === 1 ? JSON.stringify(v[0]) : JSON.stringify(v));
  const prompt = doc.get('prompt');
  if (prompt) parts.push('"prompt":' + enc(prompt));
  const keys = [...doc.keys()].filter(k => k !== 'prompt').sort(byCodePoint);
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
