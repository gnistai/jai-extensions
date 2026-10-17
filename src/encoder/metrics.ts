// Pure helpers: measurement, fitness and aspect reduction.
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

const area = (b: Box): number => Math.max(0, b.xmax - b.xmin) * Math.max(0, b.ymax - b.ymin);
function iou(a: Box, b: Box): number {
  const ix = Math.max(0, Math.min(a.xmax, b.xmax) - Math.max(a.xmin, b.xmin));
  const iy = Math.max(0, Math.min(a.ymax, b.ymax) - Math.max(a.ymin, b.ymin));
  const inter = ix * iy;
  const union = area(a) + area(b) - inter;
  return union > 0 ? inter / union : 0;
}
const pct = (x: number): string => `${Math.round(Math.abs(x) * 100)}%`;
const times = (lnRatio: number): string => `${Math.exp(Math.abs(lnRatio)).toFixed(1)}×`;
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
 * horizon 1, palette 1, lightness 0.5.
 */
export function measure(target: Observation, render: Observation, thumbs?: { target: Thumbnail; render: Thumbnail } | null): Measure {
  // Each difference contributes weight × error; the feedback lists them largest first.
  const items: Array<{ w: number; error: number; text: string | null }> = [];
  const subjects: SubjectMeasure[] = [];
  for (const { name, target: tb, render: r } of subjectPairs(target, render)) {
    const w = area(tb) > 0.4 ? 0.5 : 1;
    if (!r || area(r) <= 0) {
      subjects.push({ name, present: false, dx: 0, dy: 0, sw: 0, sh: 0, iou: 0, error: 1 });
      items.push({ w, error: 1, text: `${name}: missing` });
      continue;
    }
    const tw = Math.max(1e-3, tb.xmax - tb.xmin), th = Math.max(1e-3, tb.ymax - tb.ymin);
    const rw = Math.max(1e-3, r.xmax - r.xmin), rh = Math.max(1e-3, r.ymax - r.ymin);
    const dx = (r.xmin + r.xmax) / 2 - (tb.xmin + tb.xmax) / 2;
    const dy = (r.ymin + r.ymax) / 2 - (tb.ymin + tb.ymax) / 2;
    const sw = Math.log(rw / tw), sh = Math.log(rh / th);
    const o = iou(tb, r);
    // Gentle enough to separate near misses: position in frame fractions, size as log ratios, overlap a little.
    const error = Math.min(1, 1.2 * (Math.abs(dx) + Math.abs(dy)) + 0.35 * (Math.abs(sw) + Math.abs(sh)) + 0.15 * (1 - o));
    subjects.push({ name, present: true, dx, dy, sw, sh, iou: o, error });
    const says: string[] = [];
    if (Math.abs(sw) > 0.15) says.push(`${times(sw)} too ${sw > 0 ? 'wide' : 'narrow'}`);
    if (Math.abs(sh) > 0.15) says.push(`${times(sh)} too ${sh > 0 ? 'tall' : 'short'}`);
    if (Math.abs(dx) > 0.04) says.push(`${pct(dx)} of the frame too far ${dx > 0 ? 'right' : 'left'}`);
    if (Math.abs(dy) > 0.04) says.push(`${pct(dy)} of the frame too ${dy > 0 ? 'low' : 'high'}`);
    items.push({ w, error, text: says.length ? `${name}: ${says.join(', ')}` : null });
  }
  let horizon: number | null = null;
  if (target.horizon !== null) {
    if (render.horizon === null) items.push({ w: 1, error: 0.5, text: `horizon: not visible (target ${pct(target.horizon)} from the top)` });
    else {
      horizon = render.horizon - target.horizon;
      items.push({ w: 1, error: Math.min(1, 4 * Math.abs(horizon)), text: Math.abs(horizon) > 0.03 ? `horizon ${pct(horizon)} of the frame too ${horizon > 0 ? 'low' : 'high'} (at ${pct(render.horizon)} from the top, target ${pct(target.horizon)})` : null });
    }
    if (target.horizonShape && render.horizonShape && render.horizonShape !== target.horizonShape) items.push({ w: 1, error: 1, text: `horizon is ${render.horizonShape}, target ${target.horizonShape}` });
  } else if (render.horizon !== null) items.push({ w: 1, error: 0.3, text: 'a horizon appears; the target has none' });
  let palette: number | null = null, lightness: number | null = null;
  if (thumbs) {
    palette = thumbnailSimilarity(thumbs.target, thumbs.render);
    lightness = meanLightness(thumbs.render) - meanLightness(thumbs.target);
    items.push({ w: 1, error: 1 - palette, text: palette < 0.75 ? `colours and their placement differ (similarity ${palette.toFixed(2)})` : null });
    items.push({ w: 1, error: Math.min(1, Math.abs(lightness) / 30), text: Math.abs(lightness) > 5 ? `overall ${lightness > 0 ? 'too bright' : 'too dark'} (lightness ${lightness > 0 ? '+' : ''}${Math.round(lightness)} of 100)` : null });
  }
  const weights = items.reduce((a, i) => a + i.w, 0);
  const fitness = weights > 0 ? Math.max(0, 1 - items.reduce((a, i) => a + i.w * i.error, 0) / weights) : 0;
  const ranked = items.filter(i => i.text).sort((a, b) => b.w * b.error - a.w * a.error).map(i => i.text!);
  if (render.medium && !same(render.medium, target.medium)) ranked.push(`looks like: ${render.medium} (target: ${target.medium})`);
  if (render.lighting && !same(render.lighting, target.lighting)) ranked.push(`light: ${render.lighting} (target: ${target.lighting})`);
  const feedback = ranked.length ? `Largest first: ${ranked.join('; ')}` : 'matches the measured layout';
  return { fitness, subjects, horizon, palette, lightness, feedback };
}

/** The target's measurements in words, for the optimiser. */
export function describeObservation(o: Observation): string {
  const lines = o.subjects.filter(s => s.box).map(s => {
    const b = s.box!;
    return `- ${s.name}: x ${pct(b.xmin)}–${pct(b.xmax)}, y ${pct(b.ymin)}–${pct(b.ymax)} of the frame (width ${pct(b.xmax - b.xmin)}, height ${pct(b.ymax - b.ymin)})`;
  });
  if (o.horizon !== null) lines.push(`- horizon: ${pct(o.horizon)} from the top${o.horizonShape ? `, ${o.horizonShape}` : ''}`);
  lines.push(`- camera: ${o.camera}; light: ${o.lighting}; medium: ${o.medium}`);
  return lines.join('\n');
}
