// Character classes, normalisation and diagnostics shared by every JAI module.
// TypeScript port of the reference implementation jai.js (gnistai/jai). MIT.

export const EDITION = '1.0-draft';
export const MEDIA_TYPE = 'image/jai';

/** A document: key -> non-empty ordered list of values. */
export type JaiDocument = Map<string, string[]>;

export interface Diagnostic {
  code: string;
  key?: string;
  [extra: string]: unknown;
}

/** The 25 code points with the Unicode White_Space property, as a regex class body. */
export const WS_CLASS = '\\t\\n\\v\\f\\r \\u0085\\u00a0\\u1680\\u2000-\\u200a\\u2028\\u2029\\u202f\\u205f\\u3000';
export const WS_ONLY_RE = new RegExp(`^[${WS_CLASS}]*$`);
const TRIM_RE = new RegExp(`^[${WS_CLASS}]+|[${WS_CLASS}]+$`, 'g');
export const LEADING_WS_RE = new RegExp(`^[${WS_CLASS}]*`);

export const KEY_SRC = '[a-z0-9][a-z0-9_-]*(?:\\.[a-z0-9][a-z0-9_-]*)*(?:/[a-z0-9][a-z0-9_-]*)?';
/** Names that take a per-model target (spec §Per-model refinements). */
export const QUALIFIABLE: ReadonlySet<string> = new Set(['prompt', 'style', 'avoid']);
/** The key grammar (lowercase). */
export const KEY_RE = new RegExp(`^${KEY_SRC}$`);

export const STANDARD_KEYS: readonly string[] = ['prompt', 'style', 'avoid', 'model', 'aspect', 'mode', 'strength', 'seed', 'source', 'preview', 'animate'];
export const SINGLE_VALUED: ReadonlySet<string> = new Set(['aspect', 'mode', 'strength', 'seed', 'source', 'preview', 'animate']);
export const RASTER_TYPES: ReadonlySet<string> = new Set(['image/png', 'image/jpeg', 'image/webp', 'image/gif', 'image/avif']);

/** Trim exactly the JAI whitespace code points. */
export function trim(s: string): string { return s.replace(TRIM_RE, ''); }
/** ASCII lowercase only (never locale-sensitive). */
export function asciiLower(s: string): string { return s.replace(/[A-Z]/g, c => String.fromCharCode(c.charCodeAt(0) + 32)); }
export function isKey(s: string): boolean { return KEY_RE.test(s); }

export function diag(code: string, key?: string | null, extra?: Record<string, unknown>): Diagnostic {
  const d: Diagnostic = { code };
  if (key !== undefined && key !== null) d.key = key;
  if (extra) Object.assign(d, extra);
  return d;
}

/** Replace surrogate code points that are not part of a pair with U+FFFD. */
export function toScalarValueString(s: string): string {
  let out = '';
  for (let i = 0; i < s.length; i++) {
    const c = s.charCodeAt(i);
    if (c >= 0xd800 && c <= 0xdbff) {
      const d = i + 1 < s.length ? s.charCodeAt(i + 1) : 0;
      if (d >= 0xdc00 && d <= 0xdfff) { out += s.slice(i, i + 2); i++; continue; }
      out += '�';
    } else if (c >= 0xdc00 && c <= 0xdfff) {
      out += '�';
    } else {
      out += s.charAt(i);
    }
  }
  return out;
}

/** Value normalisation (spec §Values); null when the value is empty afterwards. */
export function normalizeValue(s: string): string | null {
  let v = toScalarValueString(s);
  v = v.replace(/\r\n/g, '\n').replace(/\r/g, '\n');
  v = v.replace(/\u0000/g, '�');
  v = trim(v);
  return v.length ? v : null;
}

export const byCodePoint = (a: string, b: string): number => (a < b ? -1 : a > b ? 1 : 0);
/** Canonical key order after `prompt`: the qualified prompt keys, then every other key, each by code point. */
export function orderedKeys(doc: JaiDocument): string[] {
  const rest = [...doc.keys()].filter(k => k !== 'prompt');
  return [...rest.filter(k => k.startsWith('prompt/')).sort(byCodePoint), ...rest.filter(k => !k.startsWith('prompt/')).sort(byCodePoint)];
}
