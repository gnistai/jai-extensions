// HTML discovery (spec §Discovery). Works on a minimal element interface so it
// runs on real DOM elements and on hand-made trees alike.
import { MEDIA_TYPE, asciiLower, isAsciiSpace, normalizeValue, trimBy, type Diagnostic, type JaiDocument } from './chars.ts';
import { decodeDataURL } from './decode.ts';
import { parse, type ParseInfo } from './text.ts';

export interface ElementLike {
  localName: string;
  getAttribute(name: string): string | null;
  parentElement: ElementLike | null;
  children: ArrayLike<ElementLike> | Iterable<ElementLike>;
  ownerDocument?: { baseURI?: string } | null;
  shadowRoot?: RootLike | null;
  getAttributeNames?(): string[];
}
export interface RootLike { children?: ArrayLike<ElementLike> | Iterable<ElementLike>; querySelectorAll?(selector: string): ArrayLike<ElementLike> | Iterable<ElementLike> }

export type Carrier = 'picture' | 'attribute' | 'src';

export interface Discovery {
  carrier: Carrier;
  kind: 'inline' | 'url';
  document?: JaiDocument;
  diagnostics?: Diagnostic[];
  info?: ParseInfo;
  url?: string;
  source?: ElementLike;
}

function mimeEssence(type: string | null): string {
  if (!type) return '';
  return asciiLower(trimBy(type.split(';')[0] ?? '', isAsciiSpace));
}

export interface SrcsetCandidate { url: string; width: number | null; density: number | null; height: number | null }

/** HTML's "parse a srcset attribute": the candidates that survive, in order. */
export function srcsetCandidates(input: string | null | undefined): SrcsetCandidate[] {
  const s = String(input ?? '');
  const isWS = (c: string): boolean => c === ' ' || c === '\t' || c === '\n' || c === '\f' || c === '\r';
  const out: SrcsetCandidate[] = [];
  let pos = 0;
  for (;;) {
    while (pos < s.length && (isWS(s.charAt(pos)) || s.charAt(pos) === ',')) pos++;
    if (pos >= s.length) return out;
    const start = pos;
    while (pos < s.length && !isWS(s.charAt(pos))) pos++;
    let url = s.slice(start, pos);
    const descriptors: string[] = [];
    if (url.endsWith(',')) url = trimBy(url, c => c === 0x2c, false);
    else {
      while (pos < s.length && isWS(s.charAt(pos))) pos++;
      let current = '';
      let state: 'in' | 'paren' | 'after' = 'in';
      for (;;) {
        const c = pos < s.length ? s.charAt(pos) : undefined;
        if (state === 'in') {
          if (c !== undefined && isWS(c)) { if (current) { descriptors.push(current); current = ''; } state = 'after'; }
          else if (c === ',') { pos++; if (current) descriptors.push(current); break; }
          else if (c === '(') { current += c; state = 'paren'; }
          else if (c === undefined) { if (current) descriptors.push(current); break; }
          else current += c;
        } else if (state === 'paren') {
          if (c === ')') { current += c; state = 'in'; }
          else if (c === undefined) { descriptors.push(current); break; }
          else current += c;
        } else if (c === undefined) break;
        else if (!isWS(c)) { state = 'in'; continue; }
        pos++;
      }
    }
    let error = false;
    let w: number | null = null, d: number | null = null, h: number | null = null;
    for (const desc of descriptors) {
      const last = desc.charAt(desc.length - 1), val = desc.slice(0, -1);
      if (last === 'w' && /^[0-9]+$/.test(val)) { if (w !== null || d !== null) error = true; else { w = +val; if (w === 0) error = true; } }
      else if (last === 'x' && /^-?(?:[0-9]+(?:\.[0-9]+)?|\.[0-9]+)(?:[eE][+-]?[0-9]+)?$/.test(val)) { if (w !== null || d !== null || h !== null) error = true; else { d = parseFloat(val); if (d < 0) error = true; } }
      else if (last === 'h' && /^[0-9]+$/.test(val)) { if (h !== null || d !== null) error = true; else { h = +val; if (h === 0) error = true; } }
      else error = true;
    }
    if (h !== null && w === null) error = true;
    if (!error && url) out.push({ url, width: w, density: d, height: h });
  }
}

/** First candidate URL of a srcset attribute under HTML's parsing, or null. */
export function firstSrcsetURL(srcset: string | null): string | null {
  if (srcset == null) return null;
  return srcsetCandidates(srcset)[0]?.url ?? null;
}

const toArray = <T>(x: ArrayLike<T> | Iterable<T>): T[] => Array.from(x as ArrayLike<T>);

/**
 * A .jai resource URL for the src carrier (spec §Carriers): an http(s) URL, after
 * resolving against the base, whose path ends in ".jai" (ASCII case-insensitive;
 * query and fragment ignored). Returns the absolute URL or null.
 */
export function jaiResourceURL(value: string | null | undefined, baseURL?: string): string | null {
  if (value == null) return null;
  let u: URL;
  try { u = new URL(value, baseURL || undefined); } catch { return null; }
  if (u.protocol !== 'http:' && u.protocol !== 'https:') return null;
  return /\.jai$/i.test(u.pathname) ? u.href : null;
}

/** Whether HTML's image source selection would use the element's src at all (no own srcset, no preceding <source>). */
export function srcIsSelected(img: ElementLike): boolean {
  if (img.getAttribute('srcset') !== null) return false;
  const parent = img.parentElement;
  if (parent && parent.localName === 'picture') {
    for (const child of toArray(parent.children)) {
      if (child === img) break;
      if (child.localName === 'source') return false;
    }
  }
  return true;
}

/** Discovery for one <img>. For a 'url' kind (picture or src carrier) the caller fetches and decodes it, only when rendering. */
export function discover(img: ElementLike, { baseURL }: { baseURL?: string } = {}): Discovery | null {
  const parent = img.parentElement;
  if (parent && parent.localName === 'picture') {
    for (const child of toArray(parent.children)) {
      if (child === img) break;
      if (child.localName !== 'source') continue;
      if (mimeEssence(child.getAttribute('type')) !== MEDIA_TYPE) continue;
      const media = child.getAttribute('media');
      const mm = (globalThis as { matchMedia?: (q: string) => { matches: boolean } }).matchMedia;
      if (media && typeof mm === 'function' && !mm(media).matches) continue;
      const url = firstSrcsetURL(child.getAttribute('srcset'));
      if (!url) continue;
      if (/^data:/i.test(url)) {
        const r = decodeDataURL(url);
        if (!r) continue;
        return { carrier: 'picture', kind: 'inline', document: r.document, diagnostics: r.diagnostics, source: child };
      }
      let abs = url;
      try { abs = new URL(url, baseURL || img.ownerDocument?.baseURI || undefined).href; } catch { /* keep as written */ }
      return { carrier: 'picture', kind: 'url', url: abs, source: child };
    }
  }
  const dj = img.getAttribute('data-jai');
  if (dj !== null) {
    const r = parse(dj);
    return { carrier: 'attribute', kind: 'inline', document: withAltPrompt(r.document, img.getAttribute('alt')), diagnostics: r.diagnostics, info: r.info };
  }
  const src = img.getAttribute('src');
  if (src !== null && srcIsSelected(img)) {
    const r = decodeDataURL(src);
    if (r) return { carrier: 'src', kind: 'inline', document: r.document, diagnostics: r.diagnostics };
    const url = jaiResourceURL(src, baseURL || img.ownerDocument?.baseURI);
    if (url) return { carrier: 'src', kind: 'url', url };
  }
  // `alt` is never a carrier: it is the image's text for screen readers.
  return null;
}

/** Every carried document under a root, in tree order; open shadow trees are visited at their host. */
export function discoverAll(root: RootLike, opts?: { baseURL?: string }): Array<Discovery & { element: ElementLike }> {
  const out: Array<Discovery & { element: ElementLike }> = [];
  const visit = (node: { children?: ArrayLike<ElementLike> | Iterable<ElementLike> }): void => {
    for (const el of toArray(node.children ?? [])) {
      if (el.localName === 'img') { const d = discover(el, opts); if (d) out.push({ element: el, ...d }); }
      if (el.shadowRoot) visit(el.shadowRoot);
      visit(el);
    }
  };
  visit(root);
  return out;
}

/**
 * Attribute carrier (spec §Carriers): when the document parsed from data-jai has no
 * prompt value (qualified prompt/<target> keys do not count), the element's alt text,
 * after value normalisation and if not empty, is its single prompt value, taken
 * literally and never parsed. Otherwise alt plays no part.
 */
export function withAltPrompt(doc: JaiDocument, alt: string | null | undefined): JaiDocument {
  if (doc.has('prompt')) return doc;
  const a = alt === null || alt === undefined ? null : normalizeValue(String(alt));
  return a ? new Map([['prompt', [a]], ...doc]) : doc;
}
