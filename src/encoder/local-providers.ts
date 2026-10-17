// Local analyst: Qwen3.8-27B through JAI Studio's chat endpoint, with the
// encoder's prompts and JSON-schema answers. Every call sends one image as a
// data: URL and a response_format whose schema fixes the item counts, which
// llama.cpp turns into a grammar. See studio/DESIGN.md section 9.
import type { ChatResult, OpenAIChatClient } from '../providers/openai-chat.ts';
import { describeObservation } from './metrics.ts';
import { SYSTEM, cleanText, describePrompt, inspectRenderPrompt, inspectSystem, inspectTargetPrompt, parseAnswer, proposePrompt, toAdditions, toDrafts, toObservation } from './prompts.ts';
import type { Analyst, AnalystContext, AnalystReply, Draft, ImageRef, Observation, ProposeContext } from './types.ts';

type Thinking = 'off' | 'propose' | 'all';
type Schema = Record<string, unknown>;
/** A response_format json_schema: its name and schema. */
interface Format { name: string; schema: Schema }

// ------------------------------------------------------------ answer schemas

const text = (maxLength: number, minLength = 0): Schema => (minLength ? { type: 'string', minLength, maxLength } : { type: 'string', maxLength });
const exactly = (items: Schema, n: number): Schema => ({ type: 'array', items, minItems: n, maxItems: n });
const object = (properties: Record<string, Schema>): Schema => ({ type: 'object', properties, required: Object.keys(properties), additionalProperties: false });
const orNull = (s: Schema): Schema => ({ anyOf: [s, { type: 'null' }] });

// Lengths of the written answers, in characters; writingTokens derives max_tokens from them.
const PROMPT_CHARS = 1200, TERM_CHARS = 80, STYLE_TERMS = 3, AVOID_TERMS = 2, NOTES_CHARS = 600, ADDITION_CHARS = 300;
const DRAFT_CHARS = PROMPT_CHARS + (STYLE_TERMS + AVOID_TERMS) * TERM_CHARS;

const DRAFT = object({
  prompt: text(PROMPT_CHARS, 1),
  style: { type: 'array', items: text(TERM_CHARS), maxItems: STYLE_TERMS },
  avoid: { type: 'array', items: text(TERM_CHARS), maxItems: AVOID_TERMS },
});
const COORD: Schema = { type: 'integer', minimum: 0, maximum: 1000 };
const BOX = exactly(COORD, 4);
const SCENE: Record<string, Schema> = {
  horizon_y: orNull(COORD),
  horizon_shape: { enum: ['flat', 'curved', null] },
  camera: text(120),
  lighting: text(120),
  medium: text(120),
};

/** describe and core propose: exactly n drafts. */
const draftsFormat = (n: number): Format => ({ name: 'jai_drafts', schema: object({ notes: text(NOTES_CHARS), drafts: exactly(DRAFT, n) }) });
/** refine propose: exactly n additions. */
const additionsFormat = (n: number): Format => ({ name: 'jai_additions', schema: object({ notes: text(NOTES_CHARS), additions: exactly(text(ADDITION_CHARS, 1), n) }) });
/** Target inspection: 1 to 5 subjects with boxes. */
const TARGET_FORMAT: Format = {
  name: 'jai_target_inspection',
  schema: object({ subjects: { type: 'array', items: object({ name: text(60, 1), bbox_2d: BOX }), minItems: 1, maxItems: 5 }, ...SCENE }),
};
/**
 * Render inspection: one entry per given subject, each named from the list, each
 * box or null. The model picks the order (toObservation matches by name): a
 * tuple that fixes the order made Qwen write its boxes in its own order under
 * the wrong names.
 */
function renderFormat(subjects: string[]): Format {
  const n = subjects.length;
  const name = n ? { type: 'string', enum: subjects } : text(60);
  return { name: 'jai_render_inspection', schema: object({ subjects: exactly(object({ name, bbox_2d: orNull(BOX) }), n), ...SCENE }) };
}

// ------------------------------------------------------------ sampling

interface Sampling { temperature: number; max_tokens: number; top_p?: number; top_k?: number; presence_penalty?: number }
const INSPECTING: Sampling = { temperature: 0, max_tokens: 800 };
const WRITING = { temperature: 0.7, top_p: 0.8, top_k: 20, presence_penalty: 0.5 } as const;

/** JAI Studio clamps max_tokens to this (DESIGN.md section 7). */
const SERVER_MAX_TOKENS = 8192;
/** Tokens a JSON field of up to `chars` characters can take: English text stays above 3 characters a token; 60 more for the keys, quotes and spacing. */
const fieldTokens = (chars: number): number => Math.ceil(chars / 3) + 60;
/**
 * max_tokens for describe and propose: room for the longest answer the schema
 * allows (n items of `itemChars`, plus the notes), so no valid answer is cut off.
 * With thinking on, the server's limit: the reasoning, which counts against
 * max_tokens, has no length known in advance.
 */
const writingTokens = (n: number, itemChars: number, think: boolean): number =>
  (think ? SERVER_MAX_TOKENS : Math.min(SERVER_MAX_TOKENS, n * fieldTokens(itemChars) + fieldTokens(NOTES_CHARS)));
const writing = (n: number, itemChars: number, think: boolean): Sampling => ({ ...WRITING, max_tokens: writingTokens(n, itemChars, think) });

/** A reasoning block some servers leave in the content; its braces would confuse parseAnswer. */
const THINK_BLOCK = /^\s*<think>[\s\S]*?<\/think>/;

const countOf = (n: number): number => (Number.isFinite(n) && n >= 1 ? Math.floor(n) : 1);
const notesOf = (a: Record<string, unknown>): string => (typeof a.notes === 'string' ? cleanText(a.notes, NOTES_CHARS) : '');

export class LocalAnalyst implements Analyst {
  readonly id = 'local/qwen3.8-27b';
  readonly costHint = 0;
  private readonly thinking: Thinking;

  constructor(private readonly client: OpenAIChatClient, opts: { thinking?: Thinking } = {}) {
    this.thinking = opts.thinking === 'propose' || opts.thinking === 'all' ? opts.thinking : 'off';
  }

  /**
   * One chat call: system prompt, the task, then one image, answered as JSON in
   * `format`. The text goes first: with the image first, Qwen3.8 garbled its box
   * coordinates (measured on FLUX renders of Earthrise).
   */
  private async ask(system: string, prompt: string, image: ImageRef, format: Format, think: boolean, sampling: Sampling, signal?: AbortSignal): Promise<Record<string, unknown>> {
    const r: ChatResult = await this.client.complete({
      messages: [
        { role: 'system', content: system },
        { role: 'user', content: [{ type: 'text', text: prompt }, { type: 'image_url', image_url: { url: image.url } }] },
      ],
      ...sampling,
      response_format: { type: 'json_schema', json_schema: { name: format.name, strict: true, schema: format.schema } },
      chat_template_kwargs: { enable_thinking: think },
    }, { signal });
    try {
      return parseAnswer(r.text.replace(THINK_BLOCK, ''));
    } catch (e) {
      if (r.finishReason === 'length') throw new Error(`The analyst's answer was cut off at ${sampling.max_tokens} tokens`);
      throw e;
    }
  }

  async describe(target: ImageRef, n: number, ctx: AnalystContext): Promise<AnalystReply<Draft[]>> {
    const count = countOf(n);
    const think = this.thinking === 'all';
    const a = await this.ask(SYSTEM, describePrompt(count, ctx.models, ctx.maxWords), target, draftsFormat(count), think, writing(count, DRAFT_CHARS, think), ctx.signal);
    return { value: toDrafts(a.drafts).slice(0, count), notes: notesOf(a), cost: 0 };
  }

  async inspect(image: ImageRef, subjects: string[] | null, ctx: AnalystContext & { careful?: boolean }): Promise<AnalystReply<Observation>> {
    // Boxes in Qwen's own [x1, y1, x2, y2] order (see BoxOrder in prompts.ts).
    const a = subjects
      ? await this.ask(inspectSystem('xy'), inspectRenderPrompt(subjects, 'xy'), image, renderFormat(subjects), false, INSPECTING, ctx.signal)
      : await this.ask(inspectSystem('xy'), inspectTargetPrompt('xy'), image, TARGET_FORMAT, false, INSPECTING, ctx.signal);
    return { value: toObservation(a, subjects, 'xy'), notes: '', cost: 0 };
  }

  async propose(target: ImageRef, ctx: ProposeContext): Promise<AnalystReply<Draft[] | string[]>> {
    const count = countOf(ctx.n);
    const refine = ctx.scope.kind === 'refine';
    const prompt = proposePrompt({ ...ctx, n: count }, describeObservation(ctx.targetObservation));
    const think = this.thinking !== 'off';
    const a = await this.ask(SYSTEM, prompt, target, refine ? additionsFormat(count) : draftsFormat(count), think, writing(count, refine ? ADDITION_CHARS : DRAFT_CHARS, think), ctx.signal);
    const value = refine ? toAdditions(a.additions).slice(0, count) : toDrafts(a.drafts).slice(0, count);
    return { value, notes: notesOf(a), cost: 0 };
  }
}
