// The text form: parsing algorithm and canonical serialisation.
import { KEY_SRC, LEADING_WS_RE, WS_CLASS, WS_ONLY_RE, asciiLower, byCodePoint, diag, isKey, normalizeValue, toScalarValueString, trim, type Diagnostic, type JaiDocument } from './chars.ts';
import { addValue, createDocument } from './document.ts';

const ASSIGNMENT_PATTERN_RE = new RegExp(`^[${WS_CLASS}]*${KEY_SRC}[${WS_CLASS}]*=`, 'i');
const ESCAPE_VALUE_RE = new RegExp(`\\\\(?=[;=\\\\]|$)|^;|;(?=[${WS_CLASS}]*(?:$|;|${KEY_SRC}[${WS_CLASS}]*=))`, 'gi');

export type SegmentInfo =
  | { kind: 'assignment'; key: string; index: number }
  | { kind: 'prompt'; key: 'prompt'; index: number }
  | { kind: 'continuation'; key: string; index: number; raw: string }
  | { kind: 'reserved'; key: string }
  | { kind: 'dropped' };

interface Pending { key: string; text: string; kind: 'assignment' | 'prose'; continued: number; value?: string; dropped?: boolean }

export interface ParseInfo { prefixed: boolean; segments: SegmentInfo[]; pending: Pending[] }
export interface ParseResult { document: JaiDocument; diagnostics: Diagnostic[]; info: ParseInfo }

function unescapeText(s: string): string {
  let out = '';
  for (let i = 0; i < s.length; i++) {
    const c = s.charAt(i), n = s.charAt(i + 1);
    if (c === '\\' && i + 1 < s.length && (n === ';' || n === '=' || n === '\\')) { out += n; i++; }
    else out += c;
  }
  return out;
}

function indexOfUnescaped(s: string, ch: string): number {
  for (let i = 0; i < s.length; i++) {
    if (s.charAt(i) === '\\' && i + 1 < s.length) { i++; continue; }
    if (s.charAt(i) === ch) return i;
  }
  return -1;
}

/** Text parsing algorithm (spec §Parsing). Total and deterministic. */
export function parse(rawInput: string): ParseResult {
  const diagnostics: Diagnostic[] = [];
  const doc = createDocument();
  let input = toScalarValueString(String(rawInput));
  let i = 0;
  while (input.charCodeAt(i) === 0xfeff) i++;
  input = input.slice(i).replace(/\r\n/g, '\n').replace(/\r/g, '\n');
  i = (LEADING_WS_RE.exec(input)?.[0] ?? '').length;
  let prefixed = false;
  if (/^jai:/i.test(input.slice(i, i + 4))) { i += 4; prefixed = true; }

  const raws: string[] = [];
  let cur = '';
  for (let j = i; j < input.length; j++) {
    const c = input.charAt(j);
    if (c === '\\' && j + 1 < input.length) { cur += c + input.charAt(j + 1); j++; continue; }
    if (c === ';') { raws.push(cur); cur = ''; continue; }
    cur += c;
  }
  raws.push(cur);

  const DISCARD: Pending = { key: '', text: '', kind: 'assignment', continued: 0, dropped: true };
  const pending: Pending[] = [];
  let current: Pending | null = null;
  const segments: SegmentInfo[] = [];
  raws.forEach((raw, idx) => {
    if (WS_ONLY_RE.test(raw)) {
      if (idx !== raws.length - 1) diagnostics.push(diag('empty-segment'));
      return;
    }
    const eq = indexOfUnescaped(raw, '=');
    if (eq >= 0) {
      const key = asciiLower(trim(raw.slice(0, eq)));
      if (isKey(key)) {
        if (key === 'jai') {
          diagnostics.push(diag('reserved-key', 'jai'));
          current = DISCARD;
          segments.push({ kind: 'reserved', key });
          return;
        }
        const p: Pending = { key, text: raw.slice(eq + 1), kind: 'assignment', continued: 0 };
        pending.push(p); current = p;
        segments.push({ kind: 'assignment', key, index: pending.length - 1 });
        return;
      }
    }
    if (current === null) {
      const p: Pending = { key: 'prompt', text: raw, kind: 'prose', continued: 0 };
      pending.push(p); current = p;
      segments.push({ kind: 'prompt', key: 'prompt', index: pending.length - 1 });
    } else if (current === DISCARD) {
      segments.push({ kind: 'dropped' });
    } else {
      const c: Pending = current;
      c.text += ';' + raw; c.continued++;
      segments.push({ kind: 'continuation', key: c.key, index: pending.indexOf(c), raw });
    }
  });
  for (const p of pending) {
    const v = normalizeValue(unescapeText(p.text));
    if (v === null) { diagnostics.push(diag('empty-value', p.key)); p.dropped = true; }
    else { addValue(doc, p.key, v); p.value = v; }
  }
  return { document: doc, diagnostics, info: { prefixed, segments, pending } };
}

/** escape-value: one left-to-right pass (spec §Canonical text serialisation). */
export function escapeValue(v: string): string { return v.replace(ESCAPE_VALUE_RE, m => '\\' + m); }

/** escape-prose: escape every \ ; = (spec §Escaping arbitrary text). */
export function escapeProse(s: string): string { return s.replace(/[\;=]/g, m => '\\' + m); }

/** Canonical text serialisation; {bare: true} omits the prefix and adds the bare protections. */
export function serialize(doc: JaiDocument, { bare = false }: { bare?: boolean } = {}): string {
  const segments: string[] = [];
  (doc.get('prompt') ?? []).forEach((v, n) => {
    let promptForm = n > 0 || ASSIGNMENT_PATTERN_RE.test(v);
    if (!promptForm && bare && (/^jai:/i.test(v) || v.charCodeAt(0) === 0xfeff || v.charAt(0) === '{')) promptForm = true;
    segments.push(promptForm ? 'prompt=' + escapeValue(v) : escapeValue(v));
  });
  const keys = [...doc.keys()].filter(k => k !== 'prompt').sort(byCodePoint);
  for (const k of keys) for (const v of doc.get(k) ?? []) segments.push(k + '=' + escapeValue(v));
  return (bare ? '' : 'jai:') + segments.join(';');
}
