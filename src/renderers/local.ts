// Local renderer: JAI Studio's image endpoint (FLUX.1 schnell on
// stable-diffusion.cpp). It presents the Replicate flux-schnell model, so
// documents encoded locally name the same model, and it assembles the same
// prompt text Replicate would get. See studio/DESIGN.md section 9.
import { ProviderError, errorText, postJSON } from '../providers/openai-chat.ts';
import type { FetchLike } from '../providers/replicate-client.ts';
import { assemblePrompt, nearestAspect, ratioOf } from './prompt.ts';
import { sniffImage } from './image-size.ts';
import { REPLICATE_MODELS } from './replicate.ts';
import { RenderError, type ModelInfo, type RenderDiagnostic, type RenderRequest, type RenderResult, type Renderer } from './types.ts';

function replicateInfo(id: string): ModelInfo {
  const m = REPLICATE_MODELS.find(x => x.id === id);
  if (!m) throw new Error(`${id} is missing from REPLICATE_MODELS`);
  return { id: m.id, label: m.label, family: m.family, aliases: [...m.aliases], imageInput: m.imageInput, negativePrompt: m.negativePrompt, shortWindow: m.shortWindow, costHint: 0, description: m.description };
}

/** The Replicate flux-schnell entry's ModelInfo fields, with costHint 0. */
export const LOCAL_FLUX: ModelInfo = replicateInfo('black-forest-labs/flux-schnell');

/** Width and height for each FLUX aspect; every side is a multiple of 64. */
export const FLUX_SIZES: Record<string, [number, number]> = {
  '1:1': [1024, 1024], '16:9': [1344, 768], '21:9': [1536, 640], '3:2': [1216, 832], '2:3': [832, 1216], '4:5': [896, 1088],
  '5:4': [1088, 896], '3:4': [896, 1152], '4:3': [1152, 896], '9:16': [768, 1344], '9:21': [640, 1536],
};

// The server's limits (DESIGN.md section 7, POST /v1/images/generations).
const MIN_SIDE = 256;
const MAX_SIDE = 2048;
const MAX_PIXELS = 2_359_296;
const MAX_PROMPT_CHARS = 8000;
const MAX_STEPS = 16;
const SEED_RANGE = 2n ** 32n;
/** The server gives up on a generation after 10 minutes; queueing behind other work comes on top. */
const IMAGE_TIMEOUT_MS = 900_000;

/** How far (log) a render size's ratio may be from its FLUX size's; the FLUX sizes keep to their aspects as closely. */
const RATIO_TOLERANCE = 0.05;

/**
 * A FLUX size at `scale` (an area of width × height × scale², at most the
 * server's): of the sizes the server takes (multiples of 64 from 256 to 2048,
 * within its pixel limit), the one within RATIO_TOLERANCE of the FLUX size's
 * ratio that comes closest to that area, a ratio error weighing four times an
 * area error, since the composition matters more than the pixel count. Rounding
 * each side by itself would distort small sizes: 16:9 at 0.25 would be 320×256.
 * Scale 1 gives the FLUX size itself.
 */
function scaledSize(width: number, height: number, scale: number): { width: number; height: number } {
  const ratio = width / height;
  const area = Math.min(MAX_PIXELS, width * height * scale * scale);
  let best = { width: MIN_SIDE, height: MIN_SIDE }, bestCost = Infinity;
  for (let w = MIN_SIDE; w <= MAX_SIDE; w += 64) {
    for (let h = MIN_SIDE; h <= MAX_SIDE && w * h <= MAX_PIXELS; h += 64) {
      const ratioError = Math.abs(Math.log(w / h / ratio));
      // Outside the tolerance only when no size is inside it (never for a FLUX size).
      const cost = ratioError + Math.abs(Math.log((w * h) / area)) / 4 + (ratioError > RATIO_TOLERANCE ? 1000 : 0);
      if (cost < bestCost) { best = { width: w, height: h }; bestCost = cost; }
    }
  }
  return best;
}

const B64_TABLE = (() => {
  const t = new Uint8Array(128).fill(255);
  const a = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/';
  for (let i = 0; i < 64; i++) t[a.charCodeAt(i)] = i;
  return t;
})();
const NATIVE_FROM_BASE64 = (Uint8Array as unknown as { fromBase64?: (s: string) => Uint8Array }).fromBase64;
const isSpace = (c: number): boolean => c === 32 || c === 9 || c === 10 || c === 12 || c === 13;

/**
 * Standard base64 (padding optional, ASCII whitespace ignored) to bytes.
 * Uses Uint8Array.fromBase64 where the engine has it (QuickJS-ng does); the
 * table decoder below handles a few megabytes quickly without atob or Buffer.
 * Throws RangeError on anything else.
 */
export function base64ToBytes(s: string): Uint8Array {
  if (typeof NATIVE_FROM_BASE64 === 'function') {
    try { return NATIVE_FROM_BASE64.call(Uint8Array, s); } catch { /* the decoder below reports the error */ }
  }
  let end = s.length;
  while (end > 0 && isSpace(s.charCodeAt(end - 1))) end--;
  for (let pad = 0; pad < 2 && end > 0 && s.charCodeAt(end - 1) === 61; pad++) end--;
  const out = new Uint8Array(Math.floor((end * 3) / 4));
  const T = B64_TABLE;
  let o = 0, i = 0, acc = 0, bits = 0;
  // Fast path: four characters to three bytes; any whitespace or invalid character drops to the loop below.
  for (; i + 4 <= end; i += 4) {
    const c0 = s.charCodeAt(i), c1 = s.charCodeAt(i + 1), c2 = s.charCodeAt(i + 2), c3 = s.charCodeAt(i + 3);
    if ((c0 | c1 | c2 | c3) > 127) break;
    const a = T[c0]!, b = T[c1]!, c = T[c2]!, d = T[c3]!;
    if ((a | b | c | d) > 63) break;
    out[o++] = (a << 2) | (b >> 4);
    out[o++] = ((b & 15) << 4) | (c >> 2);
    out[o++] = ((c & 3) << 6) | d;
  }
  for (; i < end; i++) {
    const c = s.charCodeAt(i);
    const v = c < 128 ? T[c]! : 255;
    if (v === 255) {
      if (isSpace(c)) continue;
      throw new RangeError('Invalid base64');
    }
    acc = ((acc << 6) | v) & 0xffffff;
    bits += 6;
    if (bits >= 8) { bits -= 8; out[o++] = (acc >> bits) & 0xff; }
  }
  if (bits >= 6) throw new RangeError('Invalid base64 length');
  return o === out.length ? out : out.subarray(0, o);
}

interface LocalRequest {
  /** The request body for POST /v1/images/generations. */
  body: { prompt: string; width: number; height: number; seed?: number; steps?: number; output_format: 'png' | 'jpeg' };
  diagnostics: RenderDiagnostic[];
}

/** Map a render request to the local server's request body. Pure. */
function buildRequest(req: RenderRequest, opts: { outputFormat?: 'png' | 'jpeg'; scale?: number; steps?: number }): LocalRequest {
  const { intent } = req;
  const assembled = assemblePrompt(intent, LOCAL_FLUX, { foldAvoid: true, maxChars: MAX_PROMPT_CHARS });
  const diagnostics: RenderDiagnostic[] = [...assembled.diagnostics];
  let prompt = assembled.prompt;
  if (prompt.length > MAX_PROMPT_CHARS) {
    // Only style and avoid text are left to cut; never split a surrogate pair.
    const cut = prompt.charCodeAt(MAX_PROMPT_CHARS - 1) >= 0xd800 && prompt.charCodeAt(MAX_PROMPT_CHARS - 1) <= 0xdbff ? MAX_PROMPT_CHARS - 1 : MAX_PROMPT_CHARS;
    prompt = prompt.slice(0, cut).trimEnd();
    if (!diagnostics.some(d => d.code === 'adjusted-value' && d.key === 'prompt')) diagnostics.push({ code: 'adjusted-value', key: 'prompt' });
  }

  // Ratio (spec: Size and aspect): the governing ratio, else aspect, else the preview's, else 1:1; snapped to the nearest FLUX size.
  const ratio = req.governingRatio ?? (intent.aspect ? intent.aspect.width / intent.aspect.height : null) ?? req.previewRatio;
  const key = nearestAspect(ratio ?? 1, Object.keys(FLUX_SIZES));
  if (intent.aspect && ratio && !req.governingRatio && Math.abs(Math.log(ratioOf(key) / ratio)) > 0.05) diagnostics.push({ code: 'adjusted-value', key: 'aspect', requested: `${intent.aspect.width}:${intent.aspect.height}`, actual: key });
  const [bw, bh] = FLUX_SIZES[key] ?? [1024, 1024];
  const scale = typeof opts.scale === 'number' && Number.isFinite(opts.scale) && opts.scale > 0 ? opts.scale : 1;
  const { width, height } = scaledSize(bw, bh, scale);

  // FLUX.1 schnell takes no input image, so transform renders as generate, reported as the Replicate renderer does.
  if (intent.mode === 'transform') diagnostics.push(req.inputImage ? { code: 'mode-degraded', key: 'mode' } : { code: 'original-not-readable', key: 'mode' });

  const body: LocalRequest['body'] = { prompt, width, height, output_format: opts.outputFormat === 'jpeg' ? 'jpeg' : 'png' };
  if (intent.seed !== null) {
    const digits = intent.seed.trim();
    if (!/^[0-9]+$/.test(digits)) throw new RenderError(`Invalid seed ${JSON.stringify(intent.seed)}`, 'config');
    const big = BigInt(digits);
    const seed = Number(big % SEED_RANGE);
    if (big >= SEED_RANGE) diagnostics.push({ code: 'adjusted-value', key: 'seed', requested: intent.seed, actual: String(seed) });
    body.seed = seed;
  }
  if (typeof opts.steps === 'number' && Number.isFinite(opts.steps)) body.steps = Math.min(MAX_STEPS, Math.max(1, Math.round(opts.steps)));
  return { body, diagnostics };
}

const intOf = (v: unknown): number | null => (typeof v === 'number' && Number.isSafeInteger(v) && v >= 0 ? v : null);

export class LocalRenderer implements Renderer {
  readonly id = 'local';
  readonly label = 'JAI Studio';
  private readonly base: string;

  constructor(baseUrl: string, private readonly token: string, private readonly opts: { fetch?: FetchLike; outputFormat?: 'png' | 'jpeg'; scale?: number; steps?: number } = {}) {
    this.base = baseUrl.replace(/\/+$/, '');
  }

  models(): ModelInfo[] { return [LOCAL_FLUX]; }

  private get fetch(): FetchLike { return this.opts.fetch ?? ((u, i) => fetch(u, i)); }

  async render(req: RenderRequest): Promise<RenderResult> {
    if (req.modelId !== LOCAL_FLUX.id && !LOCAL_FLUX.aliases.includes(req.modelId)) throw new RenderError(`Model ${req.modelId} is not supported by this renderer`, 'config');
    const { body, diagnostics } = buildRequest(req, this.opts);
    // The server needs some text; with none, a provider default would be rendered (spec: Assembling the model input).
    if (!body.prompt.trim()) throw new RenderError('Nothing to render: the document has no description, style or avoid values', 'refused');
    req.onStatus?.('starting');
    let reply: Record<string, unknown>;
    try {
      reply = await postJSON(this.fetch, `${this.base}/v1/images/generations`, this.token, body, { what: 'Image generation', timeoutMs: IMAGE_TIMEOUT_MS, signal: req.signal });
    } catch (e) {
      if (e instanceof ProviderError) throw new RenderError(e.message, e.kind);
      throw new RenderError(errorText(e), 'provider');
    }
    const item = Array.isArray(reply.data) ? (reply.data[0] as Record<string, unknown> | undefined) : undefined;
    const raw = item && typeof item.b64_json === 'string' ? item.b64_json : '';
    if (!raw) throw new RenderError('The server returned no image', 'provider');
    const b64 = /\s/.test(raw) ? raw.replace(/\s+/g, '') : raw;
    let bytes: Uint8Array;
    try { bytes = base64ToBytes(b64); } catch { throw new RenderError('The server returned an image that is not valid base64', 'provider'); }
    const mime = sniffImage(bytes);
    if (!mime) throw new RenderError('The server returned something that is not an image', 'provider');
    // With no seed in the request the server picks one; its reply reports the seed used.
    const timings = reply.timings && typeof reply.timings === 'object' ? (reply.timings as Record<string, unknown>) : {};
    const generateMs = typeof timings.generateMs === 'number' && Number.isFinite(timings.generateMs) ? timings.generateMs : null;
    const sentInput: Record<string, unknown> = {
      prompt: body.prompt,
      width: intOf(reply.width) ?? body.width,
      height: intOf(reply.height) ?? body.height,
      seed: intOf(item?.seed) ?? body.seed ?? null,
      steps: intOf(reply.steps) ?? body.steps ?? null,
      output_format: body.output_format,
    };
    return {
      bytes, mime, model: LOCAL_FLUX.id, sentPrompt: body.prompt, sentNegative: null, sentInput, jobId: null, diagnostics,
      outputURL: `data:${mime};base64,${b64}`, predictSeconds: generateMs === null ? null : generateMs / 1000,
    };
  }
}
