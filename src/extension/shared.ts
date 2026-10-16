// Settings, message types and helpers shared by the extension's contexts.

export type SiteMode = 'never' | 'on-request' | 'auto';

export interface Settings {
  enabledModels: string[];
  defaultModel: string;
  /** When a model has no negative prompt: fold avoid into the prompt (true) or omit and report. */
  foldAvoid: boolean;
  maxConcurrent: number;
  maxPerPage: number;
  maxPerDay: number;
  /** Elements smaller than this (CSS px, both sides) are never rendered automatically. */
  minDisplayedPx: number;
  defaultSiteMode: SiteMode;
  sites: Record<string, SiteMode>;
  /** Sites where the user accepted the provider disclosure. */
  disclosed: Record<string, boolean>;
  keepHistory: boolean;
}

export const DEFAULT_SETTINGS: Settings = {
  enabledModels: ['black-forest-labs/flux-schnell', 'black-forest-labs/flux-kontext-pro'],
  defaultModel: 'black-forest-labs/flux-schnell',
  foldAvoid: true,
  maxConcurrent: 2,
  maxPerPage: 10,
  maxPerDay: 50,
  minDisplayedPx: 48,
  defaultSiteMode: 'on-request',
  sites: {},
  disclosed: {},
  keepHistory: true,
};

export async function getSettings(): Promise<Settings> {
  const { settings } = await chrome.storage.local.get('settings');
  return { ...DEFAULT_SETTINGS, ...(settings as Partial<Settings> | undefined) };
}

export async function saveSettings(patch: Partial<Settings>): Promise<Settings> {
  const s = { ...(await getSettings()), ...patch };
  await chrome.storage.local.set({ settings: s });
  return s;
}

export function siteMode(s: Settings, origin: string): SiteMode { return s.sites[origin] ?? s.defaultSiteMode; }

/** Messages from content scripts and pages to the background. */
export type BgRequest =
  | { type: 'state'; origin: string }
  | { type: 'openOptions' }
  | { type: 'fetchJai'; url: string }
  | { type: 'acceptDisclosure'; origin: string }
  | { type: 'setSiteMode'; origin: string; mode: SiteMode }
  | { type: 'setToken'; token: string }
  | { type: 'clearToken' }
  | { type: 'tokenStatus' }
  | { type: 'testToken' }
  | { type: 'history' }
  | { type: 'clearHistory' }
  | { type: 'clearCache' };

export interface ModelChoice { id: string; label: string; imageInput: boolean; costHint: number; enabled: boolean; description: string }

export interface StateReply {
  hasToken: boolean;
  mode: SiteMode;
  disclosed: boolean;
  models: ModelChoice[];
  defaultModel: string;
  usage: { today: number; maxPerDay: number; maxPerPage: number };
  /** Elements smaller than this (CSS px, either side) are never rendered automatically. */
  minDisplayedPx: number;
}

/** Port message (content -> background) to render one document. */
export interface RenderPortRequest {
  type: 'render';
  canonicalJSON: string;
  origin: string;
  topSite: string;
  pageId: string;
  modelId: string | null;
  inputImage: { b64: string; mime: string } | null;
  governingRatio: number | null;
  target: { width: number; height: number } | null;
  automatic: boolean;
}

export type RenderPortReply =
  | { type: 'status'; status: string }
  | { type: 'done'; dataURL: string; mime: string; model: string; modelLabel: string; sentPrompt: string; sentNegative: string | null; sentInput: Record<string, unknown>; jobId: string | null; diagnostics: Array<{ code: string; key?: string; requested?: string; actual?: string }>; cached: boolean; seconds: number }
  | { type: 'error'; kind: string; message: string };

export const b64encode = (bytes: Uint8Array): string => {
  let s = '';
  for (let i = 0; i < bytes.length; i += 0x8000) s += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
  return btoa(s);
};
export const b64decode = (s: string): Uint8Array => Uint8Array.from(atob(s), c => c.charCodeAt(0));
export const today = (): string => new Date().toISOString().slice(0, 10);

export /** Read a response body, giving up (null) past `max` bytes without buffering the rest. */
async function readCapped(res: Response, max: number): Promise<Uint8Array | null> {
  const declared = Number(res.headers.get('content-length'));
  if (Number.isFinite(declared) && declared > max) { void res.body?.cancel(); return null; }
  if (!res.body) return new Uint8Array(0);
  const reader = res.body.getReader();
  const parts: Uint8Array[] = [];
  let total = 0;
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    total += value.length;
    if (total > max) { void reader.cancel(); return null; }
    parts.push(value);
  }
  const out = new Uint8Array(total);
  let at = 0;
  for (const p of parts) { out.set(p, at); at += p.length; }
  return out;
}

// ---------------------------------------------------------------- encoder port
// The encode page runs the encoder engine; the background, which alone holds
// the token, runs each Replicate prediction for it, for allow-listed models only.
export interface EncodeModelRef { id: string; version?: string }
export type EncodePortRequest =
  | { type: 'begin'; budgetUSD: number }
  | { type: 'predict'; id: number; model: EncodeModelRef; input: Record<string, unknown> }
  | { type: 'cancel'; id: number };
export type EncodePortReply =
  | { type: 'status'; id: number; status: string }
  | { type: 'result'; id: number; prediction: { id: string; status: string; output: unknown; metrics?: Record<string, unknown> } }
  | { type: 'error'; id: number; kind: string; message: string; jobId: string | null };
