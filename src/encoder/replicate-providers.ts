// Analysts backed by models hosted on Replicate. All calls go through a
// ReplicateClient, so the extension can keep the token in its background while
// the engine runs in a page. One shared implementation does describe, inspect
// and propose; each model only defines how to call it.
import type { ReplicateClient } from '../providers/replicate-client.ts';
import { tokenCostUSD, worstCaseUSD } from '../providers/pricing.ts';
import { describeObservation } from './metrics.ts';
import { INSPECT_SYSTEM, SYSTEM, describePrompt, inspectRenderPrompt, inspectTargetPrompt, parseAnswer, proposePrompt, toAdditions, toDrafts, toObservation } from './prompts.ts';
import type { Analyst, AnalystContext, AnalystReply, Draft, ImageRef, Observation, ProposeContext } from './types.ts';

const textOf = (output: unknown): string => (Array.isArray(output) ? output.join('') : typeof output === 'string' ? output : '');

interface Call { system: string; prompt: string; images: ImageRef[]; careful: boolean; signal?: AbortSignal }

abstract class ReplicateAnalyst implements Analyst {
  abstract readonly id: string;
  readonly costHint: number;
  constructor(protected readonly client: ReplicateClient, model: string, costHint?: number) {
    this.costHint = costHint ?? worstCaseUSD(model);
  }
  /** Run one call; returns the answer text and the provider's metrics. */
  protected abstract run(c: Call): Promise<{ text: string; metrics?: Record<string, unknown> }>;

  private async ask(c: Call): Promise<{ answer: Record<string, unknown>; cost: number }> {
    const r = await this.run(c);
    const cost = Math.min(this.costHint, tokenCostUSD(this.id, r.metrics) ?? this.costHint);
    return { answer: parseAnswer(r.text), cost };
  }

  async describe(target: ImageRef, n: number, ctx: AnalystContext): Promise<AnalystReply<Draft[]>> {
    const { answer, cost } = await this.ask({ system: SYSTEM, prompt: describePrompt(n, ctx.models, ctx.maxWords), images: [target], careful: true, signal: ctx.signal });
    return { value: toDrafts(answer.drafts).slice(0, n), notes: String(answer.notes ?? ''), cost };
  }

  async inspect(image: ImageRef, subjects: string[] | null, ctx: AnalystContext & { careful?: boolean }): Promise<AnalystReply<Observation>> {
    const prompt = subjects ? inspectRenderPrompt(subjects) : inspectTargetPrompt();
    const { answer, cost } = await this.ask({ system: INSPECT_SYSTEM, prompt, images: [image], careful: !!ctx.careful, signal: ctx.signal });
    return { value: toObservation(answer, subjects), notes: '', cost };
  }

  async propose(target: ImageRef, ctx: ProposeContext): Promise<AnalystReply<Draft[] | string[]>> {
    const prompt = proposePrompt(ctx, describeObservation(ctx.targetObservation));
    const { answer, cost } = await this.ask({ system: SYSTEM, prompt, images: [target], careful: true, signal: ctx.signal });
    const value = ctx.scope.kind === 'refine' ? toAdditions(answer.additions).slice(0, ctx.n) : toDrafts(answer.drafts).slice(0, ctx.n);
    return { value, notes: String(answer.notes ?? ''), cost };
  }
}

/** OpenAI GPT-5.6 luna: cheap, several images per call, and precise boxes (measured against Earthrise). The default. */
export class OpenAIAnalyst extends ReplicateAnalyst {
  readonly id: string;
  constructor(client: ReplicateClient, private readonly opts: { model?: string; costHint?: number } = {}) {
    super(client, opts.model ?? 'openai/gpt-5.6-luna', opts.costHint);
    this.id = opts.model ?? 'openai/gpt-5.6-luna';
  }
  protected async run(c: Call): Promise<{ text: string; metrics?: Record<string, unknown> }> {
    const p = await this.client.predict({ id: this.id }, {
      prompt: c.prompt, system_prompt: c.system, image_input: c.images.slice(0, 10).map(i => i.url),
      reasoning_effort: c.careful ? 'low' : 'none', max_completion_tokens: 4096,
    }, { signal: c.signal });
    return { text: textOf(p.output), metrics: p.metrics };
  }
}

/** Google Gemini 2.5 Flash (kept as an option; its vertical box estimates were less precise). */
export class GeminiAnalyst extends ReplicateAnalyst {
  readonly id = 'google/gemini-2.5-flash';
  constructor(client: ReplicateClient, opts: { costHint?: number } = {}) { super(client, 'google/gemini-2.5-flash', opts.costHint); }
  protected async run(c: Call): Promise<{ text: string; metrics?: Record<string, unknown> }> {
    // A thinking budget of 0 cut answers short on Replicate; keep a small one.
    const p = await this.client.predict({ id: this.id }, {
      prompt: c.prompt, system_instruction: c.system, images: c.images.slice(0, 10).map(i => i.url),
      temperature: c.careful ? 0.7 : 0, thinking_budget: c.careful ? 1024 : 256, max_output_tokens: 8192,
    }, { signal: c.signal });
    return { text: textOf(p.output), metrics: p.metrics };
  }
}

/**
 * Anthropic Claude on Replicate (Sonnet 5 by default) takes one image per call.
 * It is meant as the optimiser in the "strong model" route, where it sees the
 * target and the measured history; inspection stays on a cheaper model.
 */
export class ClaudeAnalyst extends ReplicateAnalyst {
  readonly id: string;
  constructor(client: ReplicateClient, opts: { model?: string; costHint?: number } = {}) {
    super(client, opts.model ?? 'anthropic/claude-sonnet-5', opts.costHint);
    this.id = opts.model ?? 'anthropic/claude-sonnet-5';
  }
  protected async run(c: Call): Promise<{ text: string; metrics?: Record<string, unknown> }> {
    const p = await this.client.predict({ id: this.id }, { prompt: c.prompt, system_prompt: c.system, max_tokens: 4096, ...(c.images[0] ? { image: c.images[0].url } : {}) }, { signal: c.signal });
    return { text: textOf(p.output), metrics: p.metrics };
  }
}
