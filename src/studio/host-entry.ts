// Entry of dist/studio/host.mjs, the bundle that JAI Studio runs inside QuickJS
// (studio/crates/jai-js). It wires the JAI library and the encoder engine to the
// local server through LocalRenderer and LocalAnalyst. The host supplies the web
// APIs and globalThis.__jai (see globals.d.ts). See studio/DESIGN.md section 8.
import { LocalAnalyst } from '../encoder/local-providers.ts';
import { encode as runEncoder } from '../encoder/engine.ts';
import type { Candidate, EncodeEvent, ScoredRender, Thumbnail } from '../encoder/types.ts';
import * as J from '../jai/index.ts';
import { OpenAIChatClient, errorText } from '../providers/openai-chat.ts';
import { previewRatio } from '../renderers/image-size.ts';
import { LOCAL_FLUX, LocalRenderer } from '../renderers/local.ts';
import { FAMILIES } from '../renderers/replicate.ts';
import { INTERPRETER_CODES, type RenderResult } from '../renderers/types.ts';

/** jai-tools' package.json version, injected by scripts/build.mjs. */
declare const JAI_TOOLS_VERSION: string;

/**
 * A document: the bytes of a .jai file (decoded as the file form only), or text. Text that
 * is a data:image/jai URL (see readDocument) is decoded as its text body; any other text is
 * JAI text, parsed as the text form.
 */
export type DocInput = { bytes: Uint8Array } | { text: string };

export interface ServerRef { url: string; token: string }

export interface Diag { code: string; key?: string; requested?: string; actual?: string; reason?: string }

export interface ValidateResult { ok: boolean; fatal: boolean; text: string | null; diagnostics: Diag[] }

export interface RenderOptions { server: ServerRef; seed?: string | null; outputFormat?: 'png' | 'jpeg' }

export interface RenderOutput {
  bytes: Uint8Array; mime: string; model: string; modelLabel: string; sentPrompt: string; seed: number;
  width: number; height: number; description: string; documentText: string; diagnostics: Diag[];
}

export interface EncodeHostOptions {
  server: ServerRef;
  target: { url: string; width: number; height: number };
  targetThumbnail: { width: number; height: number; data: Uint8Array };
  generations: number; population: number; seeds: number[]; restarts: number; goal: number;
  timeLimitMs?: number; renderScale?: number; thinking?: 'off' | 'propose' | 'all'; sourceName?: string;
}

export interface EncodeOutput {
  text: string; jai: Uint8Array; stopReason: string; seconds: number;
  /** Why the run stopped early, when it stopped on an error (stopReason 'error') or with a message; else null. */
  error: string | null;
  best: { id: string; fitness: number };
  /** Same shape as the Node CLI's report.json; renders are named by file. */
  report: unknown;
}

export const version: string = JAI_TOOLS_VERSION;

/** Diagnostics that make a document unreadable, as `jai validate` treats them: a .jai file that is not the file form, or a data:image/jai body that is not UTF-8 text within the text bound. */
const FATAL = new Set(['invalid-json', 'malformed-container', 'undecodable']);
/** The one model Studio renders with, as a catalog entry. */
const CATALOG: J.CatalogModel[] = [{ id: LOCAL_FLUX.id, aliases: LOCAL_FLUX.aliases, family: LOCAL_FLUX.family, imageInput: LOCAL_FLUX.imageInput, default: true }];
const EXTENSIONS: Record<string, string> = { 'image/jpeg': 'jpg', 'image/png': 'png', 'image/webp': 'webp', 'image/gif': 'gif', 'image/avif': 'avif' };

// ------------------------------------------------------------ helpers

function toDiag(d: { code: string; key?: string; requested?: unknown; actual?: unknown; reason?: unknown }): Diag {
  const out: Diag = { code: d.code };
  for (const k of ['key', 'requested', 'actual', 'reason'] as const) {
    const v = d[k];
    if (v !== undefined && v !== null) out[k] = String(v);
  }
  return out;
}

/** Bytes from a Uint8Array, another typed array, an ArrayBuffer or an array of byte values; null otherwise. */
function toBytes(v: unknown): Uint8Array | null {
  if (v instanceof Uint8Array) return v;
  if (v instanceof ArrayBuffer) return new Uint8Array(v);
  if (ArrayBuffer.isView(v)) return new Uint8Array(v.buffer, v.byteOffset, v.byteLength);
  if (Array.isArray(v) && v.every(x => Number.isInteger(x) && x >= 0 && x <= 255)) return Uint8Array.from(v as number[]);
  return null;
}

const isPositiveInt = (v: unknown): v is number => typeof v === 'number' && Number.isSafeInteger(v) && v > 0;
const finite = (v: unknown): v is number => typeof v === 'number' && Number.isFinite(v);
/** A count option normalised as the engine does it (at least 1), or the default. */
const count = (v: unknown, d: number): number => (finite(v) ? Math.max(1, Math.floor(v)) : d);

function serverOf(s: ServerRef | undefined): ServerRef {
  if (!s || typeof s.url !== 'string' || !s.url || typeof s.token !== 'string') throw new TypeError('server must be {url, token}');
  return s;
}

/** A decoded document; `url` is the data:image/jai URL it came from, if any. */
interface ReadDocument { document: J.JaiDocument; diagnostics: J.Diagnostic[]; info?: J.ParseInfo; url?: string }

/**
 * As `jai validate` reads its argument. Bytes are a .jai file, decoded as the file form only.
 * Text that, after trimming C0 controls and spaces (as the URL parser does), begins with
 * `data:image/jai` and then `,` or `;` (ASCII case-insensitively) is a data:image/jai URL,
 * decoded as its text body; a URL that carries no document is refused. Any other text,
 * prose that begins with "Data:" and other data: URLs included, is parsed as the text form.
 */
function readDocument(input: DocInput): ReadDocument {
  if (input && typeof input === 'object' && 'text' in input && typeof input.text === 'string') {
    const url = J.trimBy(input.text, c => c <= 0x20);
    if (!/^data:image\/jai[,;]/i.test(url)) return J.parse(input.text);
    const r = J.decodeDataURL(url);
    if (!r) throw new Error('The document cannot be read: this data:image/jai URL carries no document (no "," or a ";base64" body that is not base64)');
    return { ...r, url };
  }
  const bytes = input && typeof input === 'object' && 'bytes' in input ? toBytes(input.bytes) : null;
  if (!bytes) throw new TypeError('A document is {bytes: Uint8Array} or {text: string}');
  return J.decode(bytes);
}

/** A seed the user chose, as the document's seed key takes it (1 to 20 digits); null for none. */
function seedOption(v: string | null | undefined): string | null {
  if (v === undefined || v === null) return null;
  const s = String(v).trim();
  if (!s) return null;
  if (!/^[0-9]{1,20}$/.test(s)) throw new RangeError(`seed must be 1 to 20 digits, not ${JSON.stringify(v)}`);
  return s;
}

function thumbnailOf(t: unknown, what: string): Thumbnail {
  const o = t && typeof t === 'object' ? (t as { width?: unknown; height?: unknown; data?: unknown }) : null;
  const data = o ? toBytes(o.data) : null;
  if (!o || !data || !isPositiveInt(o.width) || !isPositiveInt(o.height) || data.length !== o.width * o.height * 4) throw new TypeError(`${what} must be {width, height, data} with RGBA bytes`);
  return { width: o.width, height: o.height, data };
}

/** `001-l1g0c2-s1.jpg`: the order of completion, the candidate and the seed. Matches ^[A-Za-z0-9._-]{1,120}$. */
function renderName(n: number, candidate: string, seed: number, mime: string): string {
  const id = candidate.replace(/[^A-Za-z0-9._-]/g, '_').slice(0, 80);
  return `${String(n).padStart(3, '0')}-${id}-s${seed}.${EXTENSIONS[mime] ?? 'img'}`;
}

// ------------------------------------------------------------ entry points

/**
 * Mirrors `bin/jai.mjs validate`: the decoder's diagnostics, then a data:image/jai URL's own
 * advisories (validateDataURL), then the document's. Fatal codes: malformed-container,
 * invalid-json, undecodable; an unreadable document gets no canonical text and is not
 * validated (it decoded to the empty document, which would only report no-prompt).
 */
export function validate(input: DocInput): ValidateResult {
  const r = readDocument(input);
  const fatal = r.diagnostics.some(d => FATAL.has(d.code));
  const diagnostics = [...r.diagnostics, ...(r.url ? J.validateDataURL(r.url) : []), ...(fatal ? [] : J.validate(r.document, r.info))].map(toDiag);
  return { ok: !fatal, fatal, text: fatal ? null : J.serialize(r.document), diagnostics };
}

/**
 * Refuses an unreadable document as `validate` judges it; else interprets it with a one-model
 * catalog (FLUX schnell) and renders it with LocalRenderer, at the document's aspect, else its
 * preview's ratio, else 1:1 (a standalone document has no governing ratio).
 */
export async function render(input: DocInput, opts: RenderOptions): Promise<RenderOutput> {
  const server = serverOf(opts?.server);
  const r = readDocument(input);
  const fatal = r.diagnostics.find(d => FATAL.has(d.code));
  if (fatal) throw new Error(`The document cannot be read: ${fatal.code}${typeof fatal.reason === 'string' ? ` (${fatal.reason})` : ''}`);
  const { interpreted: it, diagnostics: found } = J.interpret(r.document, { catalog: CATALOG, families: FAMILIES });
  const renderer = new LocalRenderer(server.url, server.token, { outputFormat: opts.outputFormat === 'jpeg' ? 'jpeg' : 'png' });
  const result = await renderer.render({
    intent: {
      description: it.modelDescription, style: it.modelStyle, avoid: it.modelAvoid,
      aspect: it.aspect ? { width: it.aspect.width, height: it.aspect.height } : null,
      // A render the user asks for is a generate render, keep documents included (spec: Rendering modes).
      mode: it.mode === 'keep' ? 'generate' : it.mode,
      strength: it.strength, seed: seedOption(opts.seed) ?? it.seed,
    },
    modelId: LOCAL_FLUX.id, inputImage: null, governingRatio: null, previewRatio: previewRatio(it.preview), target: null,
  });
  const sent = result.sentInput;
  return {
    bytes: result.bytes, mime: result.mime, model: result.model, modelLabel: LOCAL_FLUX.label, sentPrompt: result.sentPrompt,
    // The server reports the seed it used; 0 only if a server failed to.
    seed: finite(sent.seed) ? sent.seed : 0,
    width: finite(sent.width) ? sent.width : 0,
    height: finite(sent.height) ? sent.height : 0,
    description: it.description, documentText: J.serialize(r.document),
    diagnostics: [...found.filter(d => INTERPRETER_CODES.has(d.code)), ...result.diagnostics].map(toDiag),
  };
}

/** Runs the encoder engine with LocalRenderer (JPEG) and LocalAnalyst at zero cost. */
export async function encode(opts: EncodeHostOptions): Promise<EncodeOutput> {
  const host = globalThis.__jai as JaiHostFunctions | undefined;
  if (!host || typeof host.thumbnail !== 'function' || typeof host.emit !== 'function' || typeof host.saveRender !== 'function') throw new Error('The host functions (globalThis.__jai) are missing');
  const server = serverOf(opts?.server);
  const t = opts.target;
  if (!t || typeof t.url !== 'string' || !isPositiveInt(t.width) || !isPositiveInt(t.height)) throw new TypeError('target must be {url, width, height}');
  const target = { url: t.url, width: t.width, height: t.height };
  const targetThumbnail = thumbnailOf(opts.targetThumbnail, 'targetThumbnail');
  const population = count(opts.population, 4);
  const generations = count(opts.generations, 8);
  const restarts = count(opts.restarts, 1);
  const goal = finite(opts.goal) ? opts.goal : 0.9;
  // The engine's own fallback, so that the report names the seeds it renders with.
  const given = Array.isArray(opts.seeds) ? opts.seeds.filter(x => Number.isSafeInteger(x) && x >= 0) : [];
  const seeds = given.length ? given : [1, 2];
  const thinking = opts.thinking === 'propose' || opts.thinking === 'all' ? opts.thinking : 'off';
  const renderScale = finite(opts.renderScale) && opts.renderScale > 0 ? opts.renderScale : 1;
  const timeLimitMs = finite(opts.timeLimitMs) && opts.timeLimitMs > 0 ? opts.timeLimitMs : undefined;

  const renderer = new LocalRenderer(server.url, server.token, { outputFormat: 'jpeg', scale: renderScale });
  const analyst = new LocalAnalyst(new OpenAIChatClient(server.url, server.token), { thinking });

  const send = (e: unknown): void => {
    try { host.emit(JSON.stringify(e)); } catch (err) { console.warn(`JAI host: an encode event was not delivered: ${errorText(err)}`); }
  };
  // Each render is saved once, when its candidate completes; events and the report name it by file.
  const files = new Map<ScoredRender, string>();
  let saved = 0;
  const save = (c: Candidate): void => {
    for (const r of c.renders) {
      if (!r.result || files.has(r)) continue;
      const name = renderName(++saved, c.id, r.seed, r.result.mime);
      try { host.saveRender(name, r.result.bytes); files.set(r, name); } catch (err) { send({ type: 'log', message: `Could not save render ${name}: ${errorText(err)}` }); }
    }
  };
  const onEvent = (e: EncodeEvent): void => {
    if (e.type !== 'candidate') return send(e);
    save(e.candidate);
    send({ type: 'candidate', candidate: { ...e.candidate, renders: e.candidate.renders.map(r => ({ model: r.model, seed: r.seed, file: files.get(r) ?? null, error: r.error, measure: r.measure, score: r.score })) } });
  };
  const thumbnail = async (r: RenderResult): Promise<Thumbnail> => {
    const thumb = thumbnailOf(host.thumbnail(r.bytes), 'A render thumbnail');
    // A size mismatch would fail the whole batch in measure(); throwing here only drops the colour terms of this render.
    if (thumb.width !== targetThumbnail.width || thumb.height !== targetThumbnail.height) throw new Error('thumbnail sizes differ');
    return thumb;
  };

  const ac = new AbortController();
  const previous = globalThis.__jaiAbort;
  globalThis.__jaiAbort = () => ac.abort();
  try {
    // The engine's default concurrency: at most four renders and four inspections
    // in flight. The scheduler still batches a whole generation per engine (each
    // queue stays filled until that kind of work runs out), while a request waits
    // only for the few ahead of it, never for a whole generation, which would
    // count against the providers' time limits.
    const result = await runEncoder({
      target, models: [LOCAL_FLUX.id], renderer, analyst, thumbnail, targetThumbnail,
      budgetUSD: 0, population, maxGenerations: generations, restarts, goal, seeds, refine: false,
      timeLimitMs, signal: ac.signal, onEvent,
    });
    const strip = (c: Candidate): unknown => ({
      ...c,
      renders: c.renders.map(r => ({ model: r.model, seed: r.seed, file: files.get(r) ?? null, error: r.error, observation: r.observation, measure: r.measure, score: r.score, sentPrompt: r.result?.sentPrompt ?? null })),
    });
    // Plain JSON (no undefined, NaN or shared references), so the Rust side can take it as serde_json::Value.
    const report: unknown = JSON.parse(JSON.stringify({
      source: opts.sourceName ?? null, size: [target.width, target.height], models: [LOCAL_FLUX.id], analyst: analyst.id, optimiser: analyst.id,
      settings: { population, generations, seeds, restarts, goal, thinking, renderScale, timeLimitMs: timeLimitMs ?? null },
      target: result.target, budgetUSD: 0, text: result.text, ranking: result.ranking, stopReason: result.stopReason, error: result.error,
      spentEstimateUSD: result.spent, seconds: result.seconds, generations: result.generations, best: strip(result.best),
      candidates: result.candidates.map(strip), spend: result.spend,
    }));
    return {
      text: result.text, jai: await J.encode(result.document), stopReason: result.stopReason, seconds: result.seconds,
      error: result.error, best: { id: result.best.id, fitness: result.best.fitness }, report,
    };
  } finally {
    globalThis.__jaiAbort = previous;
  }
}
