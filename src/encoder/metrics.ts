// Pure helpers: measurement, fitness and aspect reduction.
import { area, height, layoutOf, layoutWords, pct } from './geometry.ts';
import { EXTRA_HEIGHT, EXTRA_WEIGHT, MAX_TARGET_PEOPLE, comparePeople, crowdDistance, personLabel, personWeight, traitsInWords } from './people.ts';
import type { Box, Measure, Observation, SubjectMeasure, Thumbnail } from './types.ts';

const srgbToLinear = (c: number): number => { const v = c / 255; return v <= 0.04045 ? v / 12.92 : ((v + 0.055) / 1.055) ** 2.4; };
function toLab(r: number, g: number, b: number): [number, number, number] {
  const R = srgbToLinear(r), G = srgbToLinear(g), B = srgbToLinear(b);
  const x = (0.4124 * R + 0.3576 * G + 0.1805 * B) / 0.95047, y = 0.2126 * R + 0.7152 * G + 0.0722 * B, z = (0.0193 * R + 0.1192 * G + 0.9505 * B) / 1.08883;
  const f = (t: number): number => (t > 216 / 24389 ? Math.cbrt(t) : (24389 / 27 * t + 16) / 116);
  const fx = f(x), fy = f(y), fz = f(z);
  return [116 * fy - 16, 500 * (fx - fy), 200 * (fy - fz)];
}

/**
 * Colour and layout similarity of two thumbnails of the same grid size:
 * 1 - mean CIE76 colour difference / 50, clamped to 0..1. Coarse on purpose:
 * it rewards the right palette in the right places, not pixel detail.
 */
export function thumbnailSimilarity(a: Thumbnail, b: Thumbnail): number {
  if (a.width !== b.width || a.height !== b.height) throw new RangeError('thumbnails differ in size');
  const n = a.width * a.height;
  let sum = 0;
  for (let i = 0; i < n; i++) {
    const p = i * 4;
    const [l1, a1, b1] = toLab(a.data[p]!, a.data[p + 1]!, a.data[p + 2]!);
    const [l2, a2, b2] = toLab(b.data[p]!, b.data[p + 1]!, b.data[p + 2]!);
    sum += Math.hypot(l1 - l2, a1 - a2, b1 - b2);
  }
  return Math.max(0, Math.min(1, 1 - sum / n / 50));
}

const gcd = (a: number, b: number): number => (b ? gcd(b, a % b) : a);

/** Spec output convention: the original's ratio, reduced to small whole numbers when that moves it by less than 1%. */
export function reduceAspect(width: number, height: number): string {
  if (!(width > 0 && height > 0)) throw new RangeError('dimensions must be positive');
  const r = width / height;
  // The aspect grammar accepts quotients from 1/100 to 100.
  if (r >= 100) return '100:1';
  if (r <= 0.01) return '1:100';
  for (let h = 1; h <= 32; h++) {
    const w = Math.round(r * h);
    if (w >= 1 && w <= 64 && Math.abs(Math.log(w / h / r)) < 0.01) return `${w}:${h}`;
  }
  const g = gcd(Math.round(width), Math.round(height));
  return `${Math.round(width) / g}:${Math.round(height) / g}`;
}

// ---------------------------------------------------------------- measured fitness


/** Mean CIE L* of a thumbnail. */
export function meanLightness(t: Thumbnail): number {
  let sum = 0;
  const n = t.width * t.height;
  for (let i = 0; i < n; i++) sum += toLab(t.data[i * 4]!, t.data[i * 4 + 1]!, t.data[i * 4 + 2]!)[0];
  return sum / n;
}

/**
 * Same phrase, or one phrase's words all within the other's ("photograph" and
 * "digital photograph"). For the medium and the light only: subjects pair by
 * exact name (subjectPairs), since "tree" and "large tree" are two subjects.
 */
const same = (a: string, b: string): boolean => {
  const words = (x: string): string[] => x.toLowerCase().split(/[^\p{L}\p{N}]+/u).filter(Boolean);
  const wa = words(a), wb = words(b);
  if (!wa.length || !wb.length) return wa.length === wb.length;
  const [short, long] = wa.length <= wb.length ? [wa, new Set(wb)] : [wb, new Set(wa)];
  return short.every(w => long.has(w));
};

/** A subject name as inspections compare it: case and spacing do not count. */
export const subjectKey = (name: string): string => name.trim().replace(/\s+/g, ' ').toLowerCase();

/**
 * The render's box for each target subject that has one, in target order. A
 * render's subjects carry the target's names (toObservation keeps them), so they
 * pair by exact name; a name the target lists twice pairs by occurrence.
 */
function subjectPairs(target: Observation, render: Observation): Array<{ name: string; target: Box; render: Box | null }> {
  const boxes = new Map<string, Array<Box | null>>();
  for (const s of render.subjects) {
    const k = subjectKey(s.name);
    boxes.set(k, [...(boxes.get(k) ?? []), s.box]);
  }
  return target.subjects.flatMap(t => (t.box ? [{ name: t.name, target: t.box, render: boxes.get(subjectKey(t.name))?.shift() ?? null }] : []));
}

/**
 * Compare a render's observation with the target's (and, when given, their
 * thumbnails) and return a fitness in 0..1 with the differences in words.
 * Weights: each subject 1 (regions covering more than 40% of the frame 0.5),
 * horizon 1, palette 1, lightness 1; each person 1 to 3 by height (people.ts)
 * and the background people 0.5, all of the people together capped at as much
 * as everything else. What appears only in the render (an extra prominent
 * person 0.5, background people 0.5, a horizon 1) adds to the error without
 * adding weight, so it can only lower the fitness.
 */
export function measure(target: Observation, render: Observation, thumbs?: { target: Thumbnail; render: Thumbnail } | null): Measure {
  // Each difference contributes weight × error; the feedback lists them largest first.
  const items: Array<{ w: number; error: number; text: string | null; people?: true; renderOnly?: true }> = [];
  const subjects: SubjectMeasure[] = [];
  for (const { name, target: tb, render: r } of subjectPairs(target, render)) {
    const w = area(tb) > 0.4 ? 0.5 : 1;
    if (!r || area(r) <= 0) {
      subjects.push({ name, present: false, dx: 0, dy: 0, sw: 0, sh: 0, iou: 0, error: 1 });
      items.push({ w, error: 1, text: `${name}: missing` });
      continue;
    }
    const l = layoutOf(tb, r);
    subjects.push({ name, present: true, dx: l.dx, dy: l.dy, sw: l.sw, sh: l.sh, iou: l.iou, error: l.error });
    const says = layoutWords(l);
    items.push({ w, error: l.error, text: says.length ? `${name}: ${says.join(', ')}` : null });
  }
  // A full list may have left people out of the target, so a render person no one matched is not counted as extra.
  const full = target.people.length >= MAX_TARGET_PEOPLE;
  const { people, extras } = comparePeople(target.people, render.people, !full);
  for (const [i, pm] of people.entries()) {
    const who = `person ${pm.n} (${pm.label})`;
    const says = [...pm.differs.slice(0, 6).map(d => `${d.trait} ${d.render} (target ${d.target})`), ...layoutWords(pm)];
    items.push({ w: personWeight(target.people[i]!), error: pm.error, people: true, text: !pm.box ? `${who}: missing` : says.length ? `${who}: ${says.join(', ')}` : null });
  }
  const extraPeople = full ? [] : extras.filter(p => height(p.box) >= EXTRA_HEIGHT);
  for (const p of extraPeople) items.push({ w: EXTRA_WEIGHT, error: 1, people: true, renderOnly: true, text: `an extra person, not in the target: ${personLabel(p)} (${pct(height(p.box))} of the frame tall)` });
  const crowdShown = !!target.crowd && target.crowd !== 'none';
  if (crowdShown || (render.crowd && render.crowd !== 'none')) {
    const d = target.crowd && render.crowd ? crowdDistance(render.crowd, target.crowd) : 0.5;
    items.push({ w: 0.5, error: d, people: true, ...(crowdShown ? {} : { renderOnly: true as const }), text: d > 0 ? `other people, too small or too many to list: ${render.crowd ?? 'not read'} (target ${target.crowd ?? 'not read'})` : null });
  }
  let horizon: number | null = null;
  if (target.horizon !== null) {
    if (render.horizon === null) items.push({ w: 1, error: 0.5, text: `horizon: not visible (target ${pct(target.horizon)} from the top)` });
    else {
      horizon = render.horizon - target.horizon;
      items.push({ w: 1, error: Math.min(1, 4 * Math.abs(horizon)), text: Math.abs(horizon) > 0.03 ? `horizon ${pct(horizon)} of the frame too ${horizon > 0 ? 'low' : 'high'} (at ${pct(render.horizon)} from the top, target ${pct(target.horizon)})` : null });
    }
    if (target.horizonShape && render.horizonShape && render.horizonShape !== target.horizonShape) items.push({ w: 1, error: 1, text: `horizon is ${render.horizonShape}, target ${target.horizonShape}` });
  } else if (render.horizon !== null) items.push({ w: 1, error: 0.3, renderOnly: true, text: 'a horizon appears; the target has none' });
  let palette: number | null = null, lightness: number | null = null;
  if (thumbs) {
    palette = thumbnailSimilarity(thumbs.target, thumbs.render);
    lightness = meanLightness(thumbs.render) - meanLightness(thumbs.target);
    items.push({ w: 1, error: 1 - palette, text: palette < 0.75 ? `colours and their placement differ (similarity ${palette.toFixed(2)})` : null });
    items.push({ w: 1, error: Math.min(1, Math.abs(lightness) / 30), text: Math.abs(lightness) > 5 ? `overall ${lightness > 0 ? 'too bright' : 'too dark'} (lightness ${lightness > 0 ? '+' : ''}${Math.round(lightness)} of 100)` : null });
  }
  // People together weigh at most as much as everything else, so that a crowd does not drown the
  // composition; both sides are counted on what the target has.
  const peopleW = items.reduce((a, i) => a + (i.people && !i.renderOnly ? i.w : 0), 0);
  const otherW = items.reduce((a, i) => a + (i.people || i.renderOnly ? 0 : i.w), 0);
  if (otherW > 0 && peopleW > otherW) for (const i of items) if (i.people) i.w *= otherW / peopleW;
  const weights = items.reduce((a, i) => a + (i.renderOnly ? 0 : i.w), 0);
  const errors = items.reduce((a, i) => a + i.w * i.error, 0);
  const fitness = weights > 0 ? Math.max(0, 1 - errors / weights) : 0;
  const ranked = items.filter(i => i.text).sort((a, b) => b.w * b.error - a.w * a.error).map(i => i.text!);
  if (render.medium && !same(render.medium, target.medium)) ranked.push(`looks like: ${render.medium} (target: ${target.medium})`);
  if (render.lighting && !same(render.lighting, target.lighting)) ranked.push(`light: ${render.lighting} (target: ${target.lighting})`);
  const feedback = ranked.length ? `Largest first: ${ranked.join('; ')}` : 'matches the measured layout';
  return { fitness, subjects, people, extraPeople: extraPeople.length, horizon, palette, lightness, feedback };
}

/** The target's measurements in words, for the optimiser. */
export function describeObservation(o: Observation): string {
  const lines = o.subjects.filter(s => s.box).map(s => {
    const b = s.box!;
    return `- ${s.name}: x ${pct(b.xmin)}–${pct(b.xmax)}, y ${pct(b.ymin)}–${pct(b.ymax)} of the frame (width ${pct(b.xmax - b.xmin)}, height ${pct(b.ymax - b.ymin)})`;
  });
  for (const [i, p] of o.people.entries()) {
    const b = p.box, traits = traitsInWords(p);
    lines.push(`- person ${i + 1} (${personLabel(p)}): x ${pct(b.xmin)}–${pct(b.xmax)}, y ${pct(b.ymin)}–${pct(b.ymax)} of the frame (${pct(b.ymax - b.ymin)} of the frame tall)${p.face ? `; face ${p.face}` : ''}${traits ? `; ${traits}` : ''}`);
  }
  if (o.crowd && (o.crowd !== 'none' || o.people.length)) lines.push(`- other people, too small or too many to list: ${o.crowd}`);
  if (o.horizon !== null) lines.push(`- horizon: ${pct(o.horizon)} from the top${o.horizonShape ? `, ${o.horizonShape}` : ''}`);
  lines.push(`- camera: ${o.camera}; light: ${o.lighting}; medium: ${o.medium}`);
  return lines.join('\n');
}
