// In-memory stand-in for the JAI Studio backend, used when the UI runs in a
// plain browser (no window.__TAURI__). Every command of DESIGN.md section 10
// answers with plausible data: downloads progress on a sped-up clock, the
// engines load and swap, the viewer renders a painted placeholder, and an
// encode run emits realistic events over about 20 seconds, measured with the
// encoder's own metrics. Query parameters: ?installed=1 starts with everything
// installed, ?vc=1 reports the VC++ runtime as present, ?server=down reports the
// server as not running (with the reason), ?speed=N runs N times faster.
import { measure } from '../../encoder/metrics.ts';
import type { Box, Draft, Observation, Thumbnail } from '../../encoder/types.ts';
import type {
  AppInfo, CandidateRef, ComponentRow, Config, DocumentInfo, EncodeDone, EncodeEvent, EncodeSettings, EncodeStarted, EngineKind,
  EngineState, ImagePreview, ModelsStatus, Progress, RenderRef, SchedulerStatus, ServerInfo, StatusReply, VcInstallReply, ViewerRender,
} from './api.ts';
import { ENCODE_LIMITS } from './rules.ts';

const params = new URLSearchParams(location.search);
const SPEED = Math.max(0.1, Number(params.get('speed')) || 1);
const sleep = (ms: number): Promise<void> => new Promise(r => setTimeout(r, ms / SPEED));
const DATA_DIR = 'C:\\Users\\you\\AppData\\Local\\JAI';
const FLUX = 'black-forest-labs/flux-schnell';

// ---------------------------------------------------------------- catalog (Windows + CUDA selection of studio/catalog.json)

type Spec = Pick<ComponentRow, 'id' | 'role' | 'label' | 'size' | 'license' | 'source'> & { archive?: true };
const QWEN = 'unsloth/Qwen3.8-27B-GGUF@4ca720788d1e', LLAMA = 'ggml-org/llama.cpp b11669', SD = 'leejet/stable-diffusion.cpp master-970-2f016e8';
const CUDA_EULA = 'NVIDIA CUDA EULA (redistributable runtime)';
const CATALOG: Spec[] = [
  { id: 'llm-model', role: 'llm-model', label: 'Qwen3.8 27B (UD-IQ3_XXS)', size: 10934860704, license: 'Apache-2.0', source: QWEN },
  { id: 'llm-projector', role: 'llm-projector', label: 'Qwen3.8 27B vision projector (F16)', size: 927607488, license: 'Apache-2.0', source: QWEN },
  { id: 'image-diffusion', role: 'image-diffusion', label: 'FLUX.1 schnell (Q8_0)', size: 12801721248, license: 'Apache-2.0', source: 'leejet/FLUX.1-schnell-gguf@c7f665ddaf9f' },
  { id: 'image-vae', role: 'image-vae', label: 'FLUX autoencoder', size: 335304388, license: 'Apache-2.0', source: 'second-state/FLUX.1-schnell-GGUF@8c45a2ba25e2' },
  { id: 'image-clip-l', role: 'image-clip-l', label: 'CLIP-L text encoder', size: 246144152, license: 'MIT', source: 'comfyanonymous/flux_text_encoders@6af2a98e3f61' },
  { id: 'image-t5xxl', role: 'image-t5xxl', label: 'T5-XXL text encoder (Q8_0)', size: 5061584064, license: 'Apache-2.0', source: 'city96/t5-v1_1-xxl-encoder-gguf@005a6ea51a7d' },
  { id: 'llama-win-cuda', role: 'llm-engine', label: 'llama.cpp b11669 (CUDA 13.4)', size: 154582227, license: 'MIT', source: LLAMA, archive: true },
  { id: 'llama-win-cuda-rt', role: 'llm-runtime', label: 'CUDA 13.4 runtime for llama.cpp', size: 423535356, license: CUDA_EULA, source: LLAMA, archive: true },
  { id: 'sd-win-cuda', role: 'image-engine', label: 'stable-diffusion.cpp 2f016e8 (CUDA 12.8)', size: 338054307, license: 'MIT', source: SD, archive: true },
  { id: 'sd-win-cuda-rt', role: 'image-runtime', label: 'CUDA 12.8 runtime for stable-diffusion.cpp', size: 563452046, license: CUDA_EULA, source: SD, archive: true },
];
type RowState = Pick<ComponentRow, 'state' | 'partialBytes' | 'reusablePath' | 'reuseLinks'>;
const FRESH: Record<string, RowState> = {
  'llm-model': { state: 'reusable', partialBytes: null, reusablePath: 'C:\\Users\\you\\.lmstudio\\models\\unsloth\\Qwen3.8-27B-GGUF\\Qwen3.8-27B-UD-IQ3_XXS.gguf', reuseLinks: true },
  'image-t5xxl': { state: 'partial', partialBytes: 1946157056, reusablePath: null, reuseLinks: null },
};
const INSTALLED: RowState = { state: 'installed', partialBytes: null, reusablePath: null, reuseLinks: null };

const defaultConfig = (): Config => ({
  version: 1, server: { host: '127.0.0.1', port: 5240, allowLan: false }, idleUnloadMinutes: 15, backend: 'auto',
  llm: { ctx: 20480, gpuLayers: 'all', mtp: true, parallel: 1, imageMaxTokens: 1536, extraArgs: [] },
  image: { steps: 4, extraArgs: [] }, app: { closeToTray: true, startAtLogin: false, fileAssociation: false },
});

function newToken(): string {
  const b = crypto.getRandomValues(new Uint8Array(32));
  return 'jai_' + btoa(String.fromCharCode(...b)).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
}

const ENGINE_START: Record<EngineKind, string[]> = {
  llm: [
    'build: 11669 (5e1c2a9) with clang version 19.1.5 for x86_64-pc-windows-msvc',
    'system info: n_threads = 16, n_threads_batch = 16, total_threads = 32',
    `llama_model_loader: loaded meta data with 46 key-value pairs and 851 tensors from ${DATA_DIR}\\models\\Qwen3.8-27B-UD-IQ3_XXS.gguf`,
    'load_tensors: offloaded 65/65 layers to GPU',
    'load_tensors:        CUDA0 model buffer size = 10187.64 MiB',
    'llama_context: n_ctx = 20480, n_batch = 2048, flash_attn = enabled',
    'llama_kv_cache:      CUDA0 KV buffer size =  1280.00 MiB',
    `clip_model_loader: loaded ${DATA_DIR}\\models\\mmproj-F16.gguf (image_max_tokens = 1536)`,
  ],
  image: [
    `[INFO ] stable-diffusion.cpp:229  - loading diffusion model from '${DATA_DIR}\\models\\flux1-schnell-q8_0.gguf'`,
    `[INFO ] stable-diffusion.cpp:262  - loading t5xxl from '${DATA_DIR}\\models\\t5-v1_1-xxl-encoder-Q8_0.gguf'`,
    `[INFO ] stable-diffusion.cpp:276  - loading vae from '${DATA_DIR}\\models\\ae.safetensors'`,
    '[INFO ] model.cpp:2142 - total params memory size = 18402.11MB (VRAM 13137.63MB, RAM 5264.48MB)',
    '[INFO ] stable-diffusion.cpp:576  - running in Flux FLOW mode',
  ],
};
const ENGINE_READY: Record<EngineKind, string[]> = {
  llm: ['srv    load_model: speculative decoding: draft-mtp', 'main: server is listening on http://127.0.0.1:53817 - starting the main loop', 'srv  update_slots: all slots are idle'],
  image: ['[INFO ] main.cpp:871  - listening on: 127.0.0.1:54121'],
};

// ---------------------------------------------------------------- painted placeholder scenes

/** A lighthouse at sunset; the encoder mock jitters these numbers and measures the result. */
interface Scene { horizon: number; tower: number; towerH: number; island: number; boatX: number; boat: boolean; sunX: number; hue: number; light: number; medium: number }
const TARGET: Scene = { horizon: 0.58, tower: 0.61, towerH: 0.44, island: 1, boatX: 0.17, boat: true, sunX: 0.3, hue: 0, light: 0, medium: 0 };
const MEDIA = ['watercolour painting', 'digital painting', 'photograph'];
const clamp = (v: number, lo: number, hi: number): number => Math.min(hi, Math.max(lo, v));

function mulberry32(seed: number): () => number {
  let a = seed >>> 0;
  return () => {
    a = (a + 0x6d2b79f5) >>> 0;
    let t = a;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

function jitter(b: Scene, spread: number, rand: () => number): Scene {
  const n = (): number => rand() * 2 - 1;
  return {
    horizon: clamp(b.horizon + n() * 0.12 * spread, 0.3, 0.8),
    tower: clamp(b.tower + n() * 0.22 * spread, 0.15, 0.85),
    towerH: clamp(b.towerH * Math.exp(n() * 0.45 * spread), 0.15, 0.5),
    island: clamp(b.island * Math.exp(n() * 0.35 * spread), 0.5, 1.6),
    boatX: clamp(b.boatX + n() * 0.2 * spread, 0.06, 0.4),
    boat: rand() < 0.3 * spread ? !b.boat : b.boat,
    sunX: clamp(b.sunX + n() * 0.25 * spread, 0.1, 0.9),
    hue: b.hue + n() * 45 * spread,
    light: clamp(b.light + n() * 14 * spread, -14, 14),
    medium: rand() < 0.35 * spread ? Math.floor(rand() * 3) : b.medium,
  };
}

/** Moves a scene part of the way to the target, as the optimiser's next drafts would. */
function toward(a: Scene, t: number): Scene {
  const mix = (x: number, y: number): number => x + (y - x) * t;
  return {
    horizon: mix(a.horizon, TARGET.horizon), tower: mix(a.tower, TARGET.tower), towerH: mix(a.towerH, TARGET.towerH), island: mix(a.island, TARGET.island),
    boatX: mix(a.boatX, TARGET.boatX), boat: a.boat || t > 0.3, sunX: mix(a.sunX, TARGET.sunX), hue: mix(a.hue, 0), light: mix(a.light, 0), medium: t > 0.3 ? 0 : a.medium,
  };
}

function paint(w: number, h: number, s: Scene): HTMLCanvasElement {
  const c = document.createElement('canvas');
  c.width = w; c.height = h;
  const g = c.getContext('2d');
  if (!g) return c;
  const X = (f: number): number => f * w, Y = (f: number): number => f * h;
  const hy = Y(s.horizon), hue = s.hue, L = s.light;
  const sky = g.createLinearGradient(0, 0, 0, hy);
  sky.addColorStop(0, `hsl(${228 + hue} 42% ${20 + L}%)`);
  sky.addColorStop(0.5, `hsl(${300 + hue} 30% ${40 + L}%)`);
  sky.addColorStop(1, `hsl(${26 + hue} 88% ${63 + L}%)`);
  g.fillStyle = sky; g.fillRect(0, 0, w, hy + 1);
  const sx = X(s.sunX), sy = hy - Y(0.05), r = Math.min(w, h) * 0.06;
  const sun = g.createRadialGradient(sx, sy, 0, sx, sy, r * 4);
  sun.addColorStop(0, 'rgba(255, 240, 200, 1)'); sun.addColorStop(0.22, 'rgba(255, 205, 130, 0.85)'); sun.addColorStop(1, 'rgba(255, 150, 80, 0)');
  g.fillStyle = sun; g.fillRect(0, 0, w, hy + 1);
  const sea = g.createLinearGradient(0, hy, 0, h);
  sea.addColorStop(0, `hsl(${18 + hue} 50% ${48 + L}%)`); sea.addColorStop(0.2, `hsl(${212 + hue} 32% ${30 + L}%)`); sea.addColorStop(1, `hsl(${222 + hue} 42% ${13 + L}%)`);
  g.fillStyle = sea; g.fillRect(0, hy, w, h - hy);
  g.fillStyle = 'rgba(255, 215, 160, 0.35)';
  for (let i = 0; i < 9; i++) { const y = hy + (i + 1) * (h - hy) / 11, half = X(0.05 - i * 0.004); g.fillRect(sx - half, y, half * 2, Math.max(1, Y(0.004))); }
  const tx = X(s.tower);
  g.fillStyle = `hsl(${20 + hue} 18% ${11 + L / 2}%)`;
  g.beginPath(); g.moveTo(tx - X(0.2 * s.island), hy + Y(0.11));
  g.quadraticCurveTo(tx - X(0.09 * s.island), hy - Y(0.08), tx, hy - Y(0.06));
  g.quadraticCurveTo(tx + X(0.13 * s.island), hy - Y(0.07), tx + X(0.25 * s.island), hy + Y(0.11));
  g.closePath(); g.fill();
  const base = hy - Y(0.04), top = base - Y(s.towerH), bw = X(0.025), tw = X(0.016);
  g.save(); g.beginPath(); g.moveTo(tx - bw, base); g.lineTo(tx + bw, base); g.lineTo(tx + tw, top); g.lineTo(tx - tw, top); g.closePath(); g.clip();
  for (let i = 0; i < 6; i++) { g.fillStyle = i % 2 ? '#f3ece0' : '#c3192f'; g.fillRect(tx - bw, top + (base - top) * i / 6, bw * 2, (base - top) / 6 + 1); }
  g.restore();
  g.fillStyle = '#26221d'; g.fillRect(tx - tw * 1.3, top - Y(0.012), tw * 2.6, Y(0.012));
  g.fillStyle = '#ffe7a3'; g.fillRect(tx - tw, top - Y(0.04), tw * 2, Y(0.028));
  g.fillStyle = '#26221d'; g.beginPath(); g.moveTo(tx - tw * 1.2, top - Y(0.04)); g.lineTo(tx, top - Y(0.055)); g.lineTo(tx + tw * 1.2, top - Y(0.04)); g.fill();
  g.fillStyle = 'rgba(255, 236, 170, 0.18)'; g.beginPath(); g.moveTo(tx, top - Y(0.026)); g.lineTo(tx - X(0.3), top - Y(0.1)); g.lineTo(tx - X(0.3), top + Y(0.05)); g.closePath(); g.fill();
  if (s.boat) {
    const bx = X(s.boatX), by = hy + Y(0.045);
    g.fillStyle = '#2a2420'; g.beginPath(); g.moveTo(bx - X(0.035), by); g.lineTo(bx + X(0.035), by); g.lineTo(bx + X(0.025), by + Y(0.03)); g.lineTo(bx - X(0.025), by + Y(0.03)); g.closePath(); g.fill();
    g.fillStyle = '#efe6d6'; g.beginPath(); g.moveTo(bx, by - Y(0.085)); g.lineTo(bx, by - Y(0.004)); g.lineTo(bx + X(0.03), by - Y(0.004)); g.closePath(); g.fill();
  }
  if (s.medium === 0) { g.fillStyle = 'rgba(255, 250, 240, 0.07)'; for (let i = 0; i < 40; i++) g.fillRect(X(((i * 37) % 100) / 100), Y(((i * 53) % 100) / 100), X(0.08), Y(0.05)); }
  return c;
}

const box = (ymin: number, xmin: number, ymax: number, xmax: number): Box => ({ ymin: clamp(ymin, 0, 1), xmin: clamp(xmin, 0, 1), ymax: clamp(ymax, 0, 1), xmax: clamp(xmax, 0, 1) });

/** What an inspection would measure in a painted scene. */
function observe(s: Scene): Observation {
  const hy = s.horizon, base = hy - 0.04, top = base - s.towerH;
  return {
    subjects: [
      { name: 'lighthouse', box: box(top - 0.055, s.tower - 0.03, base, s.tower + 0.03) },
      { name: 'rocky island', box: box(hy - 0.08, s.tower - 0.2 * s.island, hy + 0.11, s.tower + 0.25 * s.island) },
      { name: 'sailing boat', box: s.boat ? box(hy - 0.04, s.boatX - 0.035, hy + 0.075, s.boatX + 0.035) : null },
    ],
    horizon: hy, horizonShape: 'flat', camera: 'eye level, wide shot',
    lighting: s.hue > 30 ? 'cool dusk light' : 'low warm sunset light from the left', medium: MEDIA[s.medium] ?? 'watercolour painting',
  };
}

/** imageops::thumbnail in miniature: centre-square crop, 16 x 16 RGBA. */
function thumbOf(s: Scene): Thumbnail {
  const src = paint(72, 48, s), t = document.createElement('canvas');
  t.width = 16; t.height = 16;
  const g = t.getContext('2d');
  if (!g) return { width: 16, height: 16, data: new Uint8Array(16 * 16 * 4) };
  g.drawImage(src, 12, 0, 48, 48, 0, 0, 16, 16);
  return { width: 16, height: 16, data: g.getImageData(0, 0, 16, 16).data };
}

function draftOf(s: Scene): Draft {
  const where = s.tower < 0.42 ? 'left of centre' : s.tower > 0.56 ? 'right of centre' : 'near the centre';
  const size = s.towerH > 0.4 ? 'tall and slender, its lantern high in the sky' : s.towerH > 0.3 ? 'of middling height' : 'small against the sky';
  const hz = s.horizon > 0.62 ? 'low in the frame' : s.horizon < 0.52 ? 'above the middle' : 'just below the middle';
  const boat = s.boat ? `, and a small sailing boat ${s.boatX < 0.25 ? 'far to the left' : 'left of the island'}` : '';
  return {
    prompt: `A red-and-white striped lighthouse stands on a dark rocky island ${where}, ${size}. Calm sea with the horizon ${hz}${boat}. The low sun on the left turns the sky orange, fading to violet above.`,
    style: [MEDIA[s.medium] ?? 'watercolour painting', 'soft washes'], avoid: ['text', 'people'],
  };
}

const VIEW_PROMPT = 'A red-and-white lighthouse on a rocky island at sunset, calm sea, a small sailing boat far left';
const VIEW_DOC = `${VIEW_PROMPT};aspect=16:10;style=watercolour, soft washes`;

// ---------------------------------------------------------------- the backend

interface Job { canceled: boolean }

export class MockBackend {
  private readonly listeners = new Map<string, Set<(payload: unknown) => void>>();
  private installed = params.get('installed') === '1';
  private vcOk = params.get('vc') === '1';
  private readonly rows = new Map<string, RowState>();
  private config = defaultConfig();
  private token = newToken();
  private readonly engines: SchedulerStatus = { loaded: null, state: 'stopped', active: 0, waitingLlm: 0, waitingImage: 0, mtp: true, lastError: null, swaps: 0, sinceMs: Date.now() };
  private readonly warm = new Set<EngineKind>();
  private readonly logs: Record<EngineKind, string[]> = { llm: [], image: [] };
  private engineQueue: Promise<void> = Promise.resolve();
  private download: Job | null = null;
  private readonly jobs = new Map<string, Job>();
  private jobCount = 0;
  private task = 0;
  private readonly rendered = new Set<string>();

  constructor() {
    for (const c of CATALOG) this.rows.set(c.id, this.installed ? INSTALLED : FRESH[c.id] ?? { state: 'missing', partialBytes: null, reusablePath: null, reuseLinks: null });
  }

  listen(event: string, handler: (payload: unknown) => void): () => void {
    let set = this.listeners.get(event);
    if (!set) this.listeners.set(event, (set = new Set()));
    set.add(handler);
    return () => { set?.delete(handler); };
  }

  /** Delivers like Tauri: asynchronously, as a copy. */
  private emit(event: string, payload: unknown): void {
    for (const h of this.listeners.get(event) ?? []) setTimeout(() => h(structuredClone(payload)), 0);
  }

  async invoke(cmd: string, args: Record<string, unknown>): Promise<unknown> {
    await sleep(20);
    const str = (k: string): string => { const v = args[k]; if (typeof v !== 'string' || !v) throw new Error(`missing argument ${k}`); return v; };
    switch (cmd) {
      case 'app_info': return { version: '0.1.0', dataDir: DATA_DIR, os: 'windows', backend: 'cuda', gpu: { vendor: 'nvidia', name: 'NVIDIA GeForce RTX 5070 Ti', vramTotalMb: 16303, vramUsedMb: 1312, driver: '591.44', computeCap: '12.0' }, vcRuntimeOk: this.vcOk, configProblem: null, cudaNote: null } satisfies AppInfo;
      case 'server_info': return this.serverInfo();
      case 'regenerate_token': this.token = newToken(); return { token: this.token };
      case 'status': return { engines: { ...this.engines }, installed: this.installed } satisfies StatusReply;
      case 'models_status': return this.modelsStatus();
      case 'download_start': this.startDownload(args.reuse !== false); return null;
      case 'download_cancel': if (this.download) this.download.canceled = true; return null;
      case 'unload_models': await this.unload(); return null;
      case 'engine_logs': return this.logs[args.kind === 'image' ? 'image' : 'llm'].slice(-300);
      case 'settings_get': return structuredClone(this.config);
      case 'settings_set': return this.saveSettings(args.config as Config);
      // Like the app: it returns when the installer has exited (here, asking for a restart that is not needed).
      case 'install_vc_runtime': await sleep(4000); this.vcOk = true; return { outcome: 'finished', exitCode: 3010 } satisfies VcInstallReply;
      case 'read_document': return this.readDocument(str('path'));
      case 'viewer_render': return this.render(String(args.path), typeof args.seed === 'string' ? args.seed : null);
      case 'save_file': return { path: `C:\\Users\\you\\Documents\\${str('suggestedName')}` };
      case 'open_dialog': return { path: args.kind === 'image' ? 'C:\\Users\\you\\Pictures\\harbour-lighthouse.jpg' : 'C:\\Users\\you\\Pictures\\lighthouse.jai' };
      case 'image_preview': { const c = paint(1024, 683, TARGET); return { dataUrl: c.toDataURL('image/jpeg', 0.85), width: 1024, height: 683 } satisfies ImagePreview; }
      case 'encode_start': return this.startEncode(str('path'), args.options as EncodeSettings);
      case 'encode_cancel': { const j = this.jobs.get(str('jobId')); if (j) j.canceled = true; return null; }
      case 'reveal': console.info('mock reveal', str('path')); return null;
      case 'ui_ready': return null;
      case 'third_party_notices': return 'Third-party licences of JAI Studio (browser preview: the app prints the real list).\n\nrquickjs — MIT\ntauri — Apache-2.0 OR MIT\n';
      default: throw new Error(`unknown command ${cmd}`);
    }
  }

  private serverInfo(): ServerInfo {
    const { host, port } = this.config.server;
    const url = `http://${host.includes(':') ? `[${host}]` : host}:${port}`;
    if (params.get('server') === 'down') return { url, token: this.token, host, port, running: false, note: `cannot start the server: ports ${port} to ${port + 9} are all in use` };
    return { url, token: this.token, host, port, running: true };
  }

  /** Like settings_set: the port from 1 to 65535, and the engines' extra arguments stay as stored. */
  private saveSettings(config: Config): Config {
    const port = Number(config?.server?.port);
    if (!Number.isInteger(port) || port < 1 || port > 65535) throw new Error('the port must be from 1 to 65535');
    if (!Number.isInteger(config.idleUnloadMinutes) || config.idleUnloadMinutes < 0) throw new Error('The idle time must be a whole number of minutes.');
    const next = structuredClone(config);
    next.llm.extraArgs = [...this.config.llm.extraArgs];
    next.image.extraArgs = [...this.config.image.extraArgs];
    this.config = next;
    return structuredClone(this.config);
  }

  private modelsStatus(): ModelsStatus {
    const components = CATALOG.map((c): ComponentRow => ({ id: c.id, role: c.role, label: c.label, size: c.size, license: c.license, source: c.source, ...(this.rows.get(c.id) ?? INSTALLED) }));
    const neededBytes = components.filter(c => c.state !== 'installed').reduce((a, c) => a + c.size - (c.partialBytes ?? 0), 0);
    return { components, totalBytes: components.reduce((a, c) => a + c.size, 0), neededBytes, diskFree: 412384000000 };
  }

  private readDocument(path: string): DocumentInfo {
    const name = path.split(/[\\/]/).pop() || path;
    if (/broken/i.test(name)) return { name, path, validate: { ok: false, fatal: true, text: null, diagnostics: [{ code: 'malformed-container', reason: 'truncated' }] } };
    return { name, path, validate: { ok: true, fatal: false, text: VIEW_DOC, diagnostics: [] } };
  }

  // ---------------------------------------------------------------- engines

  private setState(kind: EngineKind, state: EngineState): void {
    this.engines.loaded = state === 'stopped' ? null : kind;
    this.engines.state = state;
    this.engines.sinceMs = Date.now();
  }

  private log(kind: EngineKind, lines: string[]): void {
    const l = this.logs[kind];
    l.push(...lines);
    if (l.length > 1000) l.splice(0, l.length - 1000);
  }

  /** Loads `kind` (swapping the other engine out), then runs `work` on it; one engine at a time. */
  private async useEngine<T>(kind: EngineKind, work: () => Promise<T>): Promise<T> {
    const e = this.engines, waiting = kind === 'llm' ? 'waitingLlm' : 'waitingImage';
    e[waiting]++;
    let queued = true;
    const previous = this.engineQueue;
    let release = (): void => {};
    this.engineQueue = new Promise<void>(r => { release = r; });
    try {
      await previous;
      if (e.loaded !== kind || e.state !== 'ready') {
        if (e.loaded && e.loaded !== kind && e.state === 'ready') {
          this.setState(e.loaded, 'stopping');
          await sleep(150);
          e.swaps++;
        }
        this.setState(kind, 'starting');
        this.log(kind, ENGINE_START[kind]);
        await sleep(this.warm.has(kind) ? 350 : 1700);
        this.warm.add(kind);
        this.setState(kind, 'ready');
        this.log(kind, ENGINE_READY[kind]);
      }
      e[waiting]--;
      queued = false;
      e.active++;
      try { return await work(); } finally { e.active--; }
    } finally {
      if (queued) e[waiting]--;
      release();
    }
  }

  private async unload(): Promise<void> {
    await this.engineQueue;
    if (!this.engines.loaded) return;
    this.setState(this.engines.loaded, 'stopping');
    await sleep(300);
    this.setState('llm', 'stopped');
  }

  private workLine(kind: EngineKind, what: string): void {
    const n = ++this.task;
    if (kind === 'llm') this.log('llm', [`slot launch_slot_: id  0 | task ${n} | processing task (${what})`, `slot print_timing: id  0 | task ${n} | prompt eval time = ${(600 + Math.random() * 500).toFixed(2)} ms / ${1200 + Math.round(Math.random() * 900)} tokens, eval time = ${(1800 + Math.random() * 2400).toFixed(2)} ms / ${150 + Math.round(Math.random() * 200)} tokens (${(62 + Math.random() * 9).toFixed(1)} tokens per second)`]);
    else this.log('image', [`[INFO ] stable-diffusion.cpp:2588 - generating image: 1/1 - ${what}`, `[INFO ] stable-diffusion.cpp:2641 - generate_image completed in ${(1.2 + Math.random() * 0.4).toFixed(2)}s`]);
  }

  // ---------------------------------------------------------------- viewer

  private async render(path: string, seedArg: string | null): Promise<ViewerRender> {
    if (!this.installed) throw new Error('Download the models first: open JAI Studio or run `jai models download`.');
    const seed = seedArg !== null ? Number(BigInt(seedArg) % 4294967296n) : Math.floor(Math.random() * 4294967296);
    const key = `${path}|${seed}`, cached = this.rendered.has(key);
    if (!cached) {
      await this.useEngine('image', async () => { this.workLine('image', `seed ${seed}`); await sleep(1300); });
      this.rendered.add(key);
    }
    const scene = jitter(TARGET, 0.35, mulberry32(seed));
    return {
      dataUrl: paint(1344, 768, { ...scene, medium: 0 }).toDataURL('image/png'), width: 1344, height: 768, seed,
      sentPrompt: `${VIEW_PROMPT}\n\nStyle: watercolour, soft washes.`, description: VIEW_PROMPT, documentText: VIEW_DOC,
      diagnostics: [{ code: 'adjusted-value', key: 'aspect', requested: '16:10', actual: '16:9' }], cached,
    };
  }

  // ---------------------------------------------------------------- downloads

  private startDownload(reuse: boolean): void {
    if (this.download) throw new Error('A download is already running.');
    const job: Job = { canceled: false };
    this.download = job;
    void this.runDownload(job, reuse).finally(() => { this.download = null; });
  }

  /** Three at a time, on a clock 25 times faster than real time, at realistic speeds. */
  private async runDownload(job: Job, reuse: boolean): Promise<void> {
    type Item = { spec: Spec; p: Progress; stage: 'reusing' | 'downloading' | 'verifying' | 'unpacking'; ticks: number };
    const queue: Item[] = [];
    for (const spec of CATALOG) {
      const row = this.rows.get(spec.id);
      if (!row || row.state === 'installed') continue;
      const p: Progress = { id: spec.id, state: 'queued', bytes: row.partialBytes ?? 0, total: spec.size, speedBps: 0, error: null };
      queue.push({ spec, p, stage: reuse && row.state === 'reusable' ? 'reusing' : 'downloading', ticks: 0 });
      this.emit('download-progress', p);
    }
    const active: Item[] = [];
    const TICK_MS = 200, VIRTUAL_S = (TICK_MS / 1000) * 25;
    while (queue.length || active.length) {
      while (active.length < 3 && queue.length) {
        const it = queue.shift();
        if (!it) break;
        if (it.stage === 'reusing') it.p.bytes = 0;
        active.push(it);
      }
      await sleep(TICK_MS);
      if (job.canceled) {
        for (const it of [...active, ...queue]) {
          it.p.state = 'canceled'; it.p.speedBps = 0;
          this.emit('download-progress', it.p);
          if (it.stage === 'downloading' && it.p.bytes > 0) this.rows.set(it.spec.id, { state: 'partial', partialBytes: it.p.bytes, reusablePath: null, reuseLinks: null });
        }
        this.emit('download-done', { ok: false, error: 'Canceled' });
        return;
      }
      const downloading = active.filter(a => a.stage === 'downloading').length;
      for (const it of [...active]) {
        const p = it.p;
        if (it.stage === 'reusing' || it.stage === 'downloading') {
          const speed = it.stage === 'reusing' ? 1.4e9 : (112e6 / Math.max(1, downloading)) * (0.85 + Math.random() * 0.3);
          p.bytes = Math.min(p.total, Math.round(p.bytes + speed * VIRTUAL_S));
          p.state = it.stage; p.speedBps = Math.round(speed);
          if (p.bytes >= p.total) { it.stage = it.stage === 'reusing' ? 'unpacking' : 'verifying'; it.ticks = 0; }
        } else if (it.stage === 'verifying') {
          p.state = 'verifying'; p.speedBps = 0;
          if (++it.ticks >= 2) { it.stage = 'unpacking'; it.ticks = 0; }
        } else {
          const archive = it.spec.archive === true;
          if (archive) { p.state = 'unpacking'; p.speedBps = 0; }
          if (!archive || ++it.ticks >= 3) {
            p.state = 'done'; p.speedBps = 0;
            active.splice(active.indexOf(it), 1);
            this.rows.set(it.spec.id, INSTALLED);
          }
        }
        this.emit('download-progress', p);
      }
    }
    this.installed = true;
    this.emit('download-done', { ok: true });
  }

  // ---------------------------------------------------------------- encoder

  private startEncode(path: string, o: EncodeSettings): EncodeStarted {
    if (!o || !Number.isInteger(o.generations) || !Number.isInteger(o.population) || !Array.isArray(o.seeds) || !o.seeds.length) throw new Error('invalid encode options');
    // The app's own check (EncodeSettings::check), so the UI cannot offer what the app refuses.
    const [lo, hi] = ENCODE_LIMITS.scale;
    if (!(o.scale >= lo && o.scale <= hi)) throw new Error(`scale must be from ${lo} to ${hi}`);
    if (!this.installed) throw new Error('Download the models first: open JAI Studio or run `jai models download`.');
    const n = ++this.jobCount, jobId = `job-${n}`;
    const name = (path.split(/[\\/]/).pop() || 'image').replace(/\.[^.]+$/, '');
    const d = new Date(), pad = (x: number): string => String(x).padStart(2, '0');
    const dir = `${DATA_DIR}\\encodings\\${d.getFullYear()}${pad(d.getMonth() + 1)}${pad(d.getDate())}-${pad(d.getHours())}${pad(d.getMinutes())}${pad(d.getSeconds())}-${name}`;
    const job: Job = { canceled: false };
    this.jobs.set(jobId, job);
    void this.runEncode(jobId, dir, name, o, job, mulberry32(1000 + n)).finally(() => this.jobs.delete(jobId));
    return { jobId, dir };
  }

  private async runEncode(jobId: string, dir: string, name: string, o: EncodeSettings, job: Job, rand: () => number): Promise<void> {
    const send = (event: EncodeEvent): void => { if (!job.canceled) this.emit('encode-event', { jobId, event }); };
    const check = (): void => { if (job.canceled) throw new Error('Canceled'); };
    const targetObs = observe(TARGET), targetThumb = thumbOf(TARGET);
    let best: { cand: CandidateRef; scene: Scene } | null = null;
    let stopReason = 'generations';
    const started = performance.now();
    try {
      send({ type: 'phase', phase: 'inspect' });
      await this.useEngine('llm', async () => { this.workLine('llm', 'inspect target'); await sleep(700); });
      check();
      send({ type: 'target', observation: targetObs });
      outer: for (let lineage = 1; lineage <= o.restarts; lineage++) {
        send({ type: 'phase', phase: 'describe', lineage });
        await this.useEngine('llm', async () => { this.workLine('llm', 'describe'); await sleep(600); });
        check();
        send({ type: 'phase', phase: 'evolve', lineage });
        let base = jitter(TARGET, 1.5, rand);
        let lineBest: { cand: CandidateRef; scene: Scene } | null = null;
        let stale = 0;
        for (let gen = 0; gen < o.generations; gen++) {
          const spread = 0.6 * Math.exp(-gen / 3.5) + 0.07;
          const scenes = Array.from({ length: o.population }, () => jitter(base, spread, rand));
          // Every render of the generation at once on the image engine, then every inspection on the LLM.
          const shots = await this.useEngine('image', async () => {
            const out: Array<{ scene: Scene; seed: number; thumb: string }> = [];
            for (const s of scenes) for (const seed of o.seeds) {
              check();
              const shot = jitter(s, 0.05, mulberry32(seed * 7919 + gen));
              this.workLine('image', `seed ${seed}`);
              out.push({ scene: shot, seed, thumb: paint(384, 263, shot).toDataURL('image/jpeg', 0.8) });
              await sleep(70);
            }
            return out;
          });
          const cands = await this.useEngine('llm', async () => {
            const list: Array<{ cand: CandidateRef; scene: Scene }> = [];
            for (const [k, s] of scenes.entries()) {
              const id = `l${lineage}g${gen}c${k + 1}`;
              const renders: RenderRef[] = shots.slice(k * o.seeds.length, (k + 1) * o.seeds.length).map(shot => {
                const m = measure(targetObs, observe(shot.scene), { target: targetThumb, render: thumbOf(shot.scene) });
                return { model: FLUX, seed: shot.seed, file: `${id}-s${shot.seed}.jpg`, error: null, measure: m, score: m.fitness, thumb: shot.thumb };
              });
              for (let i = 0; i < renders.length; i++) { this.workLine('llm', `inspect ${id}`); await sleep(50); }
              check();
              const fitness = renders.reduce((a, r) => a + r.score, 0) / renders.length;
              const cand: CandidateRef = { id, lineage, generation: gen, phase: 'evolve', draft: draftOf(s), refinements: {}, renders, perModel: { [FLUX]: fitness }, fitness };
              list.push({ cand, scene: s });
              send({ type: 'candidate', candidate: cand });
            }
            return list;
          });
          const top = cands.reduce((a, b) => (b.cand.fitness > a.cand.fitness ? b : a));
          const gain = lineBest ? top.cand.fitness - lineBest.cand.fitness : Infinity;
          if (!lineBest || top.cand.fitness > lineBest.cand.fitness) lineBest = top;
          if (!best || lineBest.cand.fitness > best.cand.fitness) best = lineBest;
          stale = gain >= 0.01 ? 0 : stale + 1;
          send({ type: 'generation', generation: { lineage, index: gen, phase: 'evolve', candidates: cands.map(c => c.cand.id), best: lineBest.cand.id, bestFitness: lineBest.cand.fitness, notes: gen ? 'Kept the lighthouse right of centre; moving the horizon lower and making the tower taller.' : 'First drafts.', spent: 0 } });
          if (lineBest.cand.fitness >= o.goal) { stopReason = 'goal'; break outer; }
          if (stale >= 3) { stopReason = 'plateau'; break; }
          if (gen + 1 >= o.generations) { stopReason = 'generations'; break; }
          await this.useEngine('llm', async () => { this.workLine('llm', 'propose'); await sleep(350); });
          check();
          base = toward(lineBest.scene, 0.2);
        }
      }
      send({ type: 'phase', phase: 'done' });
      this.emit('encode-done', this.result(jobId, dir, name, best?.cand ?? null, stopReason));
    } catch (e) {
      const canceled = job.canceled;
      const done: EncodeDone = canceled && best ? this.result(jobId, dir, name, best.cand, 'canceled') : { jobId, ok: false, reportDir: dir, error: canceled ? 'Canceled' : String(e), stopReason: canceled ? 'canceled' : 'error' };
      this.emit('encode-done', done);
    }
    console.info(`mock encode ${jobId} took ${Math.round((performance.now() - started) / 100) / 10} s`);
  }

  private result(jobId: string, dir: string, name: string, best: CandidateRef | null, stopReason: string): EncodeDone {
    if (!best) return { jobId, ok: false, reportDir: dir, error: 'No candidate was evaluated.', stopReason };
    const d = best.draft;
    const text = `${d.prompt};aspect=3:2;avoid=${d.avoid.join(', ')};style=${d.style.join(', ')}`;
    return { jobId, ok: true, text, jaiPath: `${dir}\\${name}.jai`, reportDir: dir, stopReason };
  }
}
