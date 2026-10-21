// Encode page (app layer): UI, image decoding and thumbnails with canvas, and
// provider access through the background, which alone holds the tokens: a
// ReplicateClient that sends every prediction there, or a fetch for JAI
// Studio's two endpoints. The engine (src/encoder) does the rest.
import { encode as encodeFile, type JaiDocument } from '../jai/index.ts';
import { GeminiAnalyst, LocalAnalyst, LocalRenderer, LOCAL_FLUX, OpenAIAnalyst, OpenAIChatClient, encode, type Candidate, type EncodeEvent, type ImageRef, type Thumbnail } from '../encoder/index.ts';
import type { FetchLike, ModelRef, PredictOptions, Prediction, ReplicateClient } from '../providers/replicate-client.ts';
import { REPLICATE_MODELS, ReplicateRenderer } from '../renderers/replicate.ts';
import { RenderError } from '../renderers/types.ts';
import { getSettings, readCapped, type EncodePortReply, type RendererChoice, type StudioStatus } from './shared.ts';

const $ = <T extends HTMLElement = HTMLElement>(id: string): T => document.getElementById(id) as T;
const el = <K extends keyof HTMLElementTagNameMap>(tag: K, props: Partial<HTMLElementTagNameMap[K]> = {}, ...kids: Array<Node | string>): HTMLElementTagNameMap[K] => {
  const e = Object.assign(document.createElement(tag), props);
  e.append(...kids);
  return e;
};
const usd = (n: number): string => `$${n.toFixed(3)}`;

// ---------------------------------------------------------------- provider access through the background

class PortClient implements ReplicateClient {
  /** The background enforces this run's limit too; it is declared once when the port opens. */
  constructor(private readonly budgetUSD: number) {}
  private port: chrome.runtime.Port | null = null;
  private next = 1;
  private waiting = new Map<number, { resolve: (p: Prediction) => void; reject: (e: Error) => void; onStatus?: (s: string) => void }>();
  private connect(): chrome.runtime.Port {
    if (this.port) return this.port;
    const port = chrome.runtime.connect({ name: 'encode' });
    port.postMessage({ type: 'begin', renderer: 'replicate', budgetUSD: this.budgetUSD });
    port.onMessage.addListener((m: EncodePortReply) => {
      const w = this.waiting.get(m.id);
      if (!w || m.type === 'studio-reply') return;
      if (m.type === 'status') { w.onStatus?.(m.status); return; }
      this.waiting.delete(m.id);
      if (m.type === 'result') w.resolve(m.prediction as Prediction);
      else w.reject(new RenderError(m.message, m.kind as RenderError['kind'], m.jobId));
    });
    port.onDisconnect.addListener(() => {
      this.port = null;
      for (const w of this.waiting.values()) w.reject(new RenderError('The extension stopped while a request was running', 'network'));
      this.waiting.clear();
    });
    this.port = port;
    return port;
  }
  predict(model: ModelRef, input: Record<string, unknown>, opts: PredictOptions = {}): Promise<Prediction> {
    const id = this.next++;
    const port = this.connect();
    return new Promise((resolve, reject) => {
      if (opts.signal?.aborted) return reject(new RenderError('Canceled', 'canceled'));
      this.waiting.set(id, { resolve, reject, onStatus: opts.onStatus });
      opts.signal?.addEventListener('abort', () => {
        if (!this.waiting.delete(id)) return;
        port.postMessage({ type: 'cancel', id });
        reject(new RenderError('Canceled', 'canceled'));
      }, { once: true });
      port.postMessage({ type: 'predict', id, model, input });
    });
  }
}

/**
 * A fetch for JAI Studio's two endpoints, through the background: it sends the
 * path and JSON body over the encode port, and the background adds the server
 * and the token. Studio's replies, and the background's own refusals in the
 * same shape, come back as responses; it throws only when the port drops.
 */
class StudioPort {
  private port: chrome.runtime.Port | null = null;
  private next = 1;
  private waiting = new Map<number, { resolve: (r: Response) => void; reject: (e: Error) => void }>();
  private connect(): chrome.runtime.Port {
    if (this.port) return this.port;
    const port = chrome.runtime.connect({ name: 'encode' });
    port.postMessage({ type: 'begin', renderer: 'studio' });
    port.onMessage.addListener((m: EncodePortReply) => {
      if (m.type !== 'studio-reply') return;
      const w = this.waiting.get(m.id);
      if (!w) return;
      this.waiting.delete(m.id);
      w.resolve(new Response(m.text, { status: m.status, headers: { 'content-type': 'application/json' } }));
    });
    port.onDisconnect.addListener(() => {
      this.port = null;
      for (const w of this.waiting.values()) w.reject(new TypeError("the extension's background stopped"));
      this.waiting.clear();
    });
    this.port = port;
    return port;
  }
  readonly fetch: FetchLike = (url, init = {}) => {
    const id = this.next++;
    const port = this.connect();
    return new Promise((resolve, reject) => {
      const signal = init.signal;
      if (signal?.aborted) return reject(new DOMException('Aborted', 'AbortError'));
      this.waiting.set(id, { resolve, reject });
      signal?.addEventListener('abort', () => {
        if (!this.waiting.delete(id)) return;
        port.postMessage({ type: 'cancel', id });
        reject(new DOMException('Aborted', 'AbortError'));
      }, { once: true });
      port.postMessage({ type: 'studio', id, path: new URL(url).pathname, body: JSON.parse(String(init.body)) });
    });
  };
}
/** The base the local providers build their URLs on; the background replaces it with the server it found. */
const STUDIO_BASE = 'http://127.0.0.1';

// ---------------------------------------------------------------- images

const THUMB = 16;
function coverThumb(src: CanvasImageSource, w: number, h: number): Thumbnail {
  const c = new OffscreenCanvas(THUMB, THUMB);
  const ctx = c.getContext('2d')!;
  const side = Math.min(w, h);
  ctx.drawImage(src, (w - side) / 2, (h - side) / 2, side, side, 0, 0, THUMB, THUMB);
  return { width: THUMB, height: THUMB, data: ctx.getImageData(0, 0, THUMB, THUMB).data };
}
const blobToDataURL = (b: Blob): Promise<string> => new Promise((res, rej) => { const r = new FileReader(); r.onload = () => res(String(r.result)); r.onerror = () => rej(r.error); r.readAsDataURL(b); });

interface Loaded { target: ImageRef & { width: number; height: number }; thumb: Thumbnail; name: string; previewURL: string }
const MAX_BYTES = 40 * 1024 * 1024;
const MAX_PIXELS = 40e6;

/** Read the dimensions from the header before any full decode (decompression bombs). */
function dimensionsOf(blob: Blob): Promise<{ width: number; height: number }> {
  const url = URL.createObjectURL(blob);
  return new Promise<{ width: number; height: number }>((resolve, reject) => {
    const img = new Image();
    img.decoding = 'async';
    img.onload = () => resolve({ width: img.naturalWidth, height: img.naturalHeight });
    img.onerror = () => reject(new Error('not an image this browser can read'));
    img.src = url;
  }).finally(() => URL.revokeObjectURL(url));
}

async function loadImage(blob: Blob, name: string): Promise<Loaded> {
  if (blob.size > MAX_BYTES) throw new Error('That image is larger than 40 MB.');
  if (blob.type && !/^image\//.test(blob.type)) throw new Error('That file is not an image.');
  const { width, height } = await dimensionsOf(blob);
  if (!width || !height) throw new Error('That image has no size.');
  if (width * height > MAX_PIXELS) throw new Error(`That image has more than ${MAX_PIXELS / 1e6} megapixels.`);
  const scale = Math.min(1, 1024 / Math.max(width, height));
  // Decode straight to the upload size.
  const bmp = await createImageBitmap(blob, { imageOrientation: 'from-image', resizeWidth: Math.max(1, Math.round(width * scale)), resizeHeight: Math.max(1, Math.round(height * scale)), resizeQuality: 'high' });
  const c = new OffscreenCanvas(bmp.width, bmp.height);
  c.getContext('2d')!.drawImage(bmp, 0, 0);
  const jpeg = await c.convertToBlob({ type: 'image/jpeg', quality: 0.85 });
  const thumb = coverThumb(bmp, bmp.width, bmp.height);
  bmp.close();
  return { target: { url: await blobToDataURL(jpeg), width, height }, thumb, name, previewURL: URL.createObjectURL(jpeg) };
}

// ---------------------------------------------------------------- UI

let loaded: Loaded | null = null;
let running: AbortController | null = null;
let objectURLs: string[] = [];
let resultDoc: JaiDocument | null = null;

const modelsBox = $('models');
for (const m of REPLICATE_MODELS) {
  const box = el('input', { type: 'checkbox', value: m.id, checked: m.id === 'black-forest-labs/flux-schnell' });
  box.addEventListener('change', refresh);
  modelsBox.append(el('label', {}, box, el('span', {}, `${m.label} `, el('span', { className: 'muted' }, `about ${usd(m.costHint)} per render · ${m.description}`))));
}
const chosenModels = (): string[] => [...modelsBox.querySelectorAll<HTMLInputElement>('input:checked')].map(i => i.value);

let renderer: RendererChoice = 'replicate';

function refresh(): void {
  const start = $<HTMLButtonElement>('start'), cancel = $<HTMLButtonElement>('cancel');
  const hadFocus = document.activeElement === start || document.activeElement === cancel;
  start.disabled = !!running || !loaded || (renderer !== 'studio' && (!chosenModels().length || !$<HTMLInputElement>('consent').checked));
  cancel.disabled = !running;
  // Keep keyboard focus on whichever button is usable.
  if (hadFocus && (document.activeElement as HTMLButtonElement | null)?.disabled) (running ? cancel : start).focus();
}
$('consent').addEventListener('change', refresh);

function log(message: string): void {
  const box = $('log');
  const atEnd = box.scrollTop + box.clientHeight >= box.scrollHeight - 4;
  box.append(el('li', {}, message));
  if (atEnd) box.scrollTop = box.scrollHeight; // scroll the log only, never the page
}

async function useBlob(blob: Blob, name: string): Promise<void> {
  try {
    loaded = await loadImage(blob, name);
    const img = $<HTMLImageElement>('preview');
    img.src = loaded.previewURL; img.alt = `Image to encode: ${name}`; img.hidden = false;
    $('dims').textContent = `${name}, ${loaded.target.width} × ${loaded.target.height} pixels`;
  } catch (e) {
    loaded = null;
    $('dims').textContent = `Could not read that image: ${(e as Error).message}`;
  }
  refresh();
}

$<HTMLInputElement>('file').addEventListener('change', e => { const f = (e.target as HTMLInputElement).files?.[0]; if (f) void useBlob(f, f.name); });
const drop = $('drop');
drop.addEventListener('dragover', e => { e.preventDefault(); drop.classList.add('over'); });
drop.addEventListener('dragleave', () => drop.classList.remove('over'));
drop.addEventListener('drop', e => { e.preventDefault(); drop.classList.remove('over'); const f = e.dataTransfer?.files?.[0]; if (f) void useBlob(f, f.name); });
document.addEventListener('paste', e => {
  if (running) return;
  const f = [...(e.clipboardData?.files ?? [])].find(x => x.type.startsWith('image/'));
  if (f) { e.preventDefault(); void useBlob(f, f.name || 'pasted image'); }
});

// Right-click "Encode this image": the URL arrives in the fragment; loading it needs the user's click
// (and, for http(s), permission for that one origin, asked for at that moment).
const src = new URLSearchParams(location.hash.slice(1)).get('src');
// Drop the URL from the address bar and history (signed image URLs are secrets too).
if (location.hash) history.replaceState(null, '', location.pathname);
const fileName = (url: string): string => { try { return decodeURIComponent(new URL(url).pathname.split('/').pop() || 'image'); } catch { return 'image'; } };
if (src && /^(https?:\/\/|data:image\/)/i.test(src)) {
  $('src-row').hidden = false;
  $('load-src').textContent = src.startsWith('data:') ? 'Load the image you right-clicked' : `Load the image from ${new URL(src).host}`;
  $('load-src').addEventListener('click', async () => {
    let granted: string | null = null;
    try {
      if (!src.startsWith('data:')) {
        const origin = new URL(src).origin + '/*';
        // Ask only for this one origin, and give the permission back afterwards unless the user already had it.
        if (!(await chrome.permissions.contains({ origins: [origin] }))) {
          if (!(await chrome.permissions.request({ origins: [origin] }))) { $('dims').textContent = 'Permission to read that site was not granted. Save the image and drop it here instead.'; return; }
          granted = origin;
        }
      }
      const res = await fetch(src, { credentials: 'omit', referrerPolicy: 'no-referrer' });
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      const type = res.headers.get('content-type') ?? '';
      if (!src.startsWith('data:') && !/^image\//i.test(type)) throw new Error('the server did not send an image');
      const bytes = await readCapped(res, MAX_BYTES);
      if (!bytes) throw new Error('larger than 40 MB');
      await useBlob(new Blob([bytes as BlobPart], { type: type.split(';')[0] }), src.startsWith('data:') ? 'image' : fileName(src));
    } catch (e) { $('dims').textContent = `Could not load that image: ${(e as Error).message}`; }
    finally { if (granted) void chrome.permissions.remove({ origins: [granted] }); }
  });
}

// The renderer chosen in the options decides where the run goes; this page only shows it.
void (async () => {
  renderer = (await getSettings()).renderer;
  const studio = renderer === 'studio';
  $('renderer-name').textContent = studio ? 'JAI Studio on this computer' : 'Replicate';
  for (const e of document.querySelectorAll<HTMLElement>('.replicate-only')) e.hidden = studio;
  for (const e of document.querySelectorAll<HTMLElement>('.studio-only')) e.hidden = !studio;
  if (studio) {
    const st = await chrome.runtime.sendMessage({ type: 'studioStatus' }) as StudioStatus | null;
    $('token-note-text').textContent = !st?.saved ? 'JAI Studio is not connected.'
      : !st.permission ? 'The permission to reach JAI Studio was removed.'
      : !st.running ? 'No JAI Studio with this token answers. Start JAI Studio, or connect it again with its new token.' : '';
    $('token-note').hidden = !!st?.saved && st.running;
  } else {
    const t = await chrome.runtime.sendMessage({ type: 'tokenStatus' }) as { set: boolean } | null;
    $('token-note').hidden = !!t?.set;
  }
  refresh();
})();
for (const a of document.querySelectorAll('.open-options')) a.addEventListener('click', e => { e.preventDefault(); void chrome.runtime.openOptionsPage(); });

function candidateCard(c: Candidate): HTMLElement {
  const figs = c.renders.map(r => {
    let img: HTMLElement;
    if (r.result) {
      const url = URL.createObjectURL(new Blob([r.result.bytes as BlobPart], { type: r.result.mime }));
      objectURLs.push(url);
      img = el('img', { src: url, alt: `AI render of candidate ${c.id} by ${REPLICATE_MODELS.find(m => m.id === r.model)?.label ?? r.model}` });
    } else img = el('div', { className: 'err', title: r.error ?? '' }, 'failed');
    const label = REPLICATE_MODELS.find(m => m.id === r.model)?.label ?? r.model;
    const fig = el('figure', {}, img, el('figcaption', {}, `${label} ${r.score.toFixed(3)}`));
    if (r.measure) fig.title = r.measure.feedback;
    return fig;
  });
  const refinements = Object.entries(c.refinements).map(([m, v]) => `+ ${m.split('/')[1]}: ${v.join(' ')}`).join('\n');
  const card = el('div', { className: 'cand' }, el('div', {}, el('b', {}, c.id), ` ${c.fitness.toFixed(3)}`), el('div', { className: 'imgs' }, ...figs), el('p', {}, c.draft.prompt + (refinements ? `\n${refinements}` : '')));
  card.dataset.id = c.id;
  return card;
}

let currentGen: HTMLElement | null = null;
function onEvent(e: EncodeEvent): void {
  if (e.type === 'target') {
    const o = e.observation, n = o.people.length;
    log(`Measured the image: ${o.subjects.map(s => s.name).join(', ') || 'no distinct subjects'}${n ? `, ${n} ${n === 1 ? 'person' : 'people'}` : ''}${o.horizon !== null ? `, horizon at ${Math.round(o.horizon * 100)}%` : ''}.`);
  }
  if (e.type === 'phase') { log({ inspect: 'Measuring the image…', describe: `Describing the image${e.lineage && e.lineage > 1 ? ` (attempt ${e.lineage})` : ''}…`, evolve: 'Rendering, measuring and improving…', refine: 'Trying per-model refinements…', done: 'Done.' }[e.phase]); currentGen = null; if (renderer !== 'studio') log(`${$('spend').textContent || 'Nothing spent yet.'}`); }
  if (e.type === 'log') log(e.message);
  if (e.type === 'spend' && renderer !== 'studio') $('spend').textContent = `Estimated spend ${usd(e.spent)} of ${usd(e.limit)}`;
  if (e.type === 'candidate') {
    if (!currentGen) { currentGen = el('section', { className: 'gen' }, el('h3', {}, e.candidate.phase === 'refine' ? 'Refinement' : `Generation ${e.candidate.generation}`), el('div', { className: 'cands' })); $('gens').append(currentGen); }
    currentGen.querySelector('.cands')!.append(candidateCard(e.candidate));
  }
  if (e.type === 'generation') {
    const g = e.generation;
    if (currentGen) {
      currentGen.querySelector('h3')!.textContent = `${g.phase === 'refine' ? 'Refinement' : 'Generation'} ${g.index}: best ${g.best} ${g.bestFitness.toFixed(3)}`;
      currentGen.querySelector(`[data-id="${CSS.escape(g.best)}"]`)?.classList.add('best');
      if (g.notes) currentGen.append(el('p', { className: 'muted' }, g.notes));
    }
    currentGen = null;
  }
}

$('start').addEventListener('click', async () => {
  if (!loaded || running) return;
  for (const u of objectURLs) URL.revokeObjectURL(u);
  objectURLs = [];
  $('log').textContent = ''; $('gens').textContent = ''; $('result-card').hidden = true; $('progress-card').hidden = false;
  const studio = renderer === 'studio';
  // JAI Studio: its Qwen and FLUX.1 schnell, free, with the settings of Studio's own Encode tab (host-entry.ts).
  // Replicate: the analyst and models chosen here, within the spending limit.
  let providers;
  if (studio) {
    const port = new StudioPort();
    providers = {
      models: [LOCAL_FLUX.id], budgetUSD: 0, refine: false,
      renderer: new LocalRenderer(STUDIO_BASE, '', { fetch: port.fetch, outputFormat: 'jpeg' }),
      analyst: new LocalAnalyst(new OpenAIChatClient(STUDIO_BASE, '', { fetch: port.fetch })),
    };
  } else {
    const budgetUSD = Math.min(5, Math.max(0.05, Number($<HTMLInputElement>('budget').value) || 0.5));
    const client = new PortClient(budgetUSD);
    providers = {
      models: chosenModels(), budgetUSD,
      renderer: new ReplicateRenderer(client, { foldAvoid: true }),
      analyst: $<HTMLSelectElement>('analyst').value === 'gemini' ? new GeminiAnalyst(client) : new OpenAIAnalyst(client),
    };
  }
  running = new AbortController();
  refresh();
  try {
    const r = await encode({
      target: loaded.target, ...providers,
      thumbnail: async res => { const b = await createImageBitmap(new Blob([res.bytes as BlobPart], { type: res.mime })); const t = coverThumb(b, b.width, b.height); b.close(); return t; },
      targetThumbnail: loaded.thumb, restarts: Math.min(3, Math.max(1, Number($<HTMLInputElement>('restarts').value) || 1)),
      population: Math.min(6, Math.max(2, Number($<HTMLInputElement>('population').value) || 4)),
      maxGenerations: Math.min(12, Math.max(1, Number($<HTMLInputElement>('generations').value) || 8)),
      signal: running.signal, onEvent,
    });
    resultDoc = r.document;
    $('result-text').textContent = r.text;
    const table = $('ranking');
    table.textContent = '';
    table.append(el('tr', {}, el('th', {}, 'Model'), el('th', {}, 'Score')), ...r.ranking.map(x => el('tr', {}, el('td', {}, REPLICATE_MODELS.find(m => m.id === x.model)?.label ?? x.model), el('td', {}, x.score.toFixed(3)))));
    const reasons: Record<string, string> = { goal: 'the goal was reached', budget: 'the spending limit was reached', plateau: 'scores stopped improving', generations: 'all generations ran', time: 'the time limit was reached', canceled: 'you canceled', error: 'of an error', complete: 'it finished' };
    $('result-note').textContent = `Stopped because ${reasons[r.stopReason]}${r.error ? ` (${r.error})` : ''}. ${studio ? `Rendered and measured with JAI Studio's local models in ${Math.round(r.seconds)} s.` : `Estimated spend ${usd(r.spent)} in ${Math.round(r.seconds)} s.`}`;
    $('result-card').hidden = false;
    $('result-heading').focus();
  } catch (e) {
    log(`Encoding failed: ${(e as Error).message}`);
    $('dims').textContent = `Encoding failed: ${(e as Error).message}`;
  } finally {
    running = null;
    refresh();
  }
});
$('cancel').addEventListener('click', () => running?.abort());
$('copy').addEventListener('click', () => { void navigator.clipboard.writeText($('result-text').textContent ?? ''); });
$('download').addEventListener('click', async () => {
  if (!resultDoc) return;
  const bytes = await encodeFile(resultDoc);
  const url = URL.createObjectURL(new Blob([bytes as BlobPart], { type: 'image/jai' }));
  const a = el('a', { href: url, download: `${(loaded?.name ?? 'image').replace(/\.[^.]+$/, '')}.jai` });
  a.click();
  setTimeout(() => URL.revokeObjectURL(url), 1000);
});
refresh();
