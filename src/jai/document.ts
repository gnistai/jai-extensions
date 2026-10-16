import { asciiLower, isKey, normalizeValue, type JaiDocument } from './chars.ts';

export function createDocument(): JaiDocument { return new Map(); }

export function addValue(doc: JaiDocument, key: string, value: string): void {
  const list = doc.get(key);
  if (list) list.push(value); else doc.set(key, [value]);
}

/** The fixture representation {key: [values]} (null-prototype object). */
export function toObject(doc: JaiDocument): Record<string, string[]> {
  const o: Record<string, string[]> = Object.create(null);
  for (const [k, v] of doc) o[k] = v.slice();
  return o;
}

/** Build a document from {key: value | [values]}; keys lowercased, values normalised. */
export function fromObject(obj: Record<string, unknown>): JaiDocument {
  const doc = createDocument();
  for (const k of Object.keys(obj)) {
    const key = asciiLower(k);
    if (!isKey(key) || key === 'jai') continue;
    const raw = obj[k];
    const items = Array.isArray(raw) ? raw : [raw];
    for (const it of items) {
      const v = normalizeValue(String(it));
      if (v !== null) addValue(doc, key, v);
    }
  }
  return doc;
}

export function documentsEqual(a: JaiDocument, b: JaiDocument): boolean {
  if (a.size !== b.size) return false;
  for (const [k, va] of a) {
    const vb = b.get(k);
    if (!vb || vb.length !== va.length) return false;
    for (let i = 0; i < va.length; i++) if (va[i] !== vb[i]) return false;
  }
  return true;
}
