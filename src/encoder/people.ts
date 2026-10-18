// People in the encoder's measurements (spec: Encoding images). Viewers look at
// people first and notice at once when a render changes who is in the picture,
// so people are measured on their own, weighted by how tall they appear in the
// frame, and compared by visible traits from closed lists: two inspections then
// compare word for word, and the kind of person (age, skin, hair, eyes, the
// shape of the face) survives encoding even though the face itself does not.
// Appearance only, never identity: no names, and features rather than
// ethnicity or nationality labels, which would pull renders toward a
// stereotype instead of this person.
import { height, layoutOf } from './geometry.ts';
import type { Box, FaceVisibility, Person, PersonMeasure } from './types.ts';

export interface Trait {
  /** The JSON field the inspector fills. */
  key: string;
  /** The trait in feedback: "eyes brown, target blue". */
  label: string;
  values: readonly string[];
  /**
   * Values on an ordered scale (all of them: true): a difference costs by the
   * steps between, a quarter for one step (a reading's usual wobble), three
   * quarters for two, in full for more or between the two ends.
   */
  ordered?: true | readonly string[];
  /** Costs of 0, 1, 2… steps on this scale, in place of STEPS. */
  steps?: readonly number[];
  /** Other pairs a quarter apart; any other difference costs in full. */
  near?: ReadonlyArray<readonly [string, string]>;
  /** Weight in the trait error; the traits that make the kind of person weigh most. */
  weight: number;
  /** Where it can be read: on the whole person, on a visible face, or only on a clear face. */
  level: 'body' | 'face' | 'detail';
  /** The unremarkable value, left out when the traits are put in words (a description names what stands out). */
  plain?: string;
}

const SHADES = ['black', 'dark brown', 'brown', 'light brown', 'dark blonde', 'blonde', 'platinum blonde'];
const LENGTHS = ['bald or shaved', 'very short', 'short', 'chin-length', 'shoulder-length', 'long', 'very long'];
const COLOURS = ['black', 'white', 'grey', 'beige', 'brown', 'red', 'orange', 'yellow', 'green', 'blue', 'navy', 'purple', 'pink', 'multicoloured'];

export const TRAITS: readonly Trait[] = [
  { key: 'gender', label: 'apparent gender', values: ['female', 'male', 'androgynous'], near: [['female', 'androgynous'], ['male', 'androgynous']], weight: 4.5, level: 'body' },
  { key: 'age', label: 'age', values: ['baby', 'child', 'teen', 'young adult', 'middle-aged', 'older adult'], ordered: true, weight: 3, level: 'body' },
  // Skin tone keeps counting past three steps: very light against very dark is the largest change of all.
  { key: 'skin_tone', label: 'skin tone', values: ['very light', 'light', 'light-medium', 'medium', 'medium-dark', 'dark', 'very dark'], ordered: true, steps: [0, 0.15, 0.5, 0.8, 1], weight: 4.5, level: 'body' },
  { key: 'hair_colour', label: 'hair', values: [...SHADES, 'auburn', 'red', 'grey', 'white'], ordered: SHADES, near: [['auburn', 'red'], ['auburn', 'brown'], ['auburn', 'dark brown'], ['grey', 'white'], ['white', 'platinum blonde']], weight: 2, level: 'body' },
  { key: 'hair_length', label: 'hair length', values: [...LENGTHS, 'tied back', 'covered'], ordered: LENGTHS, near: [['tied back', 'shoulder-length'], ['tied back', 'long'], ['tied back', 'very long']], weight: 1, level: 'body' },
  { key: 'hair_texture', label: 'hair texture', values: ['straight', 'wavy', 'curly', 'coily'], ordered: true, weight: 1, level: 'body' },
  { key: 'build', label: 'build', values: ['slim', 'average', 'large'], ordered: true, weight: 0.5, level: 'body', plain: 'average' },
  {
    key: 'clothing', label: 'clothing', values: COLOURS, weight: 1, level: 'body',
    near: [['black', 'navy'], ['black', 'grey'], ['grey', 'white'], ['white', 'beige'], ['beige', 'brown'], ['beige', 'yellow'], ['brown', 'orange'], ['orange', 'red'], ['orange', 'yellow'], ['red', 'pink'], ['pink', 'purple'], ['purple', 'blue'], ['blue', 'navy'], ['green', 'blue']],
  },
  { key: 'pose', label: 'pose', values: ['standing', 'walking', 'running', 'sitting', 'crouching', 'lying'], near: [['standing', 'walking'], ['walking', 'running'], ['sitting', 'crouching'], ['sitting', 'lying']], weight: 0.5, level: 'body' },
  { key: 'facing', label: 'facing', values: ['camera', 'left', 'right', 'away'], near: [['camera', 'left'], ['camera', 'right'], ['left', 'away'], ['right', 'away']], weight: 0.5, level: 'body' },
  { key: 'facial_hair', label: 'facial hair', values: ['none', 'stubble', 'moustache', 'goatee', 'short beard', 'full beard'], near: [['none', 'stubble'], ['stubble', 'moustache'], ['stubble', 'goatee'], ['stubble', 'short beard'], ['moustache', 'goatee'], ['goatee', 'short beard'], ['short beard', 'full beard']], weight: 1, level: 'face', plain: 'none' },
  { key: 'glasses', label: 'glasses', values: ['none', 'glasses', 'sunglasses'], near: [['glasses', 'sunglasses']], weight: 1, level: 'face', plain: 'none' },
  { key: 'expression', label: 'expression', values: ['neutral', 'smiling', 'laughing', 'serious', 'frowning', 'surprised'], near: [['neutral', 'smiling'], ['smiling', 'laughing'], ['neutral', 'serious'], ['serious', 'frowning']], weight: 0.5, level: 'face' },
  // Light eyes (green, grey, blue) are near one another; the divide that shows is light against dark.
  { key: 'eye_colour', label: 'eyes', values: ['dark brown', 'brown', 'hazel', 'green', 'grey', 'blue'], ordered: ['dark brown', 'brown', 'hazel', 'green'], near: [['green', 'grey'], ['grey', 'blue'], ['green', 'blue']], weight: 2, level: 'detail' },
  { key: 'eyelids', label: 'eyelids', values: ['monolid', 'hooded', 'double'], ordered: true, weight: 1, level: 'detail' },
  { key: 'nose_width', label: 'nose width', values: ['narrow', 'medium', 'wide'], ordered: true, weight: 1, level: 'detail', plain: 'medium' },
  { key: 'nose_bridge', label: 'nose bridge', values: ['low', 'medium', 'high'], ordered: true, weight: 1, level: 'detail', plain: 'medium' },
  { key: 'lips', label: 'lips', values: ['thin', 'medium', 'full', 'very full'], ordered: true, weight: 1, level: 'detail', plain: 'medium' },
  { key: 'mouth_width', label: 'mouth width', values: ['narrow', 'medium', 'wide'], ordered: true, weight: 0.5, level: 'detail', plain: 'medium' },
  { key: 'face_width', label: 'face width', values: ['narrow', 'medium', 'broad'], ordered: true, weight: 1, level: 'detail', plain: 'medium' },
  { key: 'cheekbones', label: 'cheekbones', values: ['flat', 'average', 'prominent'], ordered: true, weight: 1, level: 'detail', plain: 'average' },
  // Read as in neutral daylight, but light still tints it (golden hour reads golden), so it weighs less.
  { key: 'undertone', label: 'skin undertone', values: ['pinkish', 'neutral', 'golden', 'olive'], near: [['pinkish', 'neutral'], ['neutral', 'golden'], ['neutral', 'olive'], ['golden', 'olive']], weight: 0.5, level: 'detail' },
  { key: 'freckles', label: 'freckles', values: ['none', 'some', 'many'], ordered: true, weight: 0.5, level: 'detail', plain: 'none' },
];

export const FACES: readonly FaceVisibility[] = ['clear', 'small', 'hidden'];
/** The people too small or too many to list: none, up to 5, up to 20, more. */
export const CROWD = ['none', 'few', 'some', 'many'] as const;
/** People an inspection lists: of the target, those at least a tenth of the frame tall; of a render, down to a twentieth, so that a shrunken person is still found. */
export const MAX_TARGET_PEOPLE = 5, MAX_RENDER_PEOPLE = 6;
/** Target people below this share of the frame height are left to the crowd (inspectors are asked for a tenth; their boxes vary a little). */
export const PROMINENT = 0.08;
/** Weighted trait differences that make a different kind of person, for example gender and a large change of skin tone together. */
const DIFFERENT_KIND = 6;
/** A face less readable in the render costs at most this: a turned-away face, three quarters of a different person. */
const FACE_CAP = 4.5;
/** A render person this tall or taller who matches no one in the target is an extra person (and costs EXTRA_WEIGHT). */
export const EXTRA_HEIGHT = 0.1, EXTRA_WEIGHT = 0.5;

const norm = (s: string): string => s.trim().replace(/[\s-]+/g, ' ').toLowerCase().replace(/\bgray\b/g, 'grey').replace(/colored\b/g, 'coloured');
/** Words analysts use for a listed one (the local grammar allows only the listed words). */
const SYNONYMS: Record<string, string> = {
  teenager: 'teen', woman: 'female', man: 'male', bald: 'bald or shaved', shaved: 'bald or shaved',
  'sun glasses': 'sunglasses', eyeglasses: 'glasses', spectacles: 'glasses', beard: 'full beard', mustache: 'moustache',
};
/** One of `values`, matched without regard to case, spaces, hyphens or US spelling, else null. */
export function pick<T extends string>(values: readonly T[], v: unknown): T | null {
  if (typeof v !== 'string') return null;
  const k = norm(v), alias = SYNONYMS[k];
  return values.find(x => norm(x) === k) ?? (alias ? values.find(x => x === alias) ?? null : null);
}

/** The cost of a difference of 0, 1, 2 or more steps on an ordered scale. */
const STEPS = [0, 0.25, 0.75, 1];
interface Scale { ordered: readonly string[]; steps: readonly number[]; near: Set<string> }
const scaleOf = (values: readonly string[], ordered: Trait['ordered'], near: Trait['near'] = [], steps: readonly number[] = STEPS): Scale => ({
  ordered: ordered === true ? values : ordered ?? [],
  steps,
  near: new Set(near.flatMap(([a, b]) => [`${a}|${b}`, `${b}|${a}`])),
});
function distance(s: Scale, a: string, b: string): number {
  if (a === b) return 0;
  const i = s.ordered.indexOf(a), j = s.ordered.indexOf(b);
  // The two ends of a scale are as different as it gets (monolid and double, slim and large).
  if (i >= 0 && j >= 0) return Math.abs(i - j) >= s.ordered.length - 1 ? 1 : s.steps[Math.min(s.steps.length - 1, Math.abs(i - j))]!;
  return s.near.has(`${a}|${b}`) ? STEPS[1]! : 1;
}
const SCALES = new Map<string, Scale>(TRAITS.map(t => [t.key, scaleOf(t.values, t.ordered, t.near, t.steps)]));
const CROWD_SCALE = scaleOf(CROWD, true);

/** 0 for the same value, up to 1 for values far apart (see Trait.ordered and Trait.near). */
export const traitDistance = (t: Trait, a: string, b: string): number => distance(SCALES.get(t.key)!, a, b);
export const crowdDistance = (a: string, b: string): number => distance(CROWD_SCALE, a, b);

/**
 * A person from an inspection answer (the box already read): every trait one
 * of its words or null, and what the face's visibility cannot show dropped, so
 * that a guess at the eye colour of a small face never counts.
 */
export function toPerson(box: Box, raw: Record<string, unknown>): Person {
  const face = pick(FACES, raw.face);
  const traits: Record<string, string | null> = {};
  for (const t of TRAITS) {
    const readable = t.level === 'body' || (t.level === 'face' ? face !== 'hidden' : face === 'clear');
    traits[t.key] = readable ? pick(t.values, raw[t.key]) : null;
  }
  return { box, face, traits };
}

/** A person's weight in the fitness: 1, rising to 3 for someone half the frame tall or more. */
export const personWeight = (p: Person): number => 1 + 2 * Math.min(1, height(p.box) / 0.5);

/** By age, as TRAITS lists the ages. */
const NOUNS: Record<string, string[]> = {
  female: ['baby girl', 'girl', 'teenage girl', 'young woman', 'middle-aged woman', 'older woman'],
  male: ['baby boy', 'boy', 'teenage boy', 'young man', 'middle-aged man', 'older man'],
  '': ['baby', 'child', 'teenager', 'young adult', 'middle-aged adult', 'older adult'],
};
const AGES = TRAITS.find(t => t.key === 'age')!.values;

/** A person in a few words: "young woman at left". */
export function personLabel(p: Person): string {
  const g = p.traits.gender === 'female' || p.traits.gender === 'male' ? p.traits.gender : '';
  const i = p.traits.age ? AGES.indexOf(p.traits.age) : -1;
  const noun = i >= 0 ? NOUNS[g]![i]! : g === 'female' ? 'woman' : g === 'male' ? 'man' : 'person';
  const cx = (p.box.xmin + p.box.xmax) / 2;
  return `${noun} at ${cx < 1 / 3 ? 'left' : cx > 2 / 3 ? 'right' : 'centre'}`;
}

/** The traits in words, for the optimiser: "skin tone very light, hair platinum blonde, eyes blue"; unremarkable values left out. */
export function traitsInWords(p: Person): string {
  return TRAITS.flatMap(t => (p.traits[t.key] && p.traits[t.key] !== t.plain ? [`${t.label} ${p.traits[t.key]}`] : [])).join(', ');
}

const READABLE: Record<Trait['level'], readonly FaceVisibility[]> = { body: FACES, face: ['clear', 'small'], detail: ['clear'] };
const SEEN: Record<FaceVisibility, number> = { clear: 2, small: 1, hidden: 0 };

/**
 * What two readings of a person disagree on, most important first, and the
 * trait error (0..1): weighted differences, saturating at a different kind of
 * person. A trait the target shows and the render does not costs half its
 * weight, an even chance of being wrong, so that hiding a feature never comes
 * free; a detail left unread on a clear face costs nothing, since a clear face
 * hides none of them and the inspector sometimes skips a few even there. The
 * details a less readable face hides are charged to the face,
 * at most FACE_CAP: half their weight when it is turned away or covered, a
 * quarter when it is small, nothing when it is small because the whole person
 * is (the layout charges that). A render face the inspector did not classify
 * counts as small.
 */
function compareTraits(target: Person, render: Person): { differs: PersonMeasure['differs']; error: number } {
  const found: Array<{ trait: string; render: string; target: string; cost: number }> = [];
  const face = render.face ?? (target.face ? 'small' : null);
  const shrank = height(render.box) < 0.8 * height(target.box);
  let hidden = 0;
  for (const t of TRAITS) {
    const a = target.traits[t.key], b = render.traits[t.key];
    if (!a) continue;
    if (b) { if (b !== a) found.push({ trait: t.label, render: b, target: a, cost: t.weight * traitDistance(t, b, a) }); continue; }
    if (face && !READABLE[t.level].includes(face)) hidden += face === 'hidden' ? t.weight / 2 : shrank ? 0 : t.weight / 4;
    else if (t.level !== 'detail') found.push({ trait: t.label, render: 'not seen', target: a, cost: t.weight / 2 });
  }
  if (target.face && face && SEEN[face] < SEEN[target.face]) found.push({ trait: 'face', render: face, target: target.face, cost: Math.min(FACE_CAP, hidden + 0.5 * (SEEN[target.face] - SEEN[face])) });
  found.sort((x, y) => y.cost - x.cost);
  const sum = found.reduce((s, f) => s + f.cost, 0);
  return { differs: found.map(({ trait, render: r, target: t }) => ({ trait, render: r, target: t })), error: Math.min(1, sum / DIFFERENT_KIND) };
}

/**
 * Pair the render's people with the target's at the lowest cost to the
 * fitness: each target person's weight times their error (their full weight
 * when unpaired), plus EXTRA_WEIGHT for each prominent render person left over
 * when `extras` is true. So the same kind of person pairs up even when the
 * render moved them. Each target person gets a measure in target order; render
 * people left unpaired are returned as extras. At most MAX_TARGET_PEOPLE and
 * MAX_RENDER_PEOPLE are compared (the search is exhaustive).
 */
export function comparePeople(targetAll: Person[], renderAll: Person[], extras = true): { people: PersonMeasure[]; extras: Person[] } {
  const target = targetAll.slice(0, MAX_TARGET_PEOPLE), render = renderAll.slice(0, MAX_RENDER_PEOPLE);
  const weights = target.map(personWeight);
  const pair = target.map(t => render.map(r => {
    const layout = layoutOf(t.box, r.box);
    const traits = compareTraits(t, r);
    // Wrong when either the place or the look is wrong: someone of another kind counts as much as no
    // one, while the right person in the wrong place still beats no one.
    return { layout, traits, error: 1 - (1 - 0.6 * layout.error) * (1 - traits.error) };
  }));
  const leftOver = (used: boolean[]): number => (extras ? render.filter((r, j) => !used[j] && height(r.box) >= EXTRA_HEIGHT).length * EXTRA_WEIGHT : 0);
  let best = Infinity, bestPick: Array<number | null> = [];
  const pickNow: Array<number | null> = [];
  const used = new Array<boolean>(render.length).fill(false);
  const search = (i: number, sum: number): void => {
    if (sum >= best) return;
    if (i === target.length) {
      const total = sum + leftOver(used);
      if (total < best) { best = total; bestPick = [...pickNow]; }
      return;
    }
    for (let j = 0; j < render.length; j++) {
      if (used[j]) continue;
      used[j] = true; pickNow.push(j);
      search(i + 1, sum + weights[i]! * pair[i]![j]!.error);
      pickNow.pop(); used[j] = false;
    }
    pickNow.push(null);
    search(i + 1, sum + weights[i]!);
    pickNow.pop();
  };
  search(0, 0);
  const people = target.map((t, i): PersonMeasure => {
    const j = bestPick[i] ?? null;
    const label = personLabel(t);
    if (j === null) return { n: i + 1, label, box: null, dx: 0, dy: 0, sw: 0, sh: 0, differs: [], layoutError: 1, traitError: 1, error: 1 };
    const { layout, traits, error } = pair[i]![j]!;
    return { n: i + 1, label, box: render[j]!.box, dx: layout.dx, dy: layout.dy, sw: layout.sw, sh: layout.sh, differs: traits.differs, layoutError: layout.error, traitError: traits.error, error };
  });
  const taken = new Set(bestPick.filter((j): j is number => j !== null));
  return { people, extras: render.filter((_, j) => !taken.has(j)) };
}
