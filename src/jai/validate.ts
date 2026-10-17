// Advisory validator (spec §Diagnostics, validator codes).
import { MEDIA_TYPE, QUALIFIABLE, SINGLE_VALUED, STANDARD_KEYS, WS_CLASS, asciiLower, diag, isAsciiSpace, trim, trimBy, type Diagnostic, type JaiDocument } from './chars.ts';
import { srcIsSelected, srcsetCandidates, type Carrier } from './html.ts';
import { canonicalJSON } from './json.ts';
import type { DecodeResult } from './decode.ts';
import type { ElementLike } from './html.ts';
import { interpret, parseAspect } from './interpret.ts';
import type { ParseInfo } from './text.ts';

const CONFUSABLE_DELIM = new RegExp(`[\\uff1b\\u061b][${WS_CLASS}]*[A-Za-z0-9][A-Za-z0-9_.-]*(?:/[A-Za-z0-9][A-Za-z0-9_-]*)?[${WS_CLASS}]*[=\\uff1d]`);
const CONFUSABLE_EQ = /[A-Za-z0-9][\t ]*＝/;

export function validate(doc: JaiDocument, info?: ParseInfo | null): Diagnostic[] {
  const d: Diagnostic[] = [];
  const stdset = new Set(STANDARD_KEYS);
  const utf8 = (s: string): number => new TextEncoder().encode(s).length;
  for (const [k, vals] of doc) {
    const base = k.split('/')[0] ?? k;
    if (!base.includes('.') && !stdset.has(base)) d.push(diag('unknown-key', k));
    if (k.includes('/') && !QUALIFIABLE.has(base)) d.push(diag('invalid-qualifier', k));
    for (const v of vals) {
      if (/[\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f-\u009f]/.test(v)) d.push(diag('control-character', k));
      if (/[​-‍⁠]|.﻿/s.test(v)) d.push(diag('invisible-char', k));
      if (/[‪-‮⁦-⁩]/.test(v)) d.push(diag('bidi-control', k));
      if (utf8(v) > 8192) d.push(diag('large-payload', k));
      // No \n after the first one: the same matches, in linear time (a run of newlines made this quadratic).
      if (/\n[\t\f\r ]*[A-Za-z0-9][A-Za-z0-9_.-]*(?:\/[A-Za-z0-9][A-Za-z0-9_-]*)?[\t\n\f\r ]*=/.test(v)) d.push(diag('newline-assignment', k));
      if (CONFUSABLE_DELIM.test(v)) d.push(diag('confusable-delimiter', k));
      else if (CONFUSABLE_EQ.test(v)) d.push(diag('confusable-equals', k));
      if (!['source', 'preview', 'model'].includes(k) && !/^[a-z][a-z0-9+.-]*:/i.test(v) && /%[0-9A-Fa-f]{2}/.test(v) && !/%25/i.test(v)) d.push(diag('percent-encoded-text', k));
      if (k === 'avoid' && /^(no|not|without|never)[\t\n\f\r  ]/i.test(v)) d.push(diag('negated-avoid', k));
    }
    if (SINGLE_VALUED.has(k)) {
      const last = vals[vals.length - 1] ?? '';
      const q = last.charAt(0);
      if (last.length >= 2 && (q === '"' || q === "'") && last.charAt(last.length - 1) === q) d.push(diag('quoted-value', k));
    }
  }
  if (utf8(canonicalJSON(doc)) > 65536) d.push(diag('large-payload'));
  const interp = interpret(doc);
  if (!doc.has('prompt') && interp.interpreted.mode === 'generate') d.push(diag('no-prompt'));
  if (doc.has('strength') && interp.interpreted.mode !== 'transform') d.push(diag('strength-without-transform', 'strength'));
  for (const v of doc.get('model') ?? []) if (/^[a-z][a-z0-9+.-]*:\/\/|^\.?\.?\//i.test(v)) d.push(diag('model-looks-like-locator', 'model'));
  if (info?.segments) {
    const segs = info.segments;
    const first = segs[0];
    if (first && first.kind === 'assignment' && !doc.has('prompt')) {
      const base = first.key.split('/')[0] ?? first.key;
      const defined = first.key.includes('/') ? QUALIFIABLE.has(base) : stdset.has(first.key);
      if (!defined && !base.includes('.')) d.push(diag('first-segment-assignment', first.key));
    }
    for (const s of segs) {
      if (s.kind === 'continuation') {
        if (SINGLE_VALUED.has(s.key) && s.key !== 'source' && s.key !== 'preview') d.push(diag('continuation-into-single-valued-key', s.key));
        const t = asciiLower(trim(s.raw));
        if (stdset.has(t)) d.push(diag('bare-key', t));
      }
    }
    for (let i = 1; i < segs.length; i++) {
      const prev = segs[i - 1], s = segs[i];
      if (prev && s && s.kind === 'assignment' && ['charset', 'base64', 'name', 'filename'].includes(s.key) && prev.kind === 'assignment' && (prev.key === 'source' || prev.key === 'preview')) d.push(diag('split-data-uri', prev.key));
    }
  }
  for (const x of interp.diagnostics) if (x.code === 'repeated-single' || x.code === 'invalid-value') d.push(x);
  const aspect = doc.get('aspect');
  if (aspect) {
    const v = aspect[aspect.length - 1] ?? '';
    const a = parseAspect(v);
    if (a && a.kind === 'ratio' && !/^[0-9]+:[0-9]+$/.test(v) && v.toLowerCase() !== 'square') d.push(diag('nonstandard-value', 'aspect', { suggestion: `${a.width}:${a.height}` }));
  }
  return d;
}

/** Validator advisories for a data:image/jai URL itself: a missing comma, media-type parameters (`;base64` included), unencoded text. Empty for any other URL. */
export function validateDataURL(url: string): Diagnostic[] {
  const d: Diagnostic[] = [];
  const s = trimBy(String(url), c => c <= 0x20); // C0 controls and space
  if (!/^data:image\/jai(?=[;,]|$)/i.test(s)) return d;
  const comma = s.indexOf(',');
  if (comma < 0) { d.push(diag('data-url-missing-comma')); return d; }
  // Media-type parameters have no meaning in a data:image/jai URL, base64 and charset included.
  if (s.slice(5, comma).split(';').slice(1).some(p => trim(p))) d.push(diag('data-url-parameter'));
  const body = s.slice(comma + 1);
  if (/[\t\n\r ]|#/.test(body) || /%(?![0-9A-Fa-f]{2})/.test(body)) d.push(diag('unencoded-url-text'));
  return d;
}

/**
 * Whether the element sizes its layout box itself, as far as its attributes show:
 * both `width` and `height`, or an `aspect-ratio` other than `auto` in `style`.
 */
function hasAuthorSize(el: ElementLike): boolean {
  if (el.getAttribute('width') && el.getAttribute('height')) return true;
  return (el.getAttribute('style') ?? '').split(';').some(decl => {
    const m = /^\s*aspect-ratio\s*:(.*)$/is.exec(decl);
    return !!m && !/^\s*(?:auto\s*)?(?:!\s*important\s*)?$/i.test(m[1] ?? '');
  });
}

/**
 * Advisories about how a document was delivered: a discovery carrier with its
 * element, and the document it carried ({document}, as decoded or discovered).
 */
export function validateDelivery({ decoded, carrier, element }: { decoded?: Pick<DecodeResult, 'document'> | null; carrier?: Carrier; element?: ElementLike } = {}): Diagnostic[] {
  const d: Diagnostic[] = [];
  const doc = decoded?.document;
  if (carrier === 'src' && element && !hasAuthorSize(element)) d.push(diag('missing-size'));
  if ((carrier === 'picture' || carrier === 'src') && doc && interpret(doc).interpreted.mode === 'keep') d.push(diag('keep-in-source'));
  if (element) {
    const names = typeof element.getAttributeNames === 'function' ? element.getAttributeNames() : [];
    if (names.some(n => /["';=]/.test(n))) d.push(diag('attribute-split'));
    const src = element.getAttribute('src');
    if (src) d.push(...validateDataURL(src));
    // A JAI src that HTML's image selection would never use (own srcset, or a preceding <source>).
    if (src && !srcIsSelected(element) && (/^\s*data:image\/jai(?=[;,]|$)/i.test(src) || /\.jai(?:[?#]|$)/i.test(src))) d.push(diag('src-shadowed'));
    const parent = element.parentElement;
    if (parent && parent.localName === 'picture') {
      let after = false;
      for (const ch of Array.from(parent.children as ArrayLike<ElementLike>)) {
        if (ch === element) { after = true; continue; }
        if (ch.localName !== 'source') continue;
        const type = ch.getAttribute('type'), ss = ch.getAttribute('srcset') ?? '';
        const isJai = asciiLower(trimBy((type ?? '').split(';')[0] ?? '', isAsciiSpace)) === MEDIA_TYPE;
        const first = trimBy(ss.replace(/^[\t\n\f\r ,]+/, '').split(/[\t\n\f\r ]/)[0] ?? '', c => c === 0x2c, false);
        const looksJai = /^data:image\/jai(?=[;,]|$)/i.test(first) || /\.jai(?:[?#]|$)/i.test(first);
        if (after) { if (isJai || looksJai) d.push(diag('source-after-img')); continue; }
        if (isJai && srcsetCandidates(ss).length === 0) d.push(diag('srcset-unparseable'));
        if (!type && looksJai) d.push(diag('source-missing-type'));
        if (isJai) d.push(...validateDataURL(first));
      }
    }
  }
  return d;
}
