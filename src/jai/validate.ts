// Advisory validator (spec §Diagnostics, validator codes).
import { SINGLE_VALUED, STANDARD_KEYS, WS_CLASS, asciiLower, diag, trim, type Diagnostic, type JaiDocument } from './chars.ts';
import type { DecodeResult } from './decode.ts';
import type { ElementLike } from './html.ts';
import { interpret, parseAspect } from './interpret.ts';
import type { ParseInfo } from './text.ts';

const CONFUSABLE_DELIM = new RegExp(`[\\uff1b\\u061b][${WS_CLASS}]*[A-Za-z0-9][A-Za-z0-9_.-]*[${WS_CLASS}]*[=\\uff1d]`);
const CONFUSABLE_EQ = /[A-Za-z0-9][\t ]*＝/;
const KNOWN_FOREIGN = /(^|\s)--(ar|no|seed|niji|style|v|q|chaos|stylize)\b|^Negative prompt:|<lora:/i;

export function validate(doc: JaiDocument, info?: ParseInfo | null): Diagnostic[] {
  const d: Diagnostic[] = [];
  const stdset = new Set(STANDARD_KEYS);
  for (const [k, vals] of doc) {
    if (!k.includes('.') && !stdset.has(k)) d.push(diag('unknown-key', k));
    for (const v of vals) {
      if (/[\u0000-\u0008\u000b\u000e-\u001f\u007f-\u009f]/.test(v)) d.push(diag('control-character', k));
      if (/[​-‍⁠]|.﻿/s.test(v)) d.push(diag('invisible-char', k));
      if (/[‪-‮⁦-⁩]/.test(v)) d.push(diag('bidi-control', k));
      if (v.length > 8192) d.push(diag('large-payload', k));
      if (/\n[\t\n\f\r ]*[A-Za-z0-9][A-Za-z0-9_.-]*[\t\n\f\r ]*=/.test(v)) d.push(diag('newline-assignment', k));
      if (k === 'prompt' && /^jai:/i.test(v)) d.push(diag('prefix-in-prompt', k));
      if (k === 'prompt' && KNOWN_FOREIGN.test(v)) d.push(diag('foreign-syntax', k));
      if (CONFUSABLE_DELIM.test(v)) d.push(diag('confusable-delimiter', k));
      else if (CONFUSABLE_EQ.test(v)) d.push(diag('confusable-equals', k));
      if (!['source', 'preview', 'model'].includes(k) && !/^[a-z][a-z0-9+.-]*:/i.test(v) && /%[0-9A-Fa-f]{2}/.test(v) && !/%25/i.test(v)) d.push(diag('percent-encoded-text', k));
    }
  }
  if (!doc.has('prompt')) {
    const mode = doc.get('mode');
    const m = mode ? asciiLower(mode[mode.length - 1] ?? '') : 'generate';
    if (m === 'generate' && !doc.has('source')) d.push(diag('no-prompt'));
  }
  for (const v of doc.get('model') ?? []) if (/^[a-z][a-z0-9+.-]*:\/\/|^\.?\.?\//i.test(v)) d.push(diag('model-looks-like-locator', 'model'));
  if (info?.segments) {
    const segs = info.segments;
    const first = segs[0];
    if (first && first.kind === 'assignment' && !stdset.has(first.key) && !first.key.includes('.') && !doc.has('prompt')) d.push(diag('first-segment-assignment', first.key));
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
  for (const x of interpret(doc).diagnostics) if (['repeated-single', 'invalid-value', 'negated-avoid', 'strength-without-transform', 'quoted-value'].includes(x.code)) d.push(x);
  const aspect = doc.get('aspect');
  if (aspect) {
    const v = aspect[aspect.length - 1] ?? '';
    const a = parseAspect(v);
    if (a && a.kind === 'ratio' && !/^[0-9]+:[0-9]+$/.test(v) && v.toLowerCase() !== 'square') d.push(diag('nonstandard-value', 'aspect', { suggestion: `${a.width}:${a.height}` }));
  }
  return d;
}

/** Advisories about delivery: decode() result and/or discovery carrier. */
export function validateDelivery({ decoded, carrier, element }: { decoded?: Pick<DecodeResult, 'document'> & Partial<Pick<DecodeResult, 'form'>> | null; carrier?: number; element?: ElementLike } = {}): Diagnostic[] {
  const d: Diagnostic[] = [];
  if (decoded && (decoded.form === 'json' || decoded.form === 'text')) d.push(diag('non-canonical-encoding'));
  if (carrier === 4) {
    d.push(diag('alt-carrier'));
    if (decoded?.document) for (const vals of decoded.document.values()) if (vals.some(v => /data:/i.test(v))) { d.push(diag('alt-carrier-payload')); break; }
  }
  if (carrier === 3 && element && !(element.getAttribute('width') && element.getAttribute('height'))) d.push(diag('missing-size'));
  return d;
}
