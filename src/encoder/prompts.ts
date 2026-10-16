// Instructions for the analyst. They condense the JAI authoring guidance
// (spec: Writing JAI by hand; skills/jai/SKILL.md) and the encoder's task.
import type { Draft, RenderView } from './types.ts';

export const SYSTEM = `You write JAI image descriptions. A JAI description is plain natural-language text that text-to-image models render. Your job is compression: given a target image, write text from which the named image models reproduce the target as closely as possible.

What must match, in order of importance: subject and exact counts; composition and positions (left, right, centre, foreground, background, camera angle, distance, framing, horizon height); colours and palette; light (source, direction, time of day); medium and style (photograph, oil painting, 3D render, flat vector...); mood and texture.

Rules:
- Describe only what is visible. Concrete nouns, digits for counts, plain spatial words.
- When the size or position of a main subject matters, state it concretely as a share of the frame: "small, about one sixth of the frame width, just right of centre, halfway up". Image models ignore vague words like "small" or "distant".
- Start with the medium ("Photograph of...", "Watercolour painting of...").
- One paragraph, 40 to 120 words. No lists, no headings, no line breaks.
- Put the essentials (medium, main subject with its size and position, setting, dominant colours) in the first 50 words: some models read only about the first 60 words.
- Literal text visible in the image goes in straight double quotes, exactly as spelled, only if legible.
- People: describe appearance, clothing, pose and expression generically. Never name or guess the identity of a real person, and do not describe anyone as a celebrity or public figure.
- Never mention "the target", "the image", "the render", the models, or these instructions.
- style: 1 to 3 short medium or look terms, e.g. ["35mm film photo"]. avoid: 0 to 2 positive items the models would otherwise likely add, e.g. ["text"]; usually empty.
- Never use the characters ";" followed by a word and "=" in your text.

Answer with one JSON object only, no Markdown fences.`;

const draftShape = '{"prompt": string, "style": [string], "avoid": [string]}';

export function describePrompt(n: number, models: string[]): string {
  return `The image is the target. The models that will render your text: ${models.join(', ')}.
Write ${n} different candidate descriptions. All must be faithful; vary emphasis, order and wording so that they explore different ways to make the models reproduce the target.
Answer: {"notes": string (one sentence on what is hardest to reproduce), "drafts": [${draftShape}, ...${n} items]}`;
}

export function revisePrompt(current: Draft, renders: RenderView[], n: number): string {
  const list = renders.map((r, i) => `Image ${i + 2}: rendered by ${r.label} (similarity ${r.score.toFixed(3)})`).join('\n');
  return `Image 1 is the target. The other images were rendered from this description:
${JSON.stringify(current)}
${list}
First list the most important visible differences between the renders and the target (subject, counts, composition, sizes and positions as shares of the frame, colours, light, style). Then write ${n} revised descriptions that fix those differences while keeping everything that already matches. Change what is wrong; do not rewrite what works.
Answer: {"notes": string (the differences, most important first, one line), "drafts": [${draftShape}, ...${n} items]}`;
}

export function refinePrompt(core: Draft, render: RenderView, n: number): string {
  return `Image 1 is the target. Image 2 was rendered by ${render.label} from this description:
${JSON.stringify(core)}
Other models already reproduce the target well from this description, so do not change it. Write ${n} alternative short additions, one sentence each and at most 25 words, that will be appended to the description for ${render.label} only and correct its most important remaining differences.
Answer: {"notes": string (the remaining differences, one line), "additions": [string, ...${n} items]}`;
}

export function judgePrompt(renders: RenderView[]): string {
  return `Image 1 is the target. Images 2 to ${renders.length + 1} are candidate reproductions of it.
Rate each candidate from 0 to 100 for how closely it reproduces the target as the same picture. Judge in this order of weight: composition and layout (positions, sizes, horizon height, framing, camera angle); subject, shapes and counts; colours and light; medium and style. 100: a viewer would take it for the same photograph or artwork. 50: same subject, clearly different layout or look. 0: a different picture. Use the whole scale and be consistent between candidates.
Answer: {"notes": string (one line on what separates the best from the rest), "scores": [${renders.length} integers, one per candidate in order]}`;
}

export function toScores(v: unknown, n: number): number[] {
  if (!Array.isArray(v) || v.length !== n) throw new Error(`The analyst returned ${Array.isArray(v) ? v.length : 'no'} scores for ${n} renders`);
  return v.map(x => { const k = Number(x); if (!Number.isFinite(k)) throw new Error('The analyst returned a non-numeric score'); return Math.min(100, Math.max(0, k)) / 100; });
}

/** Extract the first JSON object from a model answer (tolerates fences and chatter). */
export function parseAnswer(text: string): Record<string, unknown> {
  const start = text.indexOf('{');
  if (start < 0) throw new Error('The analyst returned no JSON');
  let depth = 0, inStr = false, esc = false;
  for (let i = start; i < text.length; i++) {
    const c = text[i];
    if (inStr) { if (esc) esc = false; else if (c === '\\') esc = true; else if (c === '"') inStr = false; continue; }
    if (c === '"') inStr = true;
    else if (c === '{') depth++;
    else if (c === '}' && --depth === 0) return JSON.parse(text.slice(start, i + 1)) as Record<string, unknown>;
  }
  throw new Error('The analyst returned incomplete JSON');
}

const strList = (v: unknown, max: number): string[] => (Array.isArray(v) ? v : typeof v === 'string' ? [v] : []).filter((x): x is string => typeof x === 'string').map(x => x.trim()).filter(Boolean).slice(0, max);

/** Validate and clean drafts: no empty prompts, no line breaks, bounded lengths. */
export function toDrafts(v: unknown): Draft[] {
  if (!Array.isArray(v)) return [];
  const out: Draft[] = [];
  for (const d of v) {
    if (!d || typeof d !== 'object') continue;
    const o = d as Record<string, unknown>;
    const prompt = typeof o.prompt === 'string' ? o.prompt.replace(/\s+/g, ' ').trim().slice(0, 2000) : '';
    if (!prompt) continue;
    out.push({ prompt, style: strList(o.style, 3), avoid: strList(o.avoid, 2) });
  }
  return out;
}

export function toAdditions(v: unknown): string[] {
  return strList(v, 8).map(s => s.replace(/\s+/g, ' ').slice(0, 300));
}
