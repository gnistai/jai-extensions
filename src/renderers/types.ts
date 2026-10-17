// Provider-independent renderer interface. A renderer turns one JAI document
// into image bytes and reports, with the spec's renderer diagnostic codes,
// every standard key it did not honour as written.

/**
 * The interpreter codes a viewer reports with a render: every one except
 * `mode-degraded`, which the renderer decides from the input image it actually
 * has (with none, it reports `original-not-readable` instead).
 */
export const INTERPRETER_CODES: ReadonlySet<string> = new Set(['repeated-single', 'invalid-value', 'adjusted-value', 'ignored-value', 'invalid-image', 'unsupported-preview', 'no-matching-model', 'substituted-model', 'version-ignored']);

export interface RenderDiagnostic {
  code: string;
  key?: string;
  requested?: string;
  actual?: string;
}

/** What a renderer needs from the interpreted document (see jai interpret()). */
export interface RenderIntent {
  description: string;
  style: string[];
  avoid: string[];
  /** width/height of the requested ratio, or null */
  aspect: { width: number; height: number } | null;
  mode: 'generate' | 'transform' | 'keep';
  strength: number | null;
  seed: string | null;
}

export interface InputImage {
  bytes: Uint8Array;
  mime: string;
}

export interface RenderRequest {
  intent: RenderIntent;
  modelId: string;
  /** Pixels the page itself could read (spec: Permitted inputs). */
  inputImage: InputImage | null;
  /**
   * The ratio to render at, in the order of the spec's "Size and aspect": the
   * element's governing ratio (an author-sized layout box, else the original
   * image), which overrides `aspect`; else `intent.aspect`; else this
   * `previewRatio`; else the model's default.
   */
  governingRatio: number | null;
  /** Width/height of the document's `preview` (a raster data: URL, never fetched; see previewRatio()), or null. */
  previewRatio: number | null;
  /** Desired output size in device pixels (layout box x DPR), advisory. */
  target: { width: number; height: number } | null;
  signal?: AbortSignal;
  onStatus?: (status: string) => void;
}

export interface RenderResult {
  bytes: Uint8Array;
  mime: string;
  model: string;
  /** The exact text sent to the model (spec: renderers MUST be able to show it). */
  sentPrompt: string;
  sentNegative: string | null;
  /** The full provider input with image bytes elided. */
  sentInput: Record<string, unknown>;
  jobId: string | null;
  diagnostics: RenderDiagnostic[];
  /** Where the provider published the output, if it did (short-lived; never shown to pages). */
  outputURL?: string | null;
  /** Provider-reported compute time in seconds, if known. */
  predictSeconds?: number | null;
}

export interface ModelInfo {
  id: string;
  label: string;
  family: string;
  aliases: string[];
  /** Accepts an input image (transform). */
  imageInput: boolean;
  /** Has a native negative prompt input. */
  negativePrompt: boolean;
  /** Short prompt window (CLIP-class): put style terms first. */
  shortWindow: boolean;
  /** Rough price per image in USD, for the cost display only. */
  costHint: number;
  description: string;
}

export interface Renderer {
  readonly id: string;
  readonly label: string;
  models(): ModelInfo[];
  render(req: RenderRequest): Promise<RenderResult>;
}

export class RenderError extends Error {
  constructor(message: string, readonly kind: 'auth' | 'refused' | 'provider' | 'network' | 'timeout' | 'canceled' | 'config', readonly jobId: string | null = null) {
    super(message);
  }
}
