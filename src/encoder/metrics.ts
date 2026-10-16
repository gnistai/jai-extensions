// Pure helpers: similarity measures and aspect reduction.
import type { Thumbnail } from './types.ts';

export function cosine(a: readonly number[], b: readonly number[]): number {
  if (a.length !== b.length || !a.length) throw new RangeError('embeddings differ in length');
  let dot = 0, na = 0, nb = 0;
  for (let i = 0; i < a.length; i++) { const x = a[i]!, y = b[i]!; dot += x * y; na += x * x; nb += y * y; }
  return na && nb ? dot / Math.sqrt(na * nb) : 0;
}

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
  for (let h = 1; h <= 32; h++) {
    const w = Math.round(r * h);
    if (w >= 1 && w <= 64 && Math.abs(Math.log(w / h / r)) < 0.01) return `${w}:${h}`;
  }
  const g = gcd(Math.round(width), Math.round(height));
  return `${Math.round(width) / g}:${Math.round(height) / g}`;
}
