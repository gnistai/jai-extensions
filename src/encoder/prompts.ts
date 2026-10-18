// Instructions for the analyst. They condense the JAI authoring guidance
// (spec: Writing JAI by hand; skills/jai/SKILL.md) and the encoder's task.
import { height, iou } from './geometry.ts';
import { subjectKey } from './metrics.ts';
import { CROWD, FACES, MAX_RENDER_PEOPLE, MAX_TARGET_PEOPLE, PROMINENT, TRAITS, pick, toPerson } from './people.ts';
import type { Box, Draft, Observation, ProposeContext } from './types.ts';

export const SYSTEM = `You write JAI image descriptions. A JAI description is plain natural-language text that text-to-image models render. Your job is compression: given a target image, write text from which the named image models reproduce the target as closely as possible.

What must match, in order of importance: subject and exact counts; the people, where each one is and how they look (apparent age and gender, skin, hair, eyes, face); composition and positions (left, right, centre, foreground, background, camera angle, distance, framing, horizon height); colours and palette; light (source, direction, time of day); medium and style (photograph, oil painting, 3D render, flat vector...); mood and texture.

Rules:
- Describe only what is visible. Concrete nouns, digits for counts, plain spatial words.
- When the size or position of a main subject matters, state it concretely as a share of the frame: "small, about one sixth of the frame width, just right of centre, halfway up". Image models ignore vague words like "small" or "distant".
- Start with the medium ("Photograph of...", "Watercolour painting of...").
- One paragraph, no lists, no headings, no line breaks; keep within the word limit given in the task.
- Put the essentials (medium, main subject with its size and position, setting, dominant colours) in the first 50 words: some models read only about the first 60 words.
- Literal text visible in the image goes in straight double quotes, exactly as spelled, only if legible; leave out people's names (name tags, captions, shirts).
- People are what viewers look at first. For each prominent person give their place and size in the frame, then how they look, in natural phrases: apparent age and gender, pose and expression, skin tone in plain colour words with its tint when visible (pinkish, golden, olive), hair colour, length, texture and style, eye colour, the one or two features of the face that stand out most (for example a broad face with prominent cheekbones, a wide mouth, full lips, a narrow nose with a high bridge, monolid eyes, freckles), facial hair, glasses, build and clothing colours. Name what is there and stands out; leave out what is absent or middling ("no glasses", "a medium nose", "an average build") and marks that single out one person (moles, scars, birthmarks). Skin tone and hair colour are never middling: name them for every person in whom they are visible, light skin as much as dark, because when a description leaves them out each image model fills in its own default. Spend words in proportion to prominence: a close-up portrait gets all of this, a small figure needs age, gender, skin tone, hair and clothing; with a short word limit keep the colouring and the one feature that stands out most.
- Describe features, never a group: no ethnicity, race, nationality, region or religion, not even as an adjective on a feature ("Nordic features", "Asian eyes", "Mediterranean skin") and not even to save words; a label pulls the picture toward a stereotype, the features toward this person. Use plain colour words for skin, never food, wood or porcelain. A description that names a group is not rendered.
- Never name or guess the identity of a real person, not even as a likeness ("resembling …"), and do not describe anyone as a celebrity or public figure.
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
  return `You measure images for layout comparison. You report what is visible, with bounding boxes as ${b.field} ${b.shape} in integers from 0 to 1000 ${b.origin}, and the visible appearance of people, never who they are. Be precise: boxes tight around each subject${order === 'xy' ? ', even very small ones' : ''}. Answer with one JSON object only, no Markdown fences.`;
}
export const INSPECT_SYSTEM = inspectSystem('yx');

const OBS_SHAPE = '"horizon_y": integer 0-1000 or null (where the horizon or the line between ground and sky is, 0 = top, measured at the middle of the frame), "horizon_shape": "flat", "curved" or null, "camera": short phrase (angle and distance), "lighting": short phrase, "medium": short phrase (photograph, oil painting, 3D render, ...)';

const words = (values: readonly string[]): string => values.map(v => JSON.stringify(v)).join(' | ');
/** The people part of an inspection: the same reading for the target and for every render. */
function peopleTask(order: BoxOrder, render: boolean): { rules: string; shape: string } {
  const b = BOXES[order];
  const rules = `People are listed apart from the subjects: each person at least ${render ? 'a twentieth' : 'a tenth'} of the frame tall, largest first, at most ${render ? MAX_RENDER_PEOPLE : MAX_TARGET_PEOPLE}, each with a tight box around the whole visible person; an empty list when there are none. For each, read only what is visible, in the words given, with colours as they would look in neutral daylight, not tinted by the light. "face" is "clear" when the eyes are large enough to see their colour, "small" when the face shows but is smaller than that, "hidden" when it is turned away or covered. First write "stands_out": up to 12 plain words on what is most distinctive in this person's face and colouring, features only, never a group or nationality. Then judge each feature against the same face, not against other people: nose width against the gap between the inner corners of the eyes ("narrow" clearly narrower, "medium" about equal, "wide" clearly wider); mouth width against the eyes ("narrow" when the corners sit well inside the inner edges of the irises, "medium" about below the inner edges, "wide" below the pupils or further out; allow for a smile); face width at the cheekbones against the face's length ("narrow" long and slim, "broad" wide and short); cheekbones "prominent" when they are the widest part of the face with a shadow below, "flat" when the cheeks run smoothly from eyes to jaw; lips "thin" when they show as narrow bands, "full" when rounded and prominent, "very full" when they are among the first things one notices; nose bridge "low" when it barely rises between the eyes, "high" when it stands out as a ridge, null in a straight-on view that does not show it; eyelids "double" with a visible crease, "hooded" when a fold of skin covers the crease, "monolid" with no crease. Age: "young adult" about 18 to 35, "middle-aged" about 35 to 60, "older adult" over 60. Hair length "covered" when a headscarf, hat or hood hides the hair. Give null for any trait you cannot see clearly: pose and build need the body in view (not in a head-and-shoulders picture); eye colour, eyelids, nose, mouth, lips, face width, cheekbones, undertone and freckles need a clear face. "crowd" is the other people, too small or too many to list: "none", "few" (up to 5), "some" (up to 20) or "many".`;
  const person = `{"${b.field}": ${b.shape}, "face": ${words(FACES)} | null, "stands_out": string (up to 12 words), ${TRAITS.map(t => `"${t.key}": ${words(t.values)} | null`).join(', ')}}`;
  return { rules, shape: `"people": [${person}], "crowd": ${words(CROWD)} | null` };
}

export function inspectTargetPrompt(order: BoxOrder = 'yx'): string {
  const b = BOXES[order];
  const people = peopleTask(order, false);
  return `List up to 5 of the most important visible subjects or regions other than people, most important first (animals, objects, buildings, a planet, the ground or sea when it defines the composition). Name each with a short generic noun phrase that would let someone find it again in a similar picture ("red bicycle", "lunar surface").
${people.rules}
Answer: {"subjects": [{"name": string, "${b.field}": ${b.shape}}], ${people.shape}, ${OBS_SHAPE}}`;
}

export function inspectRenderPrompt(subjects: string[], order: BoxOrder = 'yx'): string {
  const b = BOXES[order];
  const people = peopleTask(order, true);
  return `Find each of these in the image: ${subjects.map((x, i) => `${i + 1}. ${x}`).join('; ')}. For each, give its box, or null when it is not visible.${order === 'xy' ? ' Look carefully: a subject may be very small.' : ''} Use the names exactly as given, in the same order.
${people.rules}
Answer: {"subjects": [{"name": string, "${b.field}": ${b.shape} or null}], ${people.shape}, ${OBS_SHAPE}}`;
}

const clamp01 = (x: unknown): number | null => (typeof x === 'number' && Number.isFinite(x) ? Math.min(1, Math.max(0, x / 1000)) : null);

/** A box from an answer's four numbers in `order`, as frame fractions, or null when it is not a box. */
function boxOf(v: unknown, order: BoxOrder): Box | null {
  const n = Array.isArray(v) && v.length === 4 ? v.map(clamp01) : null;
  // As [ymin, xmin, ymax, xmax] whichever order was asked for.
  const b = n && (order === 'xy' ? [n[1], n[0], n[3], n[2]] : n);
  return b && b.every(x => x !== null) && b[2]! > b[0]! && b[3]! > b[1]! ? { ymin: b[0]!, xmin: b[1]!, ymax: b[2]!, xmax: b[3]! } : null;
}

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

/**
 * Validate an inspection answer into an Observation. Subjects follow `expected`
 * (a render's inspection) when given. People are read the same way for the
 * target and for renders, largest first; the target keeps only the prominent
 * ones, a render also the smaller ones it was asked for, so that a person the
 * render shrank is still found and paired.
 */
export function toObservation(a: Record<string, unknown>, expected: string[] | null, order: BoxOrder = 'yx'): Observation {
  const raw = Array.isArray(a.subjects) ? a.subjects : [];
  const parsed = raw.map(s => {
    const o = (s ?? {}) as Record<string, unknown>;
    const name = typeof o.name === 'string' ? cleanText(o.name, 60) : '';
    return { name, box: boxOf(o[BOXES[order].field], order) };
  }).filter(s => s.name);
  const listed = (Array.isArray(a.people) ? a.people : []).flatMap(p => {
    const o = (p && typeof p === 'object' ? p : {}) as Record<string, unknown>;
    const box = boxOf(o[BOXES[order].field], order);
    return box && (expected || height(box) >= PROMINENT) ? [toPerson(box, o)] : [];
  }).sort((x, y) => height(y.box) - height(x.box));
  // One person listed twice (nearly the same box) is one person.
  const people = listed.filter((p, i) => !listed.slice(0, i).some(q => iou(p.box, q.box) >= 0.8)).slice(0, expected ? MAX_RENDER_PEOPLE : MAX_TARGET_PEOPLE);
  // A target subject that is one of the people (nearly the same box) is measured as the person only.
  const subjects = expected ? followNames(parsed, expected) : parsed.filter(s => !s.box || !people.some(p => iou(s.box!, p.box) >= 0.8)).slice(0, 5);
  const text = (v: unknown): string => (typeof v === 'string' ? cleanText(v, 80) : '');
  const shape = typeof a.horizon_shape === 'string' && /^(flat|curved)$/i.test(a.horizon_shape.trim()) ? a.horizon_shape.trim().toLowerCase() : '';
  return { subjects, people, crowd: pick(CROWD, a.crowd), horizon: clamp01(a.horizon_y), horizonShape: shape, camera: text(a.camera), lighting: text(a.lighting), medium: text(a.medium) };
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

Write ${ctx.n} new descriptions, each at most ${ctx.maxWords ?? 120} words (some models read only that much; words beyond it are lost), that reduce the largest measured differences (listed first in each feedback line) while keeping what already matches. Viewers notice a changed or missing person before anything else: when the feedback says a person is missing or looks different (skin tone, hair, eyes, the shape of the face), describe that person more fully and earlier. Keep each person's visible colouring (skin tone, hair colour, eye colour) in every description, even where the renders already match it. Image models respond to plain visual language, not to numbers: say "small, about a seventh of the frame wide", "a little right of centre", "just below the middle", "a low, flat horizon near the bottom quarter", "dark grey-brown ground"; never write coordinates, percentages of bounding boxes or measurements. Put the essentials first; describe and never instruct. Mix two kinds: make about half of them minimal edits of attempt #1, changing only the words behind its one or two largest differences and keeping every other word; make the rest fresh rewrites that try clearly different wording, order, emphasis or camera and framing terms. Learn from the history: when a phrasing did not move a measurement, do not repeat it. Make the ${ctx.n} descriptions differ from each other.
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

// Descriptions that name a group or liken someone to a named person are never
// rendered (spec: Encoding images): a label is the most compressive description
// of a face there is, so a loop that rendered them would select them, and they
// would ship as alt text. Colour words count only before a word for people
// ("a white woman", not "a white shirt" or "black-and-white"); a capitalised
// demonym (Swedish, Italian, Japanese) counts before a person or a look when it
// is not the first word of a sentence, where any adjective is capitalised.
const PERSON_WORDS = String.raw`wom[ae]n|m[ae]n|girls?|boys?|persons?|people|child(?:ren)?|kids?|teen(?:ager)?s?|adults?|lad(?:y|ies)|gentlem[ae]n|couples?|famil(?:y|ies)`;
const LOOK_WORDS = String.raw`features|faces?|eyes|skin|complexion|looks|appearance|descent|heritage|origin|ancestry`;
const GROUPS = String.raw`nordic|scandinavian|slavic|mediterranean|(?:north|south|east|west)(?:ern)?[- ]european|european|caucasian|african|asian|east[- ]asian|south[- ]asian|southeast[- ]asian|middle[- ]eastern|latin[aox]|latinx|hispanic|indigenous|aboriginal|mixed[- ]race|biracial|arab|han|french|dutch|greek|swiss|welsh|czech|thai|lao|filipin[ao]|basque|sami|inuit|maori|jewish|muslim|christian|hindu|sikh|buddhist`;
const AFTER = String.raw`(?:[- ]looking)?\s+(?:[a-z-]+\s+)?`;
const GROUP_LABEL = new RegExp(String.raw`\b(?:${GROUPS})${AFTER}(?:${PERSON_WORDS}|${LOOK_WORDS})\b`, 'i');
const COLOUR_LABEL = new RegExp(String.raw`\b(?<!and[- ])(?:white|black)\s+(?:${PERSON_WORDS})\b`, 'i');
const DEMONYM_LABEL = new RegExp(String.raw`(?<!^|[.!?]\s*)\b[A-Z][a-z]+(?:ish|ese|ian|an|i|ic)${AFTER}(?:${PERSON_WORDS}|${LOOK_WORDS})\b`);
const LIKENESS = /\b(?:resembl\w*|look-?alike|reminiscent of|looks like)\s+(?:a young |an older )?[A-Z][a-z]+\s+[A-Z][a-z]+/;

/** Why a description must not be rendered, or null when it may be. */
export function refusal(text: string): string | null {
  if (GROUP_LABEL.test(text) || COLOUR_LABEL.test(text) || DEMONYM_LABEL.test(text)) return 'it names a group (ethnicity, nationality, region or religion); describe the visible features instead';
  if (LIKENESS.test(text)) return 'it likens someone to a named person; describe the visible features instead';
  return null;
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
