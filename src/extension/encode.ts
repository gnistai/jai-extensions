// Encode page (app layer): UI, image decoding and thumbnails with canvas, and a
// ReplicateClient that sends every prediction through the background, which
// alone holds the token. The engine (src/encoder) does the rest.
import { encode as encodeFile, type JaiDocument } from '../jai/index.ts';
import { ClaudeAnalyst, ClipScorer, GeminiAnalyst, encode, type Candidate, type EncodeEvent, type ImageRef, type Thumbnail } from '../encoder/index.ts';
import type { ModelRef, PredictOptions, Prediction, ReplicateClient } from '../providers/replicate-client.ts';
import { REPLICATE_MODELS, ReplicateRenderer } from '../renderers/replicate.ts';
import { RenderError } from '../renderers/types.ts';
import type { EncodePortReply } from './shared.ts';

const $ = <T extends HTMLElement = HTMLElement>(id: string): T => document.getElementById(id) as T;
const el = <K extends keyof HTMLElementTagNameMap>(tag: K, props: Partial<HTMLElementTagNameMap[K]> = {}, ...kids: Array<Node | string>): HTMLElementTagNameMap[K] => {
  const e = Object.assign(document.createElement(tag), props);
  e.append(...kids);
  return e;
};
const usd = (n: number): string => `$${n.toFixed(3)}`;

// ---------------------------------------------------------------- provider access through the background

class PortClient implements ReplicateClient {
  private port: chrome.runtime.Port | null = null;
  private next = 1;
  private waiting = new Map<number, { resolve: (p: Prediction) => void; reject: (e: Error) => void; onStatus?: (s: string) => void }>();
  private connect(): chrome.runtime.Port {
    if (this.port) return this.port;
    const port = chrome.runtime.connect({ name: 'encode' });
    port.onMessage.addListener((m: EncodePortReply) => {
      const w = this.waiting.get(m.id);
      if (!w) return;
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
async function loadImage(blob: Blob, name: string): Promise<Loaded> {
  if (blob.size > 40 * 1024 * 1024) throw new Error('That image is larger than 40 MB.');
  const bmp = await createImageBitmap(blob, { imageOrientation: 'from-image' });
  const { width, height } = bmp;
  if (width * height > 64e6) throw new Error('That image has more than 64 megapixels.');
  const scale = Math.min(1, 1024 / Math.max(width, height));
  const c = new OffscreenCanvas(Math.round(width * scale), Math.round(height * scale));
  c.getContext('2d')!.drawImage(bmp, 0, 0, c.width, c.height);
  const jpeg = await c.convertToBlob({ type: 'image/jpeg', quality: 0.85 });
  const thumb = coverThumb(bmp, width, height);
  bmp.close();
  return { target: { url: await blobToDataURL(jpeg), width, height }, thumb, name, previewURL: URL.createObjectURL(jpeg) };
}

async function composite(images: ImageRef[]): Promise<ImageRef> {
  const tiles = await Promise.all(images.map(async i => createImageBitmap(await (await fetch(i.url)).blob())));
  const c = new OffscreenCanvas(512 * tiles.length, 512);
  const ctx = c.getContext('2d')!;
  ctx.fillStyle = '#fff'; ctx.fillRect(0, 0, c.width, c.height);
  tiles.forEach((t, k) => { const s = Math.min(512 / t.width, 512 / t.height); ctx.drawImage(t, 512 * k + (512 - t.width * s) / 2, (512 - t.height * s) / 2, t.width * s, t.height * s); t.close(); });
  return { url: await blobToDataURL(await c.convertToBlob({ type: 'image/jpeg', quality: 0.85 })) };
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

function refresh(): void {
  $<HTMLButtonElement>('start').disabled = !!running || !loaded || !chosenModels().length || !$<HTMLInputElement>('consent').checked;
  $<HTMLButtonElement>('cancel').disabled = !running;
}
$('consent').addEventListener('change', refresh);

function log(message: string): void {
  const li = el('li', {}, message);
  $('log').append(li);
  li.scrollIntoView({ block: 'nearest' });
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

// Right-click "Encode this image": the URL arrives in the fragment; loading it needs the user's click
// (and, for http(s), permission for that one origin, asked for at that moment).
const src = new URLSearchParams(location.hash.slice(1)).get('src');
if (src && /^(https?|data):/i.test(src)) {
  $('src-row').hidden = false;
  $('load-src').textContent = src.startsWith('data:') ? 'Load the image you right-clicked' : `Load the image from ${new URL(src).host}`;
  $('load-src').addEventListener('click', async () => {
    try {
      if (!src.startsWith('data:')) {
        const origin = new URL(src).origin + '/*';
        if (!(await chrome.permissions.request({ origins: [origin] }))) { $('dims').textContent = 'Permission to read that site was not granted. Save the image and drop it here instead.'; return; }
      }
      const res = await fetch(src, { credentials: 'omit', referrerPolicy: 'no-referrer' });
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      await useBlob(await res.blob(), src.startsWith('data:') ? 'image' : decodeURIComponent(new URL(src).pathname.split('/').pop() || 'image'));
    } catch (e) { $('dims').textContent = `Could not load that image: ${(e as Error).message}`; }
  });
}

void chrome.runtime.sendMessage({ type: 'tokenStatus' }).then((t: { set: boolean } | null) => { $('token-note').hidden = !!t?.set; });
$('open-options').addEventListener('click', e => { e.preventDefault(); void chrome.runtime.openOptionsPage(); });

function candidateCard(c: Candidate): HTMLElement {
  const figs = c.renders.map(r => {
    let img: HTMLElement;
    if (r.result) {
      const url = URL.createObjectURL(new Blob([r.result.bytes as BlobPart], { type: r.result.mime }));
      objectURLs.push(url);
      img = el('img', { src: url, alt: `Render by ${r.model}` });
    } else img = el('div', { className: 'err' }, 'failed');
    const label = REPLICATE_MODELS.find(m => m.id === r.model)?.label ?? r.model;
    return el('figure', {}, img, el('figcaption', {}, `${label} ${r.score.toFixed(3)}${r.judge !== null ? ` · rated ${Math.round(r.judge * 100)}` : ''}`));
  });
  const refinements = Object.entries(c.refinements).map(([m, v]) => `+ ${m.split('/')[1]}: ${v.join(' ')}`).join('\n');
  const card = el('div', { className: 'cand' }, el('div', {}, el('b', {}, c.id), ` ${c.fitness.toFixed(3)}`), el('div', { className: 'imgs' }, ...figs), el('p', {}, c.draft.prompt + (refinements ? `\n${refinements}` : '')));
  card.dataset.id = c.id;
  return card;
}

let currentGen: HTMLElement | null = null;
function onEvent(e: EncodeEvent): void {
  if (e.type === 'phase') { log({ describe: 'Describing the image…', evolve: 'Rendering and revising…', refine: 'Trying per-model refinements…', done: 'Done.' }[e.phase]); currentGen = null; }
  if (e.type === 'log') log(e.message);
  if (e.type === 'spend') $('spend').textContent = `Estimated spend ${usd(e.spent)} of ${usd(e.limit)}`;
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
  const budget = Math.min(5, Math.max(0.05, Number($<HTMLInputElement>('budget').value) || 0.5));
  const client = new PortClient();
  const analyst = $<HTMLSelectElement>('analyst').value === 'claude' ? new ClaudeAnalyst(client, { composite }) : new GeminiAnalyst(client);
  running = new AbortController();
  refresh();
  try {
    const r = await encode({
      target: loaded.target, models: chosenModels(), renderer: new ReplicateRenderer(client, { foldAvoid: true }), analyst, scorer: new ClipScorer(client),
      thumbnail: async res => { const b = await createImageBitmap(new Blob([res.bytes as BlobPart], { type: res.mime })); const t = coverThumb(b, b.width, b.height); b.close(); return t; },
      targetThumbnail: loaded.thumb, judgeWeight: $<HTMLInputElement>('judge').checked ? 0.5 : 0,
      budgetUSD: budget, population: Math.min(6, Math.max(2, Number($<HTMLInputElement>('population').value) || 4)),
      maxGenerations: Math.min(10, Math.max(1, Number($<HTMLInputElement>('generations').value) || 5)),
      signal: running.signal, onEvent,
    });
    resultDoc = r.document;
    $('result-text').textContent = r.text;
    const table = $('ranking');
    table.textContent = '';
    table.append(el('tr', {}, el('th', {}, 'Model'), el('th', {}, 'Score')), ...r.ranking.map(x => el('tr', {}, el('td', {}, REPLICATE_MODELS.find(m => m.id === x.model)?.label ?? x.model), el('td', {}, x.score.toFixed(3)))));
    const reasons: Record<string, string> = { budget: 'the spending limit was reached', plateau: 'scores stopped improving', generations: 'all generations ran', time: 'the time limit was reached', canceled: 'you canceled', error: 'of an error', complete: 'it finished' };
    $('result-note').textContent = `Stopped because ${reasons[r.stopReason]}${r.error ? ` (${r.error})` : ''}. Estimated spend ${usd(r.spent)} in ${Math.round(r.seconds)} s.`;
    $('result-card').hidden = false;
  } catch (e) {
    log(`Encoding failed: ${(e as Error).message}`);
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
