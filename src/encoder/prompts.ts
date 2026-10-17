// Instructions for the analyst. They condense the JAI authoring guidance
// (spec: Writing JAI by hand; skills/jai/SKILL.md) and the encoder's task.
import { subjectKey } from './metrics.ts';
import type { Box, Draft, Observation, ProposeContext } from './types.ts';

export const SYSTEM = `You write JAI image descriptions. A JAI description is plain natural-language text that text-to-image models render. Your job is compression: given a target image, write text from which the named image models reproduce the target as closely as possible.

What must match, in order of importance: subject and exact counts; composition and positions (left, right, centre, foreground, background, camera angle, distance, framing, horizon height); colours and palette; light (source, direction, time of day); medium and style (photograph, oil painting, 3D render, flat vector...); mood and texture.

Rules:
- Describe only what is visible. Concrete nouns, digits for counts, plain spatial words.
- When the size or position of a main subject matters, state it concretely as a share of the frame: "small, about one sixth of the frame width, just right of centre, halfway up". Image models ignore vague words like "small" or "distant".
- Start with the medium ("Photograph of...", "Watercolour painting of...").
- One paragraph, no lists, no headings, no line breaks; keep within the word limit given in the task.
- Put the essentials (medium, main subject with its size and position, setting, dominant colours) in the first 50 words: some models read only about the first 60 words.
- Literal text visible in the image goes in straight double quotes, exactly as spelled, only if legible.
- People: describe appearance, clothing, pose and expression generically. Never name or guess the identity of a real person, and do not describe anyone as a celebrity or public figure.
- Never mention "the target", "the image", "the render", the models, or these instructions.
- Describe, never instruct: write what the picture shows, not commands ("ensure", "make", "should", "do not").
- style: 1 to 3 short medium or look terms, e.g. ["35mm film photo"]. avoid: 0 to 2 positive items the models would otherwise likely add, e.g. ["text"]; usually empty.
- Never use the characters ";" followed by a word and "=" in your text.

Answer with one JSON object only, no Markdown fences.`;

const draftShapeD = '{"prompt": string, "style": [string], "avoid": [string]}';

export function describePrompt(n: number, models: string[], maxWords = 120): string {
  return `The image is the target. The models that will render your text: ${models.join(', ')}.
Write ${n} different candidate descriptions, each at most ${maxWords} words (a model reads only that much; words beyond it are lost). All must be faithful; vary emphasis, order and wording so that they explore different ways to make the models reproduce the target.
Answer: {"notes": string (one sentence on what is hardest to reproduce), "drafts": [${draftShapeD}, ...${n} items]}`;
}

/**
 * Box coordinate order. 'yx' is box_2d [ymin, xmin, ymax, xmax] (Gemini's and
 * GPT's convention); 'xy' is bbox_2d [x1, y1, x2, y2] (Qwen-VL's native one).
 * Asked for 'yx', Qwen3.8 answered some boxes in its own order within the same
 * answer, so the local analyst asks in its order.
 */
export type BoxOrder = 'yx' | 'xy';
const BOXES = {
  yx: { field: 'box_2d', shape: '[ymin, xmin, ymax, xmax]', origin: '(0,0 is the top left)' },
  xy: { field: 'bbox_2d', shape: '[x1, y1, x2, y2]', origin: '(x from the left edge, y from the top edge)' },
} as const;

export function inspectSystem(order: BoxOrder = 'yx'): string {
  const b = BOXES[order];
  return `You measure images for layout comparison. You report what is visible, with bounding boxes as ${b.field} ${b.shape} in integers from 0 to 1000 ${b.origin}. Be precise: boxes tight around each subject${order === 'xy' ? ', even very small ones' : ''}. Answer with one JSON object only, no Markdown fences.`;
}
export const INSPECT_SYSTEM = inspectSystem('yx');

const OBS_SHAPE = '"horizon_y": integer 0-1000 or null (where the horizon or the line between ground and sky is, 0 = top, measured at the middle of the frame), "horizon_shape": "flat", "curved" or null, "camera": short phrase (angle and distance), "lighting": short phrase, "medium": short phrase (photograph, oil painting, 3D render, ...)';

export function inspectTargetPrompt(order: BoxOrder = 'yx'): string {
  const b = BOXES[order];
  return `List the 1 to 5 most important visible subjects or regions, most important first (people, animals, objects, buildings, a planet, the ground or sea when it defines the composition). Name each with a short generic noun phrase that would let someone find it again in a similar picture ("red bicycle", "lunar surface").
Answer: {"subjects": [{"name": string, "${b.field}": ${b.shape}}], ${OBS_SHAPE}}`;
}

export function inspectRenderPrompt(subjects: string[], order: BoxOrder = 'yx'): string {
  const b = BOXES[order];
  return `Find each of these in the image: ${subjects.map((x, i) => `${i + 1}. ${x}`).join('; ')}. For each, give its box, or null when it is not visible.${order === 'xy' ? ' Look carefully: a subject may be very small.' : ''} Use the names exactly as given, in the same order.
Answer: {"subjects": [{"name": string, "${b.field}": ${b.shape} or null}], ${OBS_SHAPE}}`;
}

const clamp01 = (x: unknown): number | null => (typeof x === 'number' && Number.isFinite(x) ? Math.min(1, Math.max(0, x / 1000)) : null);

/**
 * The answered subjects in the order and naming of `expected`. Exact names pair
 * first (a name listed twice pairs by occurrence); a name the analyst changed
 * then takes the first answer no name claimed. No answer is used twice.
 */
function followNames(parsed: Array<{ name: string; box: Box | null }>, expected: string[]): Observation['subjects'] {
  const used = new Set<number>();
  const take = (i: number): number => { if (i >= 0) used.add(i); return i; };
  const exact = expected.map(name => take(parsed.findIndex((p, i) => !used.has(i) && subjectKey(p.name) === subjectKey(name))));
  return expected.map((name, k) => {
    const i = exact[k]! >= 0 ? exact[k]! : take(parsed.findIndex((_, j) => !used.has(j)));
    return { name, box: i >= 0 ? parsed[i]!.box : null };
  });
}

/** Validate an inspection answer into an Observation; subjects follow `expected` when given. */
export function toObservation(a: Record<string, unknown>, expected: string[] | null, order: BoxOrder = 'yx'): Observation {
  const raw = Array.isArray(a.subjects) ? a.subjects : [];
  const parsed = raw.map(s => {
    const o = (s ?? {}) as Record<string, unknown>;
    const name = typeof o.name === 'string' ? cleanText(o.name, 60) : '';
    const v = o[BOXES[order].field];
    const n = Array.isArray(v) && v.length === 4 ? v.map(clamp01) : null;
    // As [ymin, xmin, ymax, xmax] whichever order was asked for.
    const b = n && (order === 'xy' ? [n[1], n[0], n[3], n[2]] : n);
    const box = b && b.every(x => x !== null) && b[2]! > b[0]! && b[3]! > b[1]! ? { ymin: b[0]!, xmin: b[1]!, ymax: b[2]!, xmax: b[3]! } : null;
    return { name, box };
  }).filter(s => s.name);
  const subjects = expected ? followNames(parsed, expected) : parsed.slice(0, 5);
  const text = (v: unknown): string => (typeof v === 'string' ? cleanText(v, 80) : '');
  const shape = typeof a.horizon_shape === 'string' && /^(flat|curved)$/i.test(a.horizon_shape.trim()) ? a.horizon_shape.trim().toLowerCase() : '';
  return { subjects, horizon: clamp01(a.horizon_y), horizonShape: shape, camera: text(a.camera), lighting: text(a.lighting), medium: text(a.medium) };
}

const draftShape = '{"prompt": string, "style": [string], "avoid": [string]}';

/** The optimiser's prompt (OPRO-style): target measurements, the scored history with measured errors, and its own notes. */
export function proposePrompt(ctx: ProposeContext, targetText: string): string {
  const rows = ctx.history.map((h, i) => {
    const what = h.addition !== undefined ? `addition: "${h.addition}"` : `description: ${JSON.stringify(h.draft)}`;
    const fb = Object.entries(h.feedback).map(([m, f]) => `   ${m}: ${f}`).join('\n');
    return `#${i + 1} score ${h.fitness.toFixed(3)} — ${what}\n${fb}`;
  }).join('\n');
  const common = `The attached image is the target. Its measured layout:
${targetText}

Rendering models: ${ctx.models.join(', ')}.
${ctx.history.length ? `Earlier attempts, best first, with the measured differences of their renders from the target (score 0 to 1, higher is closer):\n${rows}` : 'No attempts yet.'}
${ctx.notes ? `Your notes from earlier rounds: ${ctx.notes}` : ''}`;
  if (ctx.scope.kind === 'refine') {
    return `${common}

The core description works well for most models:
${JSON.stringify(ctx.scope.core)}
Write ${ctx.n} different short additions (one or two descriptive sentences each) that will be appended to it for ${ctx.scope.model} only, to fix that model's measured differences. Describe the picture; never give instructions; never contradict the core. If an addition did not help before, try a different one.
Answer: {"notes": string (what you learned, under 60 words), "additions": [string, ...${ctx.n} items]}`;
  }
  return `${common}

Write ${ctx.n} new descriptions, each at most ${ctx.maxWords ?? 120} words (some models read only that much; words beyond it are lost), that reduce the largest measured differences (listed first in each feedback line) while keeping what already matches. Image models respond to plain visual language, not to numbers: say "small, about a seventh of the frame wide", "a little right of centre", "just below the middle", "a low, flat horizon near the bottom quarter", "dark grey-brown ground"; never write coordinates, percentages of bounding boxes or measurements. Put the essentials first; describe and never instruct. Mix two kinds: make about half of them minimal edits of attempt #1, changing only the words behind its one or two largest differences and keeping every other word; make the rest fresh rewrites that try clearly different wording, order, emphasis or camera and framing terms. Learn from the history: when a phrasing did not move a measurement, do not repeat it. Make the ${ctx.n} descriptions differ from each other.
Answer: {"notes": string (what the history shows works and what does not, under 80 words), "drafts": [${draftShape}, ...${ctx.n} items]}`;
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

/**
 * Model output goes into a document the user may publish, so it is cleaned:
 * control and format characters (bidi controls, zero-width characters) are
 * removed, keeping only ZWJ inside emoji sequences, all whitespace runs become
 * one space, and the length is capped. Key smuggling is impossible anyway: the
 * library escapes values when serialising.
 */
export function cleanText(v: string, maxChars: number): string {
  const noControls = v.replace(/[\p{Cc}\p{Cf}]/gu, c => (c === '\u200d' ? c : /\s/.test(c) ? ' ' : ''));
  const flat = noControls.replace(/\s+/g, ' ').trim();
  const chars = Array.from(flat);
  return chars.length > maxChars ? chars.slice(0, maxChars).join('').trimEnd() : flat;
}

const strList = (v: unknown, max: number, maxChars = 200): string[] => (Array.isArray(v) ? v : typeof v === 'string' ? [v] : []).filter((x): x is string => typeof x === 'string').map(x => cleanText(x, maxChars)).filter(Boolean).slice(0, max);

/** Validate and clean drafts: no empty prompts, no line breaks, bounded lengths. */
export function toDrafts(v: unknown): Draft[] {
  if (!Array.isArray(v)) return [];
  const out: Draft[] = [];
  for (const d of v) {
    if (!d || typeof d !== 'object') continue;
    const o = d as Record<string, unknown>;
    const prompt = typeof o.prompt === 'string' ? cleanText(o.prompt, 2000) : '';
    if (!prompt) continue;
    out.push({ prompt, style: strList(o.style, 3), avoid: strList(o.avoid, 2) });
  }
  return out;
}

export function toAdditions(v: unknown): string[] {
  return strList(v, 8, 300);
}
