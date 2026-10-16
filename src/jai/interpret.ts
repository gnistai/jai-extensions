// Interpretation of the standard keys and model matching (spec §Interpretation, §Model matching).
import { RASTER_TYPES, WS_CLASS, asciiLower, diag, trim, type Diagnostic, type JaiDocument } from './chars.ts';
import { parseDataURL } from './decode.ts';

export interface Aspect { kind: 'ratio' | 'keyword'; width: number; height: number; keyword?: string; ratio?: number }

export interface CatalogModel {
  id: string;
  aliases?: string[];
  family?: string;
  imageInput?: boolean;
  image_input?: boolean;
  default?: boolean;
}

export interface Interpreted {
  prompt: string[] | null;
  description: string;
  style: string[];
  avoid: string[];
  model: string[];
  aspect: Aspect | null;
  mode: 'generate' | 'transform' | 'keep';
  strength: number | null;
  seed: string | null;
  source: string | null;
  preview: string | null;
  animate: boolean;
  matched?: string | null;
}

const NUM_SRC = '(?:[0-9]+(?:\\.[0-9]*)?|\\.[0-9]+)';
const ASPECT_RATIO_RE = new RegExp(`^(${NUM_SRC})[${WS_CLASS}]*(?::|x|X|\\u00d7|/)[${WS_CLASS}]*(${NUM_SRC})$`);
const ASPECT_NUM_RE = new RegExp(`^${NUM_SRC}$`);
const STRENGTH_RE = new RegExp(`^(${NUM_SRC})[${WS_CLASS}]*(%)?$`);
const SEED_RE = /^[0-9]{1,20}$/;

function lexical(key: string, value: string, diagnostics: Diagnostic[]): string {
  let v = value;
  const q = v.charAt(0);
  if (v.length >= 2 && (q === '"' || q === "'") && v.charAt(v.length - 1) === q) {
    diagnostics.push(diag('quoted-value', key)); v = trim(v.slice(1, -1));
  }
  if (key !== 'prompt' && key !== 'style' && key !== 'avoid' && v.endsWith('.')) v = v.slice(0, -1);
  return v;
}

function gcd(a: number, b: number): number { return b ? gcd(b, a % b) : a; }

export function parseAspect(v: string): Aspect | null {
  const lv = asciiLower(v);
  if (lv === 'square') return { kind: 'ratio', width: 1, height: 1, keyword: 'square' };
  if (lv === 'landscape') return { kind: 'keyword', keyword: 'landscape', width: 4, height: 3 };
  if (lv === 'portrait') return { kind: 'keyword', keyword: 'portrait', width: 3, height: 4 };
  const m = ASPECT_RATIO_RE.exec(v);
  let w: number, h: number;
  if (m) { w = parseFloat(m[1] ?? ''); h = parseFloat(m[2] ?? ''); }
  else if (ASPECT_NUM_RE.test(v)) { w = parseFloat(v); h = 1; }
  else return null;
  if (!(w > 0) || !(h > 0)) return null;
  const r = w / h;
  if (r > 100 || r < 0.01) return null;
  let rw = w, rh = h;
  if (Number.isInteger(w) && Number.isInteger(h)) { const d = gcd(w, h); rw = w / d; rh = h / d; }
  return { kind: 'ratio', width: rw, height: rh, ratio: r };
}

export interface InterpretOptions { catalog?: CatalogModel[]; families?: Record<string, string[]>; hasOriginal?: boolean }

export function interpret(doc: JaiDocument, { catalog = [], families = {}, hasOriginal = false }: InterpretOptions = {}): { interpreted: Interpreted; diagnostics: Diagnostic[] } {
  const diagnostics: Diagnostic[] = [];
  const out: Interpreted = { prompt: null, description: '', style: [], avoid: [], model: [], aspect: null, mode: 'generate', strength: null, seed: null, source: null, preview: null, animate: false };
  const single = (key: string): string | null => {
    const vals = doc.get(key);
    if (!vals || !vals.length) return null;
    if (vals.length > 1) diagnostics.push(diag('repeated-single', key));
    return lexical(key, vals[vals.length - 1] ?? '', diagnostics);
  };
  const prompts = doc.get('prompt');
  if (prompts) { out.prompt = prompts.slice(); out.description = prompts.join('\n'); }
  out.style = (doc.get('style') ?? []).slice();
  out.avoid = (doc.get('avoid') ?? []).slice();
  for (const a of out.avoid) if (/^(no|not|without|never)[\t\n\f\r  ]/i.test(a)) diagnostics.push(diag('negated-avoid', 'avoid'));
  out.model = (doc.get('model') ?? []).slice();

  let v: string | null;
  if ((v = single('aspect')) !== null) {
    const a = parseAspect(v);
    if (a) out.aspect = a; else diagnostics.push(diag('invalid-value', 'aspect'));
  }
  if ((v = single('mode')) !== null) {
    const lv = asciiLower(v);
    if (lv === 'generate' || lv === 'transform' || lv === 'keep') out.mode = lv; else diagnostics.push(diag('invalid-value', 'mode'));
  }
  if ((v = single('strength')) !== null) {
    const m = STRENGTH_RE.exec(v);
    if (m) {
      let n = parseFloat(m[1] ?? ''); if (m[2]) n /= 100;
      if (n < 0 || n > 1) { n = Math.min(1, Math.max(0, n)); diagnostics.push(diag('adjusted-value', 'strength')); }
      out.strength = n;
    } else diagnostics.push(diag('invalid-value', 'strength'));
    if (out.mode !== 'transform') diagnostics.push(diag('strength-without-transform', 'strength'));
  }
  if ((v = single('seed')) !== null) {
    if (SEED_RE.test(v)) out.seed = v; else diagnostics.push(diag('invalid-value', 'seed'));
  }
  if ((v = single('source')) !== null) {
    const d = /^data:/i.test(v) ? parseDataURL(v) : null;
    if (d) { if (RASTER_TYPES.has(d.essence)) out.source = v; else diagnostics.push(diag('invalid-image', 'source')); }
    else out.source = v;
    if (out.mode !== 'transform') diagnostics.push(diag('ignored-value', 'source'));
  }
  if ((v = single('preview')) !== null) {
    const d = /^data:/i.test(v) ? parseDataURL(v) : null;
    if (d && RASTER_TYPES.has(d.essence)) out.preview = v; else diagnostics.push(diag('unsupported-preview', 'preview'));
  }
  if ((v = single('animate')) !== null) {
    const lv = asciiLower(v);
    if (['true', 'yes', '1', 'on'].includes(lv)) out.animate = true;
    else if (['false', 'no', '0', 'off'].includes(lv)) out.animate = false;
    else diagnostics.push(diag('invalid-value', 'animate'));
  }
  if (catalog.length) {
    const m = matchModel(out.model, { catalog, families, needsImage: out.mode === 'transform' && (hasOriginal || out.source !== null) });
    out.matched = m.model ? m.model.id : null;
    diagnostics.push(...m.diagnostics);
  }
  return { interpreted: out, diagnostics };
}

const wordsOf = (s: string): string[] => asciiLower(s).match(/[a-z0-9]+/g) ?? [];

export interface MatchOptions { catalog?: CatalogModel[]; families?: Record<string, string[]>; needsImage?: boolean; pinsVersions?: boolean }

/** Model matching: selector, never locator. Only catalog models (the user's configuration) can be chosen. */
export function matchModel(values: string[], { catalog = [], families = {}, needsImage = false, pinsVersions = false }: MatchOptions = {}): { model: CatalogModel | null; diagnostics: Diagnostic[] } {
  const diagnostics: Diagnostic[] = [];
  const usable = needsImage ? catalog.filter(m => m.imageInput || m.image_input) : catalog;
  const familyOf = (name: string): CatalogModel | null => usable.find(m => m.family === name && m.default) ?? usable.find(m => m.family === name) ?? null;
  for (const raw of values) {
    let v = raw;
    if (!pinsVersions) {
      const stripped = v.replace(/[:@][^:@/]*$/, '');
      if (stripped !== v && stripped.length) { v = stripped; diagnostics.push(diag('version-ignored', 'model')); }
    }
    const lv = asciiLower(v);
    const exact = usable.find(m => asciiLower(m.id) === lv || (m.aliases ?? []).some(a => asciiLower(a) === lv));
    if (exact) return { model: exact, diagnostics };
    const last = v.split('/').pop() ?? v;
    for (const candidate of [last, v]) {
      const words = wordsOf(candidate);
      for (const [fam, spellings] of Object.entries(families)) {
        for (const sp of [fam, ...(spellings ?? [])]) {
          const sw = wordsOf(sp);
          if (!sw.length) continue;
          for (let i = 0; i + sw.length <= words.length; i++) {
            if (sw.every((w, k) => words[i + k] === w)) {
              const m = familyOf(fam);
              if (m) { diagnostics.push(diag('substituted-model', 'model', { family: fam })); return { model: m, diagnostics }; }
            }
          }
        }
      }
    }
  }
  diagnostics.push(diag('no-matching-model', 'model'));
  return { model: usable.find(m => m.default) ?? usable[0] ?? null, diagnostics };
}
