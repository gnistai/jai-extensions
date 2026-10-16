// HTML discovery (spec §Discovery). Works on a minimal element interface so it
// runs on real DOM elements and on hand-made trees alike.
import { LEADING_WS_RE, MEDIA_TYPE, asciiLower, type Diagnostic, type JaiDocument } from './chars.ts';
import { decodeDataURL } from './decode.ts';
import { parse, type ParseInfo } from './text.ts';

export interface ElementLike {
  localName: string;
  getAttribute(name: string): string | null;
  parentElement: ElementLike | null;
  children: ArrayLike<ElementLike> | Iterable<ElementLike>;
  ownerDocument?: { baseURI?: string } | null;
  shadowRoot?: RootLike | null;
}
export interface RootLike { querySelectorAll(selector: string): ArrayLike<ElementLike> | Iterable<ElementLike> }

export interface Discovery {
  carrier: 1 | 2 | 3 | 4;
  kind: 'inline' | 'url';
  document?: JaiDocument;
  diagnostics?: Diagnostic[];
  info?: ParseInfo;
  url?: string;
  source?: ElementLike;
}

function mimeEssence(type: string | null): string {
  if (!type) return '';
  return asciiLower((type.split(';')[0] ?? '').replace(/^[\t\n\f\r ]+|[\t\n\f\r ]+$/g, ''));
}

/** First candidate URL of a srcset attribute (URL, then descriptors), or null. */
export function firstSrcsetURL(srcset: string | null): string | null {
  if (srcset == null) return null;
  const s = String(srcset);
  let i = 0;
  while (i < s.length && /[\t\n\f\r ,]/.test(s.charAt(i))) i++;
  if (i >= s.length) return null;
  let j = i;
  while (j < s.length && !/[\t\n\f\r ]/.test(s.charAt(j))) j++;
  let url = s.slice(i, j);
  if (j >= s.length) url = url.replace(/,+$/, '');
  return url || null;
}

const toArray = <T>(x: ArrayLike<T> | Iterable<T>): T[] => Array.from(x as ArrayLike<T>);

/** Discovery for one <img>. For carrier 1 with a resource URL the caller fetches and decodes it. */
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
        return { carrier: 1, kind: 'inline', document: r.document, diagnostics: r.diagnostics, source: child };
      }
      let abs = url;
      try { abs = new URL(url, baseURL || img.ownerDocument?.baseURI || undefined).href; } catch { /* keep as written */ }
      return { carrier: 1, kind: 'url', url: abs, source: child };
    }
  }
  const dj = img.getAttribute('data-jai');
  if (dj !== null) {
    const r = parse(dj);
    return { carrier: 2, kind: 'inline', document: r.document, diagnostics: r.diagnostics, info: r.info };
  }
  const src = img.getAttribute('src');
  if (src !== null) {
    const r = decodeDataURL(src);
    if (r) return { carrier: 3, kind: 'inline', document: r.document, diagnostics: r.diagnostics };
  }
  const alt = img.getAttribute('alt');
  if (alt !== null && /^jai:/i.test(alt.replace(LEADING_WS_RE, ''))) {
    const r = parse(alt);
    return { carrier: 4, kind: 'inline', document: r.document, diagnostics: r.diagnostics, info: r.info };
  }
  return null;
}

/** Every carried document under a root, in tree order, descending into open shadow roots. */
export function discoverAll(root: RootLike, opts?: { baseURL?: string }): Array<Discovery & { element: ElementLike }> {
  const out: Array<Discovery & { element: ElementLike }> = [];
  const walk = (node: RootLike): void => {
    for (const img of toArray(node.querySelectorAll('img'))) {
      const d = discover(img, opts);
      if (d) out.push({ element: img, ...d });
    }
    for (const el of toArray(node.querySelectorAll('*'))) if (el.shadowRoot) walk(el.shadowRoot);
  };
  walk(root);
  return out;
}
