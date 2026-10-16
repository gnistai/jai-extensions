// Prompt assembly, following the spec's "Assembling the model input":
// the description is always sent explicitly; style and avoid always reach the
// model or are reported; text is content, never instructions.
import type { ModelInfo, RenderDiagnostic, RenderIntent } from './types.ts';

export interface AssembledPrompt {
  prompt: string;
  negative: string | null;
  diagnostics: RenderDiagnostic[];
}

export interface PromptOptions {
  /** When the model has no negative prompt: fold avoid into the prompt (true) or omit and report (false). */
  foldAvoid: boolean;
  /** Maximum prompt length in characters the provider accepts; longer text is shortened from the end of the description. */
  maxChars?: number;
}

export function assemblePrompt(intent: RenderIntent, model: ModelInfo, opts: PromptOptions): AssembledPrompt {
  const diagnostics: RenderDiagnostic[] = [];
  let description = intent.description;
  const style = intent.style.join(', ');
  let negative: string | null = null;
  let avoidLine = '';
  if (intent.avoid.length) {
    if (model.negativePrompt) negative = intent.avoid.join(', ');
    else if (opts.foldAvoid) {
      avoidLine = `The image does not contain: ${intent.avoid.join(', ')}.`;
      diagnostics.push({ code: 'folded-into-prompt', key: 'avoid' });
    } else diagnostics.push({ code: 'unsupported-parameter', key: 'avoid' });
  }
  const build = (desc: string): string => {
    const parts: string[] = [];
    if (model.shortWindow) {
      if (style) parts.push(`${style}.`);
      if (desc) parts.push(desc);
      if (avoidLine) parts.push(avoidLine);
      return parts.join(' ');
    }
    if (desc) parts.push(desc);
    if (style) parts.push(`Style: ${style}.`);
    if (avoidLine) parts.push(avoidLine);
    return parts.join('\n\n');
  };
  let prompt = build(description);
  if (opts.maxChars && prompt.length > opts.maxChars) {
    // Drop description text from the end before any style term (spec SHOULD).
    const overhead = build('').length + 2;
    const keep = Math.max(0, opts.maxChars - overhead);
    description = Array.from(description).slice(0, keep).join('').trimEnd();
    prompt = build(description);
    diagnostics.push({ code: 'adjusted-value', key: 'prompt', requested: String(intent.description.length), actual: String(description.length) });
  }
  return { prompt, negative, diagnostics };
}

/** Nearest supported ratio, by log distance. */
export function nearestAspect(ratio: number, list: string[]): string {
  let best = list[0] ?? '1:1';
  let bd = Infinity;
  for (const a of list) {
    const [w, h] = a.split(':').map(Number);
    if (!w || !h) continue;
    const d = Math.abs(Math.log(w / h / ratio));
    if (d < bd) { bd = d; best = a; }
  }
  return best;
}

export function ratioOf(a: string): number {
  const [w, h] = a.split(':').map(Number);
  return w && h ? w / h : 1;
}
