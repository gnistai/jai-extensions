// Encoder engine types. The engine is UI-free and environment-neutral: apps
// supply the target image (and, optionally, a thumbnail decoder) and the
// providers; the engine supplies the analysis-by-synthesis loop and budgeting.
// Spec: "Encoding images" (informative) and its output conventions.
import type { JaiDocument } from '../jai/index.ts';
import type { Renderer, RenderResult } from '../renderers/types.ts';

/** An image a provider can fetch: an https: URL or a data: URL. */
export interface ImageRef { url: string }

/** The original image. `url` should point at a downscaled copy (about 1 megapixel or less). */
export interface TargetImage extends ImageRef { width: number; height: number }

/** A candidate core description (spec: prompt, style, avoid). */
export interface Draft { prompt: string; style: string[]; avoid: string[] }

export interface AnalystContext {
  /** Labels of the target models, best known first. */
  models: string[];
  signal?: AbortSignal;
}

export interface AnalystReply<T> {
  value: T;
  /** What the analyst saw (differences, reasoning summary), for the report. */
  notes: string;
  /** Estimated cost in USD of this call. */
  cost: number;
}

export interface RenderView { model: string; label: string; image: ImageRef; score: number }

/** A vision-language model that writes and revises descriptions. */
export interface Analyst {
  readonly id: string;
  /** Estimated USD per call, used to reserve budget before calling. */
  readonly costHint: number;
  /** Write n diverse, faithful first drafts. */
  describe(target: ImageRef, n: number, ctx: AnalystContext): Promise<AnalystReply<Draft[]>>;
  /** Compare the target with renders of `current`, list the differences, and write n revised drafts. */
  revise(target: ImageRef, current: Draft, renders: RenderView[], n: number, ctx: AnalystContext): Promise<AnalystReply<Draft[]>>;
  /** Propose n short additions for one model only (they become prompt/<target> values). */
  refine(target: ImageRef, core: Draft, render: RenderView, n: number, ctx: AnalystContext): Promise<AnalystReply<string[]>>;
  /**
   * Optional judge: rate how closely each render reproduces the target, 0..1,
   * comparing composition, subject, colour, light and style. At most 9 renders
   * per call; the engine always includes the current best so that ratings in
   * one call are comparable.
   */
  judge?(target: ImageRef, renders: RenderView[], ctx: AnalystContext): Promise<AnalystReply<number[]>>;
  /** Most renders judge() accepts per call (default 9). */
  readonly judgeLimit?: number;
}

/** Scores renders against the target; higher is more similar, roughly 0..1. */
export interface Scorer {
  readonly id: string;
  /** Estimated USD per scored image (the target counts once). */
  readonly costHint: number;
  prepare(target: ImageRef, signal?: AbortSignal): Promise<void>;
  score(render: ImageRef, signal?: AbortSignal): Promise<number>;
}

/** RGBA pixels of a small thumbnail, decoded by the app (sharp, canvas). */
export interface Thumbnail { width: number; height: number; data: Uint8Array | Uint8ClampedArray }

export interface ScoredRender {
  model: string;
  seed: number;
  /** null when the render failed; its score is then 0. */
  result: RenderResult | null;
  error: string | null;
  semantic: number;
  pixel: number | null;
  /** The analyst's rating, 0..1, when judging is on. */
  judge: number | null;
  /** Semantic and pixel terms combined, before the judge. */
  metric: number;
  score: number;
}

export interface Candidate {
  id: string;
  generation: number;
  phase: 'evolve' | 'refine';
  draft: Draft;
  /** Per-model refinements: model id -> values for prompt/<target>. */
  refinements: Record<string, string[]>;
  renders: ScoredRender[];
  /** Mean score per model id (over seeds). */
  perModel: Record<string, number>;
  /** With one model, its score; with several, the mean of the mean and the worst
   *  per-model score, so the core description cannot sacrifice one model. */
  fitness: number;
}

export type StopReason = 'budget' | 'plateau' | 'generations' | 'time' | 'canceled' | 'error' | 'complete';

export interface Generation { index: number; phase: 'evolve' | 'refine'; candidates: string[]; best: string; bestFitness: number; notes: string; spent: number }

export interface SpendLine { kind: 'render' | 'analyst' | 'scorer'; label: string; usd: number }

export type EncodeEvent =
  | { type: 'phase'; phase: 'describe' | 'evolve' | 'refine' | 'done' }
  | { type: 'candidate'; candidate: Candidate }
  | { type: 'generation'; generation: Generation }
  | { type: 'spend'; spent: number; limit: number }
  | { type: 'log'; message: string };

export interface EncodeOptions {
  target: TargetImage;
  /** Renderer model ids to encode for, in the author's order of preference. */
  models: string[];
  renderer: Renderer;
  analyst: Analyst;
  scorer: Scorer;
  /** Decode a render into a small RGBA thumbnail; enables the colour/layout term. */
  thumbnail?: (render: RenderResult) => Promise<Thumbnail>;
  /** The target's thumbnail, same decoder. Required with `thumbnail`. */
  targetThumbnail?: Thumbnail;
  /** Weight of the colour/layout term when thumbnails are available (default 0.25). */
  pixelWeight?: number;
  /** Weight of the analyst's rating when it can judge (default 0.5; 0 turns judging off). */
  judgeWeight?: number;
  /** Hard spending limit in USD (estimates from cost hints). */
  budgetUSD: number;
  /** Candidates per generation (default 4). */
  population?: number;
  /** Generations in the evolve phase, including the first (default 5). */
  maxGenerations?: number;
  /** Stop evolving after this many generations without a gain of `minGain` (default 2). */
  plateau?: number;
  minGain?: number;
  /** Fixed seeds; every candidate renders once per seed and model (default [1]). */
  seeds?: number[];
  /** Run the per-model refinement phase (default: when there are two or more models). */
  refine?: boolean;
  /** Share of the budget held back for refinements (default 0.25). */
  refineShare?: number;
  /** Additions tried per lagging model (default 2). */
  refineTries?: number;
  timeLimitMs?: number;
  /** Renders run at once (default 4). */
  concurrency?: number;
  signal?: AbortSignal;
  onEvent?: (e: EncodeEvent) => void;
}

export interface EncodeResult {
  document: JaiDocument;
  /** Canonical text of the document. */
  text: string;
  best: Candidate;
  candidates: Candidate[];
  generations: Generation[];
  /** Final per-model scores, best first. */
  ranking: Array<{ model: string; score: number }>;
  spent: number;
  spend: SpendLine[];
  stopReason: StopReason;
  error: string | null;
  seconds: number;
}
