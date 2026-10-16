// Analysts and a scorer backed by models hosted on Replicate. All calls go
// through a ReplicateClient, so the extension can keep the token in its
// background while the engine runs in a page.
import type { ModelRef, ReplicateClient } from '../providers/replicate-client.ts';
import { cosine } from './metrics.ts';
import { SYSTEM, describePrompt, judgePrompt, parseAnswer, refinePrompt, revisePrompt, toAdditions, toDrafts, toScores } from './prompts.ts';
import type { Analyst, AnalystContext, AnalystReply, Draft, ImageRef, RenderView, Scorer } from './types.ts';

const textOf = (output: unknown): string => (Array.isArray(output) ? output.join('') : typeof output === 'string' ? output : '');

/** Google Gemini 2.5 Flash: sees the target and several renders in one call. */
export class GeminiAnalyst implements Analyst {
  readonly id = 'google/gemini-2.5-flash';
  readonly costHint: number;
  constructor(private readonly client: ReplicateClient, private readonly opts: { costHint?: number; temperature?: number; thinkingBudget?: number } = {}) {
    this.costHint = opts.costHint ?? 0.01;
  }
  private async ask(prompt: string, images: ImageRef[], signal?: AbortSignal): Promise<Record<string, unknown>> {
    const p = await this.client.predict({ id: this.id }, {
      prompt, images: images.slice(0, 10).map(i => i.url), system_instruction: SYSTEM,
      temperature: this.opts.temperature ?? 0.8, thinking_budget: this.opts.thinkingBudget ?? 1024, max_output_tokens: 8192,
    }, { signal });
    return parseAnswer(textOf(p.output));
  }
  async describe(target: ImageRef, n: number, ctx: AnalystContext): Promise<AnalystReply<Draft[]>> {
    const a = await this.ask(describePrompt(n, ctx.models), [target], ctx.signal);
    return { value: toDrafts(a.drafts).slice(0, n), notes: String(a.notes ?? ''), cost: this.costHint };
  }
  async revise(target: ImageRef, current: Draft, renders: RenderView[], n: number, ctx: AnalystContext): Promise<AnalystReply<Draft[]>> {
    const shown = renders.slice(0, 9);
    const a = await this.ask(revisePrompt(current, shown, n), [target, ...shown.map(r => r.image)], ctx.signal);
    return { value: toDrafts(a.drafts).slice(0, n), notes: String(a.notes ?? ''), cost: this.costHint };
  }
  async refine(target: ImageRef, core: Draft, render: RenderView, n: number, ctx: AnalystContext): Promise<AnalystReply<string[]>> {
    const a = await this.ask(refinePrompt(core, render, n), [target, render.image], ctx.signal);
    return { value: toAdditions(a.additions).slice(0, n), notes: String(a.notes ?? ''), cost: this.costHint };
  }
  async judge(target: ImageRef, renders: RenderView[], ctx: AnalystContext): Promise<AnalystReply<number[]>> {
    if (renders.length > 9) throw new RangeError('judge at most 9 renders per call');
    const a = await this.ask(judgePrompt(renders), [target, ...renders.map(r => r.image)], ctx.signal);
    return { value: toScores(a.scores, renders.length), notes: String(a.notes ?? ''), cost: this.costHint };
  }
}

/**
 * Anthropic Claude Sonnet 4.5 on Replicate takes one image per call. Comparing
 * needs the target and a render side by side; the app supplies `composite`
 * (sharp in the CLI, canvas in the extension). Without it, revise and refine
 * see the target only.
 */
export class ClaudeAnalyst implements Analyst {
  readonly id = 'anthropic/claude-4.5-sonnet';
  readonly costHint: number;
  readonly judgeLimit = 5;
  /** Judging needs the images side by side, so it exists only with `composite`. */
  readonly judge?: (target: ImageRef, renders: RenderView[], ctx: AnalystContext) => Promise<AnalystReply<number[]>>;
  constructor(private readonly client: ReplicateClient, private readonly opts: { costHint?: number; composite?: (images: ImageRef[]) => Promise<ImageRef> } = {}) {
    this.costHint = opts.costHint ?? 0.04;
    if (opts.composite) this.judge = async (target, renders, ctx) => {
      const shown = renders.slice(0, 5);
      if (shown.length !== renders.length) throw new RangeError('judge at most 5 renders per call with a composite');
      const a = await this.ask(judgePrompt(shown), [target, ...shown.map(r => r.image)], ctx.signal);
      return { value: toScores(a.scores, shown.length), notes: String(a.notes ?? ''), cost: this.costHint };
    };
  }
  private async ask(prompt: string, images: ImageRef[], signal?: AbortSignal): Promise<Record<string, unknown>> {
    let image = images[0];
    let note = '';
    if (images.length > 1 && this.opts.composite) {
      image = await this.opts.composite(images);
      note = '\n(The images are shown side by side in one picture, left to right: image 1, image 2, ...)';
    } else if (images.length > 1) note = '\n(Only image 1, the target, is attached; judge the renders from their similarity values.)';
    const p = await this.client.predict({ id: this.id }, { prompt: prompt + note, system_prompt: SYSTEM, max_tokens: 4096, ...(image ? { image: image.url } : {}) }, { signal });
    return parseAnswer(textOf(p.output));
  }
  async describe(target: ImageRef, n: number, ctx: AnalystContext): Promise<AnalystReply<Draft[]>> {
    const a = await this.ask(describePrompt(n, ctx.models), [target], ctx.signal);
    return { value: toDrafts(a.drafts).slice(0, n), notes: String(a.notes ?? ''), cost: this.costHint };
  }
  async revise(target: ImageRef, current: Draft, renders: RenderView[], n: number, ctx: AnalystContext): Promise<AnalystReply<Draft[]>> {
    const shown = renders.slice(0, 3);
    const a = await this.ask(revisePrompt(current, shown, n), [target, ...shown.map(r => r.image)], ctx.signal);
    return { value: toDrafts(a.drafts).slice(0, n), notes: String(a.notes ?? ''), cost: this.costHint };
  }
  async refine(target: ImageRef, core: Draft, render: RenderView, n: number, ctx: AnalystContext): Promise<AnalystReply<string[]>> {
    const a = await this.ask(refinePrompt(core, render, n), [target, render.image], ctx.signal);
    return { value: toAdditions(a.additions).slice(0, n), notes: String(a.notes ?? ''), cost: this.costHint };
  }
}

/** CLIP ViT-L/14 image embeddings (krthr/clip-embeddings, a community model pinned by version). */
export const CLIP_MODEL: ModelRef = { id: 'krthr/clip-embeddings', version: '1c0371070cb827ec3c7f2f28adcdde54b50dcd239aa6faea0bc98b174ef03fb4' };

export class ClipScorer implements Scorer {
  readonly id = CLIP_MODEL.id;
  readonly costHint: number;
  private target: number[] | null = null;
  constructor(private readonly client: ReplicateClient, opts: { costHint?: number } = {}) { this.costHint = opts.costHint ?? 0.0005; }
  private async embed(image: ImageRef, signal?: AbortSignal): Promise<number[]> {
    const p = await this.client.predict(CLIP_MODEL, { image: image.url }, { signal });
    const e = (p.output as { embedding?: unknown } | null)?.embedding;
    if (!Array.isArray(e) || !e.length || !e.every(x => typeof x === 'number')) throw new Error('The scorer returned no embedding');
    return e as number[];
  }
  async prepare(target: ImageRef, signal?: AbortSignal): Promise<void> { this.target = await this.embed(target, signal); }
  async score(render: ImageRef, signal?: AbortSignal): Promise<number> {
    if (!this.target) throw new Error('prepare() the scorer first');
    return cosine(this.target, await this.embed(render, signal));
  }
}
