// Background-side checks for the encode port (defence in depth: the page runs
// the engine, but only the background holds the token). Every prediction input
// is rebuilt from an allow-list of fields with type and range checks, image
// fields accept only re-encoded data: images or Replicate output URLs, and each
// port has a fixed spending ceiling and a concurrency cap. Spec: Parameter
// forwarding; What leaves the browser.
import type { ModelRef } from '../providers/replicate-client.ts';
import { REPLICATE_MODELS } from '../renderers/replicate.ts';
import { tokenCostUSD, worstCaseUSD } from '../providers/pricing.ts';

export const GEMINI: ModelRef = { id: 'google/gemini-2.5-flash' };
export const CLAUDE: ModelRef = { id: 'anthropic/claude-4.5-sonnet' };
export const CLAUDE5: ModelRef = { id: 'anthropic/claude-sonnet-5' };
export const LUNA: ModelRef = { id: 'openai/gpt-5.6-luna' };
export const CLIP: ModelRef = { id: 'krthr/clip-embeddings', version: '1c0371070cb827ec3c7f2f28adcdde54b50dcd239aa6faea0bc98b174ef03fb4' };

/** Most one prediction can cost (held before it starts), the same estimates the engine budgets with. */
export function costOf(model: ModelRef): number {
  if (model.id === GEMINI.id || model.id === CLAUDE.id || model.id === CLAUDE5.id || model.id === LUNA.id) return worstCaseUSD(model.id);
  if (model.id === CLIP.id) return 0.0005;
  return REPLICATE_MODELS.find(m => m.id === model.id)?.costHint ?? Infinity;
}

export const MAX_ENCODE_BUDGET = 5;
export const MAX_IN_FLIGHT = 8;
const MAX_DATA_URL = 12 * 1024 * 1024;

export function allowedModel(m: { id?: unknown; version?: unknown }): ModelRef | null {
  const all: ModelRef[] = [...REPLICATE_MODELS.map(x => (x.official ? { id: x.id } : { id: x.id, version: x.version })), GEMINI, CLAUDE, CLAUDE5, LUNA, CLIP];
  return all.find(a => a.id === m.id && (a.version ?? null) === (m.version ?? null)) ?? null;
}

/** A provider-fetchable image: a re-encoded raster data: URL, or an https URL on Replicate's output hosts. */
export function allowedImage(v: unknown): v is string {
  if (typeof v !== 'string') return false;
  if (/^data:image\/(jpeg|png|webp);base64,[A-Za-z0-9+/]+=*$/.test(v)) return v.length <= MAX_DATA_URL;
  let u: URL;
  try { u = new URL(v); } catch { return false; }
  return u.protocol === 'https:' && !u.username && !u.password && !u.port
    && (u.hostname === 'replicate.delivery' || u.hostname.endsWith('.replicate.delivery') || u.hostname.endsWith('.r2.cloudflarestorage.com'));
}

const str = (v: unknown, max: number): string | undefined => (typeof v === 'string' && v.length <= max ? v : undefined);
const int = (v: unknown, lo: number, hi: number): number | undefined => (Number.isInteger(v) && (v as number) >= lo && (v as number) <= hi ? (v as number) : undefined);
const num = (v: unknown, lo: number, hi: number): number | undefined => (typeof v === 'number' && Number.isFinite(v) && v >= lo && v <= hi ? v : undefined);

export type Sanitized = { input: Record<string, unknown> } | { error: string };

/** Rebuild a prediction input for an allow-listed model; unknown fields are dropped, bad values refused. */
export function sanitizeInput(model: ModelRef, raw: unknown): Sanitized {
  if (!raw || typeof raw !== 'object' || Array.isArray(raw)) return { error: 'Invalid input.' };
  const r = raw as Record<string, unknown>;
  const out: Record<string, unknown> = {};
  const put = (k: string, v: unknown, required = false): boolean => {
    if (v === undefined) { if (r[k] !== undefined || required) return false; return true; }
    out[k] = v;
    return true;
  };
  const fail = (what: string): Sanitized => ({ error: `Invalid ${what} for ${model.id}.` });

  if (model.id === GEMINI.id) {
    if (!put('prompt', str(r.prompt, 40000), true)) return fail('prompt');
    if (!put('system_instruction', str(r.system_instruction, 20000))) return fail('system_instruction');
    if (!put('temperature', num(r.temperature, 0, 2))) return fail('temperature');
    if (!put('thinking_budget', int(r.thinking_budget, 0, 2048))) return fail('thinking_budget');
    out.max_output_tokens = int(r.max_output_tokens, 1, 8192) ?? 8192;
    if (r.images !== undefined) {
      if (!Array.isArray(r.images) || r.images.length > 10 || !r.images.every(allowedImage)) return fail('images');
      out.images = r.images;
    }
    return { input: out };
  }
  if (model.id === LUNA.id) {
    if (!put('prompt', str(r.prompt, 40000), true)) return fail('prompt');
    if (!put('system_prompt', str(r.system_prompt, 20000))) return fail('system_prompt');
    if (!put('reasoning_effort', ['none', 'low', 'medium'].includes(r.reasoning_effort as string) ? r.reasoning_effort : undefined)) return fail('reasoning_effort');
    out.max_completion_tokens = int(r.max_completion_tokens, 1, 4096) ?? 4096;
    if (r.image_input !== undefined) {
      if (!Array.isArray(r.image_input) || r.image_input.length > 10 || !r.image_input.every(allowedImage)) return fail('image_input');
      out.image_input = r.image_input;
    }
    return { input: out };
  }
  if (model.id === CLAUDE.id || model.id === CLAUDE5.id) {
    if (!put('prompt', str(r.prompt, 40000), true)) return fail('prompt');
    if (!put('system_prompt', str(r.system_prompt, 20000))) return fail('system_prompt');
    out.max_tokens = int(r.max_tokens, 1, 4096) ?? 4096;
    if (r.image !== undefined) { if (!allowedImage(r.image)) return fail('image'); out.image = r.image; }
    return { input: out };
  }
  if (model.id === CLIP.id) {
    if (!allowedImage(r.image)) return fail('image');
    return { input: { image: r.image } };
  }
  const m = REPLICATE_MODELS.find(x => x.id === model.id);
  if (!m) return { error: 'That model is not available to the encoder.' };
  // Generate mode only: the encoder never sends input images to image models.
  if (!put('prompt', str(r.prompt, 8000), true)) return fail('prompt');
  if (!put('negative_prompt', str(r.negative_prompt, 4000))) return fail('negative_prompt');
  if (m.aspects) { if (!put('aspect_ratio', typeof r.aspect_ratio === 'string' && m.aspects.includes(r.aspect_ratio) ? r.aspect_ratio : undefined)) return fail('aspect_ratio'); }
  if (m.sizeInputs) {
    if (!put('width', int(r.width, 256, 2048))) return fail('width');
    if (!put('height', int(r.height, 256, 2048))) return fail('height');
  }
  if (!put('seed', int(r.seed, 0, m.seedMax))) return fail('seed');
  // Fixed fields come from the renderer's own table, never from the page.
  Object.assign(out, m.extra ?? {});
  if (m.outputFormat) out[m.outputFormat.field] = m.outputFormat.value;
  return { input: out };
}

/** Per-port ledger: a ceiling fixed once at the start, and a cap on concurrent predictions. */
export class PortLedger {
  private ceiling: number | null = null;
  spent = 0;
  inFlight = 0;
  begin(budget: unknown): boolean {
    if (this.ceiling !== null) return false;
    if (typeof budget !== 'number' || !Number.isFinite(budget) || budget <= 0) return false;
    this.ceiling = Math.min(budget, MAX_ENCODE_BUDGET);
    return true;
  }
  /** Hold the worst case of one prediction; returns an error message or null. */
  admit(model: ModelRef): string | null {
    if (this.ceiling === null) return 'The encoder did not declare a spending limit.';
    if (this.inFlight >= MAX_IN_FLIGHT) return 'Too many requests at once.';
    const cost = costOf(model);
    if (this.spent + cost > this.ceiling + 1e-9) return 'The spending limit for this run is reached.';
    this.spent += cost;
    this.inFlight++;
    return null;
  }
  /** Settle a prediction: token-priced models are charged from reported usage; others keep the hold (the job may have run). */
  done(model: ModelRef, metrics?: Record<string, unknown> | null): void {
    this.inFlight = Math.max(0, this.inFlight - 1);
    const actual = metrics ? tokenCostUSD(model.id, metrics) : null;
    if (actual !== null) this.spent += Math.min(actual, costOf(model)) - costOf(model);
  }
}
