// Encoder engine types. The engine is UI-free and environment-neutral: apps
// supply the target image (and, optionally, a thumbnail decoder) and the
// providers; the engine supplies the loop and budgeting.
//
// The loop: inspect the target once; render every candidate with every target
// model and two seeds; inspect each render the same way; compute the fitness
// from the measured differences (never from a judged score); hand the best
// past candidates with their measured errors to an optimiser that writes the
// next candidates (OPRO-style). Spec: "Encoding images" (informative).
import type { JaiDocument } from '../jai/index.ts';
import type { Renderer, RenderResult } from '../renderers/types.ts';

/** An image a provider can fetch: an https: URL or a data: URL. */
export interface ImageRef { url: string }

/** The original image. `url` should point at a downscaled copy (about 1 megapixel or less). */
export interface TargetImage extends ImageRef { width: number; height: number }

/** A candidate core description (spec: prompt, style, avoid). */
export interface Draft { prompt: string; style: string[]; avoid: string[] }

/** A box as frame fractions, 0 at top and left. */
export interface Box { ymin: number; xmin: number; ymax: number; xmax: number }

/** How readable a person's face is: features readable, visible but too small for detail, or turned away or covered. */
export type FaceVisibility = 'clear' | 'small' | 'hidden';

/**
 * One person as an inspection reads them: where they are, how readable the
 * face is, and their visible traits in the closed vocabularies of TRAITS
 * (people.ts), null where a trait cannot be seen. Appearance only, never
 * identity: the aim is the same kind of person, not the same face.
 */
export interface Person {
  box: Box;
  face: FaceVisibility | null;
  /** Trait key -> one of that trait's values, or null when not readable. */
  traits: Record<string, string | null>;
}

/** What an inspection of one image measured. */
export interface Observation {
  /** Main subjects or regions other than people; for a render, in the target's order and naming, box null when absent. */
  subjects: Array<{ name: string; box: Box | null }>;
  /** Prominent people, largest first. */
  people: Person[];
  /** The people too small or too many to list: 'none', 'few', 'some', 'many', or null when not read. */
  crowd: string | null;
  /** Horizon height as a frame fraction from the top, or null when there is none. */
  horizon: number | null;
  /** Shape of the horizon line: 'flat', 'curved', or '' when unknown or absent. */
  horizonShape: string;
  camera: string;
  lighting: string;
  medium: string;
}

export interface SubjectMeasure {
  name: string;
  present: boolean;
  /** Centre offset render − target, frame fractions (positive: right, down). */
  dx: number; dy: number;
  /** ln(render size / target size) for width and height. */
  sw: number; sh: number;
  iou: number;
  error: number;
}

/** One target person compared with the render person matched to them (people.ts). */
export interface PersonMeasure {
  /** The target person's number, 1 for the first listed. */
  n: number;
  /** The target person in words, for example "young woman at left". */
  label: string;
  /** The matched render person's box, or null when no render person matched. */
  box: Box | null;
  /** Centre offset render − target, frame fractions, and ln(render size / target size); 0 when missing. */
  dx: number; dy: number; sw: number; sh: number;
  /** Traits that differ: the render's value and the target's. */
  differs: Array<{ trait: string; render: string; target: string }>;
  /** 0..1: layout, traits, and the person's error: 1 − (1 − 0.6 layout)(1 − traits), so 1 when missing or of another kind. */
  layoutError: number; traitError: number; error: number;
}

/** The measured difference of one render from the target. */
export interface Measure {
  fitness: number;
  subjects: SubjectMeasure[];
  /** The target's people in order. */
  people: PersonMeasure[];
  /** Prominent people in the render that match no one in the target. */
  extraPeople: number;
  /** Render horizon − target horizon (positive: lower), null when not measurable. */
  horizon: number | null;
  /** Colour/layout similarity of thumbnails, 0..1, null without thumbnails. */
  palette: number | null;
  /** Mean CIE lightness difference render − target, null without thumbnails. */
  lightness: number | null;
  /** The differences in words, with numbers, for the optimiser. */
  feedback: string;
}

export interface AnalystContext {
  /** Labels of the target models. */
  models: string[];
  /** Longest description, in words, that every target model reads in full (shorter when a model has a 77-token window). */
  maxWords?: number;
  signal?: AbortSignal;
}

export interface AnalystReply<T> {
  value: T;
  /** Optimiser notes for the next round, or a short remark. */
  notes: string;
  /** Cost in USD of this call (actual when the provider reports tokens). */
  cost: number;
}

/** One evaluated candidate as the optimiser sees it. */
export interface HistoryEntry {
  draft: Draft;
  /** For refinement rounds: the addition tried. */
  addition?: string;
  fitness: number;
  /** Measured feedback per model label. */
  feedback: Record<string, string>;
}

export type ProposeScope = { kind: 'core' } | { kind: 'refine'; model: string; core: Draft };

export interface ProposeContext extends AnalystContext {
  targetObservation: Observation;
  /** Best first. */
  history: HistoryEntry[];
  /** The optimiser's own notes from earlier rounds. */
  notes: string;
  n: number;
  scope: ProposeScope;
}

/** A vision-language model that writes, measures and improves descriptions. */
export interface Analyst {
  readonly id: string;
  /** Most one call can cost, held before calling. */
  readonly costHint: number;
  /** Write n diverse, faithful first drafts. */
  describe(target: ImageRef, n: number, ctx: AnalystContext): Promise<AnalystReply<Draft[]>>;
  /** Measure an image: open discovery for the target, or find the named subjects in a render. */
  inspect(image: ImageRef, subjects: string[] | null, ctx: AnalystContext & { careful?: boolean }): Promise<AnalystReply<Observation>>;
  /** Write n new candidates (drafts, or additions for a refinement scope) from the measured history. */
  propose(target: ImageRef, ctx: ProposeContext): Promise<AnalystReply<Draft[] | string[]>>;
}

/** RGBA pixels of a small thumbnail, decoded by the app (sharp, canvas). */
export interface Thumbnail { width: number; height: number; data: Uint8Array | Uint8ClampedArray }

export interface ScoredRender {
  model: string;
  seed: number;
  /** null when the render failed; its score is then 0. */
  result: RenderResult | null;
  error: string | null;
  observation: Observation | null;
  measure: Measure | null;
  score: number;
}

export interface Candidate {
  id: string;
  lineage: number;
  generation: number;
  phase: 'evolve' | 'refine';
  draft: Draft;
  /** Per-model refinements: model id -> values for prompt/<target>. */
  refinements: Record<string, string[]>;
  renders: ScoredRender[];
  /** Mean score per model id (over seeds). */
  perModel: Record<string, number>;
  /** With one model, its score; with several, the mean of the mean and the worst per-model score. */
  fitness: number;
}

export type StopReason = 'goal' | 'budget' | 'plateau' | 'generations' | 'time' | 'canceled' | 'error' | 'complete';

export interface Generation { lineage: number; index: number; phase: 'evolve' | 'refine'; candidates: string[]; best: string; bestFitness: number; notes: string; spent: number }

export interface SpendLine { kind: 'render' | 'analyst' | 'inspect'; label: string; usd: number }

export type EncodeEvent =
  | { type: 'phase'; phase: 'inspect' | 'describe' | 'evolve' | 'refine' | 'done'; lineage?: number }
  | { type: 'target'; observation: Observation }
  | { type: 'candidate'; candidate: Candidate }
  | { type: 'generation'; generation: Generation }
  | { type: 'spend'; spent: number; limit: number }
  | { type: 'log'; message: string };

export interface EncodeOptions {
  target: TargetImage;
  /** Renderer model ids to encode for, in the author's order of preference. */
  models: string[];
  renderer: Renderer;
  /** Describes the target and inspects images. */
  analyst: Analyst;
  /** Writes new candidates from the history (default: the analyst). A stronger model can go here. */
  optimiser?: Analyst;
  /** Decode a render into a small RGBA thumbnail; enables the palette and lightness terms. */
  thumbnail?: (render: RenderResult) => Promise<Thumbnail>;
  /** The target's thumbnail, same decoder. Required with `thumbnail`. */
  targetThumbnail?: Thumbnail;
  /** Hard spending limit in USD. */
  budgetUSD: number;
  /** Candidates per generation (default 4). */
  population?: number;
  /** Generations per lineage, including the first (default 8). */
  maxGenerations?: number;
  /** Stop a lineage after this many generations without a gain of `minGain` (default 4). */
  plateau?: number;
  minGain?: number;
  /** Stop as soon as the best fitness reaches this (default 0.9). */
  goal?: number;
  /** Fixed seeds; every candidate renders once per seed and model (default [1, 2]). */
  seeds?: number[];
  /** Independent lineages sharing the budget; the best by fitness wins (default 1). */
  restarts?: number;
  /** Past candidates shown to the optimiser (default 8). */
  historySize?: number;
  /** Run the per-model refinement phase (default: when there are two or more models). */
  refine?: boolean;
  /** Share of the budget held back for refinements (default 0.2). */
  refineShare?: number;
  /** Additions tried per lagging model and round (default 3). */
  refineTries?: number;
  timeLimitMs?: number;
  /**
   * Renders run at most this many at once, and so do inspections, each kind in
   * its own slots (default 4). Against a local server that queues each kind for
   * its own engine, a few of each keep both engines busy while every request
   * waits only for those ahead of it, not for a whole generation.
   */
  concurrency?: number;
  signal?: AbortSignal;
  onEvent?: (e: EncodeEvent) => void;
}

export interface EncodeResult {
  document: JaiDocument;
  /** Canonical text of the document. */
  text: string;
  best: Candidate;
  target: Observation;
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
