// Box geometry shared by the subject and the people measurements.
import type { Box } from './types.ts';

export const area = (b: Box): number => Math.max(0, b.xmax - b.xmin) * Math.max(0, b.ymax - b.ymin);
export const height = (b: Box): number => Math.max(0, b.ymax - b.ymin);

export function iou(a: Box, b: Box): number {
  const ix = Math.max(0, Math.min(a.xmax, b.xmax) - Math.max(a.xmin, b.xmin));
  const iy = Math.max(0, Math.min(a.ymax, b.ymax) - Math.max(a.ymin, b.ymin));
  const inter = ix * iy;
  const union = area(a) + area(b) - inter;
  return union > 0 ? inter / union : 0;
}

/** A frame fraction as a whole percentage, without sign. */
export const pct = (x: number): string => `${Math.round(Math.abs(x) * 100)}%`;
const times = (lnRatio: number): string => `${Math.exp(Math.abs(lnRatio)).toFixed(1)}×`;

/** Where a render's box is relative to the target's. */
export interface Layout {
  /** Centre offset render − target, frame fractions (positive: right, down). */
  dx: number; dy: number;
  /** ln(render size / target size) for width and height. */
  sw: number; sh: number;
  iou: number;
  /** 0..1. */
  error: number;
}

export function layoutOf(target: Box, render: Box): Layout {
  const tw = Math.max(1e-3, target.xmax - target.xmin), th = Math.max(1e-3, target.ymax - target.ymin);
  const rw = Math.max(1e-3, render.xmax - render.xmin), rh = Math.max(1e-3, render.ymax - render.ymin);
  const dx = (render.xmin + render.xmax) / 2 - (target.xmin + target.xmax) / 2;
  const dy = (render.ymin + render.ymax) / 2 - (target.ymin + target.ymax) / 2;
  const sw = Math.log(rw / tw), sh = Math.log(rh / th);
  const o = iou(target, render);
  // Gentle enough to separate near misses: position in frame fractions, size as log ratios, overlap a little.
  const error = Math.min(1, 1.2 * (Math.abs(dx) + Math.abs(dy)) + 0.35 * (Math.abs(sw) + Math.abs(sh)) + 0.15 * (1 - o));
  return { dx, dy, sw, sh, iou: o, error };
}

/** The layout differences worth saying, with numbers: "1.7× too wide", "16% of the frame too low". */
export function layoutWords(l: Pick<Layout, 'dx' | 'dy' | 'sw' | 'sh'>): string[] {
  const says: string[] = [];
  if (Math.abs(l.sw) > 0.15) says.push(`${times(l.sw)} too ${l.sw > 0 ? 'wide' : 'narrow'}`);
  if (Math.abs(l.sh) > 0.15) says.push(`${times(l.sh)} too ${l.sh > 0 ? 'tall' : 'short'}`);
  if (Math.abs(l.dx) > 0.04) says.push(`${pct(l.dx)} of the frame too far ${l.dx > 0 ? 'right' : 'left'}`);
  if (Math.abs(l.dy) > 0.04) says.push(`${pct(l.dy)} of the frame too ${l.dy > 0 ? 'low' : 'high'}`);
  return says;
}
