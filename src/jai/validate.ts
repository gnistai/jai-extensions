// Advisory validator (spec §Diagnostics, validator codes).
import { MEDIA_TYPE, QUALIFIABLE, SINGLE_VALUED, STANDARD_KEYS, WS_CLASS, asciiLower, diag, trim, type Diagnostic, type JaiDocument } from './chars.ts';
import { srcIsSelected, srcsetCandidates, type Carrier } from './html.ts';
import { canonicalJSON } from './json.ts';
import type { DecodeResult } from './decode.ts';
import type { ElementLike } from './html.ts';
import { interpret, parseAspect } from './interpret.ts';
import type { ParseInfo } from './text.ts';

const CONFUSABLE_DELIM = new RegExp(`[\\uff1b\\u061b][${WS_CLASS}]*[A-Za-z0-9][A-Za-z0-9_.-]*(?:/[A-Za-z0-9][A-Za-z0-9_-]*)?[${WS_CLASS}]*[=\\uff1d]`);
const CONFUSABLE_EQ = /[A-Za-z0-9][\t ]*＝/;
const KNOWN_FOREIGN = /(^|\s)--(ar|no|seed|niji|style|v|q|chaos|stylize)\b|^Negative prompt:|<lora:/i;

export function validate(doc: JaiDocument, info?: ParseInfo | null): Diagnostic[] {
  const d: Diagnostic[] = [];
  const stdset = new Set(STANDARD_KEYS);
  const utf8 = (s: string): number => new TextEncoder().encode(s).length;
  for (const [k, vals] of doc) {
    const base = k.split('/')[0] ?? k;
    if (!base.includes('.') && !stdset.has(base)) d.push(diag('unknown-key', k));
    if (k.includes('/') && !QUALIFIABLE.has(base)) d.push(diag('invalid-qualifier', k));
    for (const v of vals) {
      if (/[\u0000-\u0008\u000b\u000e-\u001f\u007f-\u009f]/.test(v)) d.push(diag('control-character', k));
      if (/[​-‍⁠]|.﻿/s.test(v)) d.push(diag('invisible-char', k));
      if (/[‪-‮⁦-⁩]/.test(v)) d.push(diag('bidi-control', k));
      if (utf8(v) > 8192) d.push(diag('large-payload', k));
      if (/\n[\t\n\f\r ]*[A-Za-z0-9][A-Za-z0-9_.-]*(?:\/[A-Za-z0-9][A-Za-z0-9_-]*)?[\t\n\f\r ]*=/.test(v)) d.push(diag('newline-assignment', k));
      if (k === 'prompt' && /^jai:/i.test(v)) d.push(diag('prefix-in-prompt', k));
      if (k === 'prompt' && KNOWN_FOREIGN.test(v)) d.push(diag('foreign-syntax', k));
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

/** Checks on a data:image/jai URL written in a carrier (validator codes). */
function dataURLAdvisories(url: string): Diagnostic[] {
  const d: Diagnostic[] = [];
  const s = String(url).replace(/^[\u0000- ]+|[\u0000- ]+$/g, '');
  if (!/^data:image\/jai(?=[;,]|$)/i.test(s)) return d;
  const comma = s.indexOf(',');
  if (comma < 0) { d.push(diag('data-url-missing-comma')); return d; }
  const params = s.slice(5, comma).split(';').slice(1).map(p => asciiLower((p.trim().split('=')[0]) ?? ''));
  if (params.some(p => p && p !== 'base64' && p !== 'charset')) d.push(diag('data-url-parameter'));
  const body = s.slice(comma + 1);
  if (/[\t\n\r ]|#/.test(body) || /%(?![0-9A-Fa-f]{2})/.test(body)) d.push(diag('unencoded-url-text'));
  return d;
}

/**
 * Advisories about delivery: a decode() result (bytes from a file or fetched
 * resource; viaDataURL for a data: body) and/or a discovery carrier and element.
 */
export function validateDelivery({ decoded, carrier, element, viaDataURL = false }: { decoded?: (Pick<DecodeResult, 'document'> & Partial<Pick<DecodeResult, 'form'>>) | null; carrier?: Carrier; element?: ElementLike; viaDataURL?: boolean } = {}): Diagnostic[] {
  const d: Diagnostic[] = [];
  if (decoded && !viaDataURL && (decoded.form === 'json' || decoded.form === 'text')) d.push(diag('non-canonical-encoding'));
  const doc = decoded?.document;
  if (carrier === 'src' && element && !(element.getAttribute('width') && element.getAttribute('height'))) d.push(diag('missing-size'));
  if (carrier === 'attribute' && doc) { const p = doc.get('prompt'); if (p && p.length > 1 && p[0] === p[1] && element && element.getAttribute('alt') !== null) d.push(diag('alt-repeated')); }
  if ((carrier === 'picture' || carrier === 'src') && doc) { const m = doc.get('mode'); if (m && asciiLower(trim(m[m.length - 1] ?? '')) === 'keep') d.push(diag('keep-in-source')); }
  if (element) {
    const names = typeof element.getAttributeNames === 'function' ? element.getAttributeNames() : [];
    if (names.some(n => /["';=]/.test(n))) d.push(diag('attribute-split'));
    const src = element.getAttribute('src');
    if (src) d.push(...dataURLAdvisories(src));
    // A JAI src that HTML's image selection would never use (own srcset, or a preceding <source>).
    if (src && !srcIsSelected(element) && (/^\s*data:image\/jai(?=[;,]|$)/i.test(src) || /\.jai(?:[?#]|$)/i.test(src))) d.push(diag('src-shadowed'));
    const parent = element.parentElement;
    if (parent && parent.localName === 'picture') {
      let after = false;
      for (const ch of Array.from(parent.children as ArrayLike<ElementLike>)) {
        if (ch === element) { after = true; continue; }
        if (ch.localName !== 'source') continue;
        const type = ch.getAttribute('type'), ss = ch.getAttribute('srcset') ?? '';
        const isJai = asciiLower(((type ?? '').split(';')[0] ?? '').replace(/^[\t\n\f\r ]+|[\t\n\f\r ]+$/g, '')) === MEDIA_TYPE;
        const first = (ss.replace(/^[\t\n\f\r ,]+/, '').split(/[\t\n\f\r ]/)[0] ?? '').replace(/,+$/, '');
        const looksJai = /^data:image\/jai(?=[;,]|$)/i.test(first) || /\.jai(?:[?#]|$)/i.test(first);
        if (after) { if (isJai || looksJai) d.push(diag('source-after-img')); continue; }
        if (isJai && srcsetCandidates(ss).length === 0) d.push(diag('srcset-unparseable'));
        if (!type && looksJai) d.push(diag('source-missing-type'));
        if (isJai) d.push(...dataURLAdvisories(first));
      }
    }
  }
  return d;
}
