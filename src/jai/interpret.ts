// Interpretation of the standard keys and model matching (spec §Interpretation, §Model matching).
import { RASTER_TYPES, WS_CLASS, asciiLower, byCodePoint, diag, trim, type Diagnostic, type JaiDocument } from './chars.ts';
import { parseDataURL } from './decode.ts';

export interface Aspect { kind: 'ratio' | 'keyword'; width: number; height: number; keyword?: string; ratio?: number }

export interface CatalogModel {
  id: string;
  aliases?: string[];
  family?: string | null;
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
  /** What the selected model receives (spec §Per-model refinements): core values, then matching refinements. */
  modelPrompt: string[];
  modelDescription: string;
  modelStyle: string[];
  modelAvoid: string[];
  matched?: string | null;
  /** Ratio to render at (spec §Size and aspect); null when the preview or the renderer default decides. */
  ratio?: number | null;
}

const NUM_SRC = '(?:[0-9]+(?:\\.[0-9]*)?|\\.[0-9]+)';
const ASPECT_RATIO_RE = new RegExp(`^(${NUM_SRC})[${WS_CLASS}]*(?::|x|X|\\u00d7|/)[${WS_CLASS}]*(${NUM_SRC})$`);
const ASPECT_NUM_RE = new RegExp(`^${NUM_SRC}$`);
const STRENGTH_RE = new RegExp(`^(${NUM_SRC})[${WS_CLASS}]*(%)?$`);
const SEED_RE = /^[0-9]{1,20}$/;

function lexical(value: string): string {
  // Single-valued keys only: one surrounding pair of ASCII quotes, then one trailing full stop.
  let v = value;
  const q = v.charAt(0);
  if (v.length >= 2 && (q === '"' || q === "'") && v.charAt(v.length - 1) === q) v = trim(v.slice(1, -1));
  if (v.endsWith('.')) v = v.slice(0, -1);
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

export interface InterpretOptions { catalog?: CatalogModel[]; families?: Record<string, string[]>; hasOriginal?: boolean; governingRatio?: number | null; /** A catalog id the user chose; it replaces model matching. */ selected?: string | null }

export function interpret(doc: JaiDocument, { catalog = [], families = {}, hasOriginal = false, governingRatio = null, selected = null }: InterpretOptions = {}): { interpreted: Interpreted; diagnostics: Diagnostic[] } {
  const diagnostics: Diagnostic[] = [];
  const out: Interpreted = { prompt: null, description: '', style: [], avoid: [], model: [], aspect: null, mode: 'generate', strength: null, seed: null, source: null, preview: null, modelPrompt: [], modelDescription: '', modelStyle: [], modelAvoid: [] };
  const single = (key: string): string | null => {
    const vals = doc.get(key);
    if (!vals || !vals.length) return null;
    if (vals.length > 1) diagnostics.push(diag('repeated-single', key));
    return lexical(vals[vals.length - 1] ?? '');
  };
  const prompts = doc.get('prompt');
  if (prompts) { out.prompt = prompts.slice(); out.description = prompts.join('\n'); }
  // prompt, style, avoid and model are used exactly as written.
  out.style = (doc.get('style') ?? []).slice();
  out.avoid = (doc.get('avoid') ?? []).slice();
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
  }
  if ((v = single('seed')) !== null) {
    if (SEED_RE.test(v)) out.seed = v; else diagnostics.push(diag('invalid-value', 'seed'));
  }
  if ((v = single('source')) !== null) {
    let valid: boolean;
    if (/^data:/i.test(v)) { const d = parseDataURL(v); valid = !!d && RASTER_TYPES.has(d.essence); }
    else if (/^[A-Za-z][A-Za-z0-9+.-]*:/.test(v)) valid = /^https?:/i.test(v);
    else valid = true; // a relative URL
    if (!valid) diagnostics.push(diag('invalid-image', 'source'));
    else {
      out.source = v;
      if (out.mode !== 'transform') diagnostics.push(diag('ignored-value', 'source'));
    }
  }
  if ((v = single('preview')) !== null) {
    const d = /^data:/i.test(v) ? parseDataURL(v) : null;
    if (d && RASTER_TYPES.has(d.essence)) out.preview = v; else diagnostics.push(diag('unsupported-preview', 'preview'));
  }
  // Ratio to render at (spec §Size and aspect): the governing ratio wins over aspect.
  out.ratio = out.aspect ? out.aspect.width / out.aspect.height : null;
  if (governingRatio !== null && governingRatio > 0) {
    if (out.aspect) {
      const r = out.aspect.width / out.aspect.height;
      const differs = out.aspect.keyword === 'landscape' ? !(governingRatio > 1)
        : out.aspect.keyword === 'portrait' ? !(governingRatio < 1)
        : Math.abs(Math.log(r / governingRatio)) > 0.01;
      if (differs) diagnostics.push(diag('ignored-value', 'aspect'));
    }
    out.ratio = governingRatio;
  }
  let chosen: CatalogModel | null = null;
  if (catalog.length) {
    const needsImage = out.mode === 'transform' && (hasOriginal || out.source !== null);
    // `selected` lets a viewer impose the model the user picked; otherwise model matching decides.
    const forced = selected ? catalog.find(m => m.id === selected) ?? null : null;
    const m = forced ? { model: forced, diagnostics: [] as Diagnostic[] } : matchModel(out.model, { catalog, families, needsImage });
    chosen = m.model;
    out.matched = m.model ? m.model.id : null;
    diagnostics.push(...m.diagnostics);
    if (needsImage && m.model && !(m.model.imageInput || m.model.image_input)) diagnostics.push(diag('mode-degraded', 'mode'));
  }
  // Per-model refinements (spec §Per-model refinements): the selected model receives the core values,
  // then the values of matching qualified keys. The description (core) is what readers see.
  const names = chosen ? [chosen.family, chosen.id, ...(chosen.aliases ?? [])].filter((x): x is string => !!x).map(asciiLower) : [];
  const extraFor = (base: string): string[] => (chosen
    ? [...doc.keys()].filter(k => k.startsWith(base + '/') && names.includes(asciiLower(k.slice(base.length + 1)))).sort(byCodePoint).flatMap(k => doc.get(k) ?? [])
    : []);
  out.modelPrompt = [...(out.prompt ?? []), ...extraFor('prompt')];
  out.modelDescription = out.modelPrompt.join('\n');
  out.modelStyle = [...out.style, ...extraFor('style')];
  out.modelAvoid = [...out.avoid, ...extraFor('avoid')];
  return { interpreted: out, diagnostics };
}

const wordsOf = (s: string): string[] => asciiLower(s).match(/[a-z0-9]+/g) ?? [];

export interface MatchOptions { catalog?: CatalogModel[]; families?: Record<string, string[]>; needsImage?: boolean; pinsVersions?: boolean }

/** Model matching (spec §Model matching): a selector, never a locator. Only catalog models can be returned. */
export function matchModel(values: string[], { catalog = [], families = {}, needsImage = false, pinsVersions = false }: MatchOptions = {}): { model: CatalogModel | null; diagnostics: Diagnostic[] } {
  const diagnostics: Diagnostic[] = [];
  const imageOk = (m: CatalogModel): boolean => !!(m.imageInput || m.image_input);
  const eligible = needsImage ? catalog.filter(imageOk) : catalog.slice();
  const userDefault = catalog.find(m => m.default) ?? catalog[0] ?? null;
  const exact = (v: string): CatalogModel | null => {
    const lv = asciiLower(v);
    return eligible.find(m => asciiLower(m.id) === lv || (m.aliases ?? []).some(a => asciiLower(a) === lv)) ?? null;
  };
  const known: Array<{ name: string; spellings: string[] }> = Object.entries(families).map(([name, sp]) => ({ name, spellings: [name, ...(sp ?? [])] }));
  for (const m of catalog) if (m.family && !known.some(f => f.name === m.family)) known.push({ name: m.family, spellings: [m.family] });
  const familyDefault = (name: string): CatalogModel | null =>
    userDefault && eligible.includes(userDefault) && userDefault.family === name ? userDefault : eligible.find(m => m.family === name) ?? null;
  for (const v of values) {
    let m = exact(v);
    if (m) return { model: m, diagnostics };
    const slash = v.lastIndexOf('/');
    const seg = v.slice(slash + 1);
    if (!pinsVersions) {
      let cut = -1;
      for (let i = seg.length - 1; i >= 1; i--) if (seg.charAt(i) === ':' || seg.charAt(i) === '@') { cut = i; break; }
      if (cut >= 1) {
        m = exact(v.slice(0, slash + 1 + cut));
        if (m) { diagnostics.push(diag('version-ignored', 'model')); return { model: m, diagnostics }; }
      }
    }
    for (const candidate of [seg, v]) {
      const words = wordsOf(candidate);
      let best: { name: string; start: number; len: number; order: number } | null = null;
      known.forEach((f, order) => {
        if (!familyDefault(f.name)) return;
        for (const sp of f.spellings) {
          const sw = wordsOf(sp);
          if (!sw.length) continue;
          for (let i = 0; i + sw.length <= words.length; i++) {
            if (sw.every((w, k) => words[i + k] === w)) {
              if (!best || i < best.start || (i === best.start && (sw.length > best.len || (sw.length === best.len && order < best.order)))) best = { name: f.name, start: i, len: sw.length, order };
              break;
            }
          }
        }
      });
      const chosen = best as { name: string } | null;
      if (chosen) { diagnostics.push(diag('substituted-model', 'model', { family: chosen.name })); return { model: familyDefault(chosen.name), diagnostics }; }
    }
  }
  diagnostics.push(diag('no-matching-model', 'model'));
  return { model: userDefault, diagnostics };
}
