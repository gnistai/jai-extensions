// Replicate renderer. All model-specific knowledge (input names, aspect enums,
// negative prompts, strength polarity) lives here, not in the JAI standard.
// Requests carry only the mapped parameters and locally re-encoded image bytes;
// nothing from the document can choose an endpoint, a model outside this list,
// safety settings, output counts or weights (spec: selectors, never locators).
import { assemblePrompt, nearestAspect, ratioOf } from './prompt.ts';
import { RenderError, type ModelInfo, type RenderDiagnostic, type RenderRequest, type RenderResult, type Renderer } from './types.ts';

interface ReplicateModel extends ModelInfo {
  /** owner/name of an official model, called through /models/{id}/predictions */
  official: boolean;
  /** pinned version for community models */
  version?: string;
  aspects?: string[];
  /** width/height inputs (multiples of 8) instead of an aspect enum */
  sizeInputs?: boolean;
  /** input name for the image in transform mode */
  imageField?: string;
  /** input name for strength, and whether it measures departure (true) or influence (false) */
  strengthField?: { name: string; departure: boolean };
  outputFormat?: { field: string; value: string };
  extra?: Record<string, unknown>;
  seedMax: number;
  maxPromptChars?: number;
}

const FLUX_ASPECTS = ['1:1', '16:9', '21:9', '3:2', '2:3', '4:5', '5:4', '3:4', '4:3', '9:16', '9:21'];

export const REPLICATE_MODELS: ReplicateModel[] = [
  {
    id: 'black-forest-labs/flux-schnell', label: 'FLUX.1 schnell', family: 'flux', aliases: ['flux-schnell'],
    imageInput: false, negativePrompt: false, shortWindow: false, costHint: 0.003, official: true,
    description: 'Fast and cheap; good default for text-to-image.',
    aspects: FLUX_ASPECTS, outputFormat: { field: 'output_format', value: 'webp' }, extra: { output_quality: 85, num_outputs: 1 }, seedMax: 2 ** 32 - 1,
  },
  {
    id: 'black-forest-labs/flux-dev', label: 'FLUX.1 dev', family: 'flux', aliases: ['flux-dev'],
    imageInput: true, negativePrompt: false, shortWindow: false, costHint: 0.025, official: true,
    description: 'Higher quality; supports image-to-image with a strength control.',
    aspects: FLUX_ASPECTS, imageField: 'image', strengthField: { name: 'prompt_strength', departure: true },
    outputFormat: { field: 'output_format', value: 'webp' }, extra: { output_quality: 85, num_outputs: 1 }, seedMax: 2 ** 32 - 1,
  },
  {
    id: 'black-forest-labs/flux-kontext-pro', label: 'FLUX.1 Kontext pro', family: 'flux', aliases: ['flux-kontext', 'kontext'],
    imageInput: true, negativePrompt: false, shortWindow: false, costHint: 0.04, official: true,
    description: 'Edits an input image following the description; best for mode=transform.',
    aspects: ['1:1', '16:9', '9:16', '4:3', '3:4', '3:2', '2:3', '4:5', '5:4', '21:9', '9:21', '2:1', '1:2'], imageField: 'input_image',
    outputFormat: { field: 'output_format', value: 'png' }, seedMax: 2 ** 32 - 1,
  },
  {
    id: 'stability-ai/sdxl', label: 'Stable Diffusion XL', family: 'sdxl', aliases: ['sdxl', 'stable-diffusion-xl'],
    imageInput: true, negativePrompt: true, shortWindow: true, costHint: 0.005, official: false,
    version: '7762fd07cf82c948538e41f63f77d685e02b063e37e496e96eefd46c929f9bdc',
    description: 'Classic open model with a negative prompt; short prompt window.',
    sizeInputs: true, imageField: 'image', strengthField: { name: 'prompt_strength', departure: true },
    extra: { apply_watermark: false, num_outputs: 1 }, seedMax: 2 ** 32 - 1,
  },
];

export const FAMILIES: Record<string, string[]> = { flux: ['flux', 'kontext'], sdxl: ['sdxl', 'stable diffusion xl'] };

const toB64 = (bytes: Uint8Array): string => {
  let s = '';
  for (let i = 0; i < bytes.length; i += 0x8000) s += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
  return btoa(s);
};

export interface BuiltInput { input: Record<string, unknown>; shown: Record<string, unknown>; diagnostics: RenderDiagnostic[]; prompt: string; negative: string | null }

/** Map a render request to a Replicate input object. Pure; exported for tests. */
export function buildInput(model: ReplicateModel, req: RenderRequest, opts: { foldAvoid: boolean }): BuiltInput {
  const { intent } = req;
  const assembled = assemblePrompt(intent, model, { foldAvoid: opts.foldAvoid, maxChars: model.maxPromptChars });
  const diagnostics: RenderDiagnostic[] = [...assembled.diagnostics];
  const input: Record<string, unknown> = { prompt: assembled.prompt, ...(model.extra ?? {}) };
  if (assembled.negative !== null) input.negative_prompt = assembled.negative;
  if (model.outputFormat) input[model.outputFormat.field] = model.outputFormat.value;

  // Ratio (spec: Size and aspect): the element's governing ratio, else aspect, else the model default.
  // Whether aspect was ignored is the interpreter's decision (library), not reported again here.
  const ratio: number | null = req.governingRatio ?? (intent.aspect ? intent.aspect.width / intent.aspect.height : null);

  const transform = intent.mode === 'transform' && req.inputImage !== null;
  if (model.sizeInputs) {
    const r = ratio ?? 1, px = 1024 * 1024;
    input.width = Math.round(Math.sqrt(px * r) / 8) * 8;
    input.height = Math.round(Math.sqrt(px / r) / 8) * 8;
  } else if (model.aspects) {
    if (transform && model.imageField === 'input_image') input.aspect_ratio = 'match_input_image';
    else if (ratio) {
      const a = nearestAspect(ratio, model.aspects);
      input.aspect_ratio = a;
      if (intent.aspect && !req.governingRatio && Math.abs(Math.log(ratioOf(a) / ratio)) > 0.05) diagnostics.push({ code: 'adjusted-value', key: 'aspect', requested: `${intent.aspect.width}:${intent.aspect.height}`, actual: a });
    }
  }

  const shown: Record<string, unknown> = { ...input };
  if (transform && req.inputImage) {
    if (!model.imageInput || !model.imageField) diagnostics.push({ code: 'mode-degraded', key: 'mode' });
    else {
      input[model.imageField] = `data:${req.inputImage.mime};base64,${toB64(req.inputImage.bytes)}`;
      shown[model.imageField] = `(input image, ${req.inputImage.bytes.length} bytes, ${req.inputImage.mime})`;
      if (intent.strength !== null) {
        if (model.strengthField) {
          const v = model.strengthField.departure ? intent.strength : 1 - intent.strength;
          input[model.strengthField.name] = v; shown[model.strengthField.name] = v;
        } else diagnostics.push({ code: 'unsupported-parameter', key: 'strength' });
      }
    }
  } else if (intent.mode === 'transform') diagnostics.push({ code: 'original-not-readable', key: 'mode' });

  if (intent.seed !== null) {
    const big = BigInt(intent.seed);
    const max = BigInt(model.seedMax);
    const s = big > max ? Number(big % (max + 1n)) : Number(big);
    if (big > max) diagnostics.push({ code: 'adjusted-value', key: 'seed', requested: intent.seed, actual: String(s) });
    input.seed = s; shown.seed = s;
  }
  if (intent.animate) diagnostics.push({ code: 'unsupported-parameter', key: 'animate' });
  return { input, shown, diagnostics, prompt: assembled.prompt, negative: assembled.negative };
}

type FetchLike = (url: string, init?: RequestInit) => Promise<Response>;

interface Prediction { id: string; status: string; output?: unknown; error?: unknown; version?: string; urls?: { cancel?: string } }

export class ReplicateRenderer implements Renderer {
  readonly id = 'replicate';
  readonly label = 'Replicate';
  constructor(private readonly token: string, private readonly opts: { foldAvoid: boolean; fetch?: FetchLike; pollMs?: number; timeoutMs?: number } = { foldAvoid: true }) {}

  models(): ModelInfo[] { return REPLICATE_MODELS; }

  private get fetch(): FetchLike { return this.opts.fetch ?? ((u, i) => fetch(u, i)); }

  private async api<T>(path: string, init: RequestInit = {}, signal?: AbortSignal): Promise<T> {
    let res: Response;
    try {
      res = await this.fetch('https://api.replicate.com/v1' + path, {
        ...init, signal, referrerPolicy: 'no-referrer', credentials: 'omit',
        headers: { Authorization: `Bearer ${this.token}`, 'Content-Type': 'application/json', ...(init.headers as Record<string, string> | undefined) },
      });
    } catch (e) {
      if ((e as Error).name === 'AbortError') throw new RenderError('Canceled', 'canceled');
      throw new RenderError('Network error contacting Replicate', 'network');
    }
    const body = (await res.json().catch(() => ({}))) as Record<string, unknown>;
    if (res.status === 401 || res.status === 403) throw new RenderError('Replicate rejected the API token', 'auth');
    if (!res.ok) throw new RenderError(`Replicate error ${res.status}: ${String(body.detail ?? body.title ?? '')}`.trim(), res.status === 422 ? 'refused' : 'provider');
    return body as T;
  }

  async cancel(id: string): Promise<void> {
    try { await this.api(`/predictions/${encodeURIComponent(id)}/cancel`, { method: 'POST' }); } catch { /* best effort */ }
  }

  async render(req: RenderRequest): Promise<RenderResult> {
    const model = REPLICATE_MODELS.find(m => m.id === req.modelId);
    if (!model) throw new RenderError(`Model ${req.modelId} is not supported by this renderer`, 'config');
    const built = buildInput(model, req, { foldAvoid: this.opts.foldAvoid });
    const path = model.official ? `/models/${model.id}/predictions` : '/predictions';
    const payload = model.official ? { input: built.input } : { version: model.version, input: built.input };
    req.onStatus?.('starting');
    let p = await this.api<Prediction>(path, { method: 'POST', body: JSON.stringify(payload), headers: { Prefer: 'wait=5' } }, req.signal);
    const started = Date.now();
    const onAbort = (): void => { void this.cancel(p.id); };
    req.signal?.addEventListener('abort', onAbort, { once: true });
    try {
      while (!['succeeded', 'failed', 'canceled'].includes(p.status)) {
        if (req.signal?.aborted) throw new RenderError('Canceled', 'canceled', p.id);
        if (Date.now() - started > (this.opts.timeoutMs ?? 180_000)) { void this.cancel(p.id); throw new RenderError('Timed out', 'timeout', p.id); }
        req.onStatus?.(p.status);
        await new Promise(r => setTimeout(r, this.opts.pollMs ?? 1500));
        p = await this.api<Prediction>(`/predictions/${encodeURIComponent(p.id)}`, {}, req.signal);
      }
    } finally {
      req.signal?.removeEventListener('abort', onAbort);
    }
    if (p.status === 'canceled') throw new RenderError('Canceled', 'canceled', p.id);
    if (p.status === 'failed') {
      const msg = String(p.error ?? 'failed');
      throw new RenderError(msg, /nsfw|safety|flagged|content/i.test(msg) ? 'refused' : 'provider', p.id);
    }
    const url = Array.isArray(p.output) ? p.output[0] : p.output;
    if (typeof url !== 'string' || !/^https:\/\//.test(url)) throw new RenderError('Replicate returned no image', 'provider', p.id);
    req.onStatus?.('downloading');
    let res: Response;
    try { res = await this.fetch(url, { signal: req.signal, credentials: 'omit', referrerPolicy: 'no-referrer' }); }
    catch { throw new RenderError(`Could not download the rendered image from ${new URL(url).host} (the extension may need permission for that host)`, 'network', p.id); }
    if (!res.ok) throw new RenderError(`Could not download the rendered image (${res.status})`, 'network', p.id);
    const bytes = new Uint8Array(await res.arrayBuffer());
    const mime = sniffImage(bytes);
    if (!mime) throw new RenderError('The provider returned something that is not an image', 'provider', p.id);
    return { bytes, mime, model: model.id, sentPrompt: built.prompt, sentNegative: built.negative, sentInput: built.shown, jobId: p.id, diagnostics: built.diagnostics };
  }
}

/** Identify PNG, JPEG, WebP, GIF or AVIF by signature; null otherwise. */
export function sniffImage(b: Uint8Array): string | null {
  const at = (i: number, ...v: number[]): boolean => v.every((x, k) => b[i + k] === x);
  if (at(0, 0x89, 0x50, 0x4e, 0x47)) return 'image/png';
  if (at(0, 0xff, 0xd8, 0xff)) return 'image/jpeg';
  if (at(0, 0x52, 0x49, 0x46, 0x46) && at(8, 0x57, 0x45, 0x42, 0x50)) return 'image/webp';
  if (at(0, 0x47, 0x49, 0x46, 0x38)) return 'image/gif';
  if (at(4, 0x66, 0x74, 0x79, 0x70) && (at(8, 0x61, 0x76, 0x69, 0x66) || at(8, 0x61, 0x76, 0x69, 0x73))) return 'image/avif';
  return null;
}
