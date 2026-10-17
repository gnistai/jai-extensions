// Typed wrapper over Tauri's invoke and events (studio/DESIGN.md sections 10
// and 11). Without window.__TAURI__ (a plain browser) it uses an in-memory mock
// (mock.ts), so the UI can be developed and screenshot without the app.
import type { Box, Draft, Generation, Measure, Observation } from '../../encoder/types.ts';
import type { Diag, ValidateResult } from '../host-entry.ts';
import { MockBackend } from './mock.ts';

export type { Box, Diag, Draft, Generation, Measure, Observation, ValidateResult };

interface TauriGlobal {
  core: { invoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T> };
  event: { listen<T>(event: string, handler: (e: { payload: T }) => void): Promise<() => void> };
}

declare global {
  interface Window { __TAURI__?: TauriGlobal }
}

// ---------------------------------------------------------------- replies (serde camelCase mirrors of the Rust types)

export type EngineKind = 'llm' | 'image';
export type EngineState = 'stopped' | 'starting' | 'ready' | 'stopping' | 'failed';

/** jai_engines::SchedulerStatus. */
export interface SchedulerStatus {
  loaded: EngineKind | null; state: EngineState; active: number; waitingLlm: number; waitingImage: number;
  mtp: boolean; lastError: string | null; swaps: number; sinceMs: number;
}

/** One engine's state as the UI shows it. Only one engine is loaded at a time. */
export type EngineView = 'off' | 'waiting' | EngineState;
export const ENGINE_WORDS: Record<EngineView, string> = { off: 'off', waiting: 'waiting', stopped: 'off', starting: 'loading', ready: 'ready', stopping: 'unloading', failed: 'failed' };
export function engineView(s: SchedulerStatus, kind: EngineKind): EngineView {
  if (s.loaded === kind) return s.state;
  return (kind === 'llm' ? s.waitingLlm : s.waitingImage) > 0 ? 'waiting' : 'off';
}

export type Backend = 'cuda' | 'vulkan' | 'cpu';
export type BackendPref = 'auto' | Backend;
/** `computeCap`: the CUDA compute capability, such as "8.9"; null when the driver does not report it. */
export interface GpuInfo { vendor: 'nvidia' | 'amd' | 'intel' | 'unknown'; name: string; vramTotalMb: number; vramUsedMb: number; driver: string; computeCap: string | null }
/**
 * `configProblem`: what is wrong with config.json, if anything (the defaults
 * or some of them are in use). `cudaNote`: why the automatic backend did not
 * pick CUDA on this NVIDIA GPU.
 */
export interface AppInfo {
  version: string; dataDir: string; os: 'windows' | 'linux'; backend: Backend; gpu: GpuInfo | null; vcRuntimeOk: boolean;
  configProblem: string | null; cudaNote: string | null;
}
/**
 * install_vc_runtime's answer. `declined`: the user said no (to Studio's question or Windows'
 * prompt); `finished`: the installer exited with `exitCode`; `started`: no process to wait for.
 */
export interface VcInstallReply { outcome: 'declined' | 'finished' | 'started'; exitCode: number | null }
/** `note`: why this app's server is not running, or that another JAI process (such as `jai serve`) runs it. */
export interface ServerInfo { url: string; token: string; host: string; port: number; running: boolean; note?: string }
export interface StatusReply { engines: SchedulerStatus; installed: boolean }

export type Role = 'llm-model' | 'llm-projector' | 'image-diffusion' | 'image-vae' | 'image-clip-l' | 'image-t5xxl'
  | 'llm-engine' | 'llm-runtime' | 'image-engine' | 'image-runtime';
export type InstallState = 'installed' | 'partial' | 'missing' | 'reusable';
/** `reuseLinks`, for a reusable file: true when it is hard-linked (no space taken), false when it is copied. */
export interface ComponentRow {
  id: string; role: Role; label: string; size: number; state: InstallState;
  partialBytes: number | null; reusablePath: string | null; reuseLinks: boolean | null; license: string; source: string;
}
export interface ModelsStatus { components: ComponentRow[]; totalBytes: number; neededBytes: number; diskFree: number | null }

export type DlState = 'queued' | 'reusing' | 'downloading' | 'verifying' | 'unpacking' | 'done' | 'failed' | 'canceled';
/** jai_core::download::Progress, one component. */
export interface Progress { id: string; state: DlState; bytes: number; total: number; speedBps: number; error: string | null }
export interface DownloadDone { ok: boolean; error?: string }

/** config.json (DESIGN.md section 3). */
export interface Config {
  version: number;
  server: { host: string; port: number; allowLan: boolean };
  idleUnloadMinutes: number;
  backend: BackendPref;
  llm: { ctx: number; gpuLayers: string; mtp: boolean; parallel: number; imageMaxTokens: number; extraArgs: string[] };
  image: { steps: number; extraArgs: string[] };
  app: { closeToTray: boolean; startAtLogin: boolean; fileAssociation: boolean };
}

export interface DocumentInfo { name: string; path: string; validate: ValidateResult }
export interface ViewerRender {
  dataUrl: string; width: number; height: number; seed: number; sentPrompt: string; description: string;
  documentText: string; diagnostics: Diag[]; cached: boolean;
}
export interface PathReply { path: string }
export interface ImagePreview { dataUrl: string; width: number; height: number }

export type Thinking = 'off' | 'propose' | 'all';
/** encode_start options; the UI's defaults are the CLI's. */
export interface EncodeSettings { generations: number; population: number; seeds: number[]; restarts: number; goal: number; scale: number; thinking: Thinking }
export interface EncodeStarted { jobId: string; dir: string }

// ---------------------------------------------------------------- encode events

/** A render as the host reports it: the bytes stay on disk; `thumb` is a JPEG data URL at most 384 px. */
export interface RenderRef { model: string; seed: number; file: string | null; error: string | null; measure: Measure | null; score: number; thumb?: string | null }
export interface CandidateRef {
  id: string; lineage: number; generation: number; phase: 'evolve' | 'refine'; draft: Draft;
  refinements: Record<string, string[]>; renders: RenderRef[]; perModel: Record<string, number>; fitness: number;
}
/** The engine's EncodeEvent (src/encoder/types.ts) with renders replaced by references. */
export type EncodeEvent =
  | { type: 'phase'; phase: 'inspect' | 'describe' | 'evolve' | 'refine' | 'done'; lineage?: number }
  | { type: 'target'; observation: Observation }
  | { type: 'candidate'; candidate: CandidateRef }
  | { type: 'generation'; generation: Generation }
  | { type: 'spend'; spent: number; limit: number }
  | { type: 'log'; message: string };
export interface EncodeEventMessage { jobId: string; event: EncodeEvent }
/** `error`: why it failed, or, with a result (`ok`), why it stopped when stopReason is 'error'. */
export interface EncodeDone { jobId: string; ok: boolean; text?: string; jaiPath?: string; reportDir: string; error?: string; stopReason?: string }
export interface OpenPath { path: string }

// ---------------------------------------------------------------- transport

let mock: MockBackend | null = null;

/** Tauri serves the UI from tauri://localhost (Linux) or http(s)://tauri.localhost (Windows). */
const appOrigin = (): boolean => location.protocol === 'tauri:' || location.hostname === 'tauri.localhost';

/** The mock, for a plain browser only: on the app's origin a missing bridge is an error, so the app never shows made-up data. */
function mockBackend(): MockBackend {
  if (appOrigin()) throw new Error('The JAI Studio bridge (window.__TAURI__) is missing, so the UI cannot reach the app.');
  return (mock ??= new MockBackend());
}

/** True inside the JAI Studio window. */
export function hasTauri(): boolean {
  return window.__TAURI__ !== undefined;
}

/** True when commands go to the in-memory mock (a plain browser). */
export function usesMock(): boolean {
  return !hasTauri() && !appOrigin();
}

/** Calls a Tauri command: snake_case name, camelCase arguments. Rejects with an Error carrying the command's message. */
export async function invoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  const tauri = window.__TAURI__;
  try {
    return tauri ? await tauri.core.invoke<T>(cmd, args) : ((await mockBackend().invoke(cmd, args ?? {})) as T);
  } catch (e) {
    throw e instanceof Error ? e : new Error(typeof e === 'string' ? e : JSON.stringify(e));
  }
}

/** Subscribes to a Tauri event; resolves to the unsubscribe function. */
export async function listen<T>(event: string, handler: (payload: T) => void): Promise<() => void> {
  const tauri = window.__TAURI__;
  if (tauri) return tauri.event.listen<T>(event, e => handler(e.payload));
  return mockBackend().listen(event, p => handler(p as T));
}

// ---------------------------------------------------------------- commands (DESIGN.md section 10)

export const api = {
  appInfo: (): Promise<AppInfo> => invoke('app_info'),
  serverInfo: (): Promise<ServerInfo> => invoke('server_info'),
  regenerateToken: (): Promise<{ token: string }> => invoke('regenerate_token'),
  status: (): Promise<StatusReply> => invoke('status'),
  modelsStatus: (): Promise<ModelsStatus> => invoke('models_status'),
  /** Progress arrives as download-progress events, the end as download-done. */
  downloadStart: (reuse: boolean): Promise<void> => invoke('download_start', { reuse }),
  downloadCancel: (): Promise<void> => invoke('download_cancel'),
  unloadModels: (): Promise<void> => invoke('unload_models'),
  engineLogs: (kind: EngineKind): Promise<string[]> => invoke('engine_logs', { kind }),
  settingsGet: (): Promise<Config> => invoke('settings_get'),
  settingsSet: (config: Config): Promise<Config> => invoke('settings_set', { config }),
  /** Waits for the installer; how it ended. */
  installVcRuntime: (): Promise<VcInstallReply> => invoke('install_vc_runtime'),
  readDocument: (path: string): Promise<DocumentInfo> => invoke('read_document', { path }),
  viewerRender: (args: { path?: string; text?: string; seed?: string }): Promise<ViewerRender> => invoke('viewer_render', args),
  /** Through a save dialog; null when the user cancels. */
  saveFile: (args: { dataUrl?: string; text?: string; bytesB64?: string; suggestedName: string }): Promise<PathReply | null> => invoke('save_file', args),
  openDialog: (kind: 'jai' | 'image'): Promise<PathReply | null> => invoke('open_dialog', { kind }),
  imagePreview: (path: string): Promise<ImagePreview> => invoke('image_preview', { path }),
  /** Events arrive as encode-event, the end as encode-done. */
  encodeStart: (path: string, options: EncodeSettings): Promise<EncodeStarted> => invoke('encode_start', { path, options }),
  encodeCancel: (jobId: string): Promise<void> => invoke('encode_cancel', { jobId }),
  reveal: (path: string): Promise<void> => invoke('reveal', { path }),
  /** The page listens for open-path now: the app sends the documents that waited for it. */
  uiReady: (): Promise<void> => invoke('ui_ready'),
  /** The licences of the open-source software in JAI Studio, as `jai licenses` prints them. */
  thirdPartyNotices: (): Promise<string> => invoke('third_party_notices'),
};

export const events = {
  downloadProgress: (fn: (p: Progress) => void): Promise<() => void> => listen('download-progress', fn),
  downloadDone: (fn: (d: DownloadDone) => void): Promise<() => void> => listen('download-done', fn),
  encodeEvent: (fn: (m: EncodeEventMessage) => void): Promise<() => void> => listen('encode-event', fn),
  encodeDone: (fn: (d: EncodeDone) => void): Promise<() => void> => listen('encode-done', fn),
  /** A document from the command line, a second launch or the file association. */
  openPath: (fn: (p: OpenPath) => void): Promise<() => void> => listen('open-path', fn),
};

/**
 * Files dragged onto the window. Tauri's webview reports them through its own
 * drag-drop events, with paths (HTML5 drops carry no paths). In a browser this
 * does nothing; main.ts uses HTML5 drag and drop there.
 */
export async function onFileDrop(h: { enter(): void; leave(): void; drop(paths: string[]): void }): Promise<void> {
  if (!hasTauri()) return;
  await listen<unknown>('tauri://drag-enter', () => h.enter());
  await listen<unknown>('tauri://drag-leave', () => h.leave());
  await listen<{ paths?: string[] }>('tauri://drag-drop', p => h.drop(p?.paths ?? []));
}
