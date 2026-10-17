// View tab (studio/DESIGN.md section 11): open or drop a .jai document, render
// it on this PC, and show the image fitted to the panel with the description,
// the exact sent prompt, the diagnostics and the seed; Render again (new seed),
// Save image and Copy HTML. Until the first render is ready, and if it fails,
// the document's preview or else its description stands in for the image.
import { interpret, parse } from '../../jai/index.ts';
import { api, engineView, type Diag, type StatusReply, type ViewerRender } from './api.ts';
import { $, baseName, copyText, el, errorText, fmtClock, randomSeed, say, stem, toast } from './dom.ts';
import type { TabContext } from './main.ts';

export interface ViewTab {
  open(path: string): void;
  openDialog(): void;
}

/** One diagnostic in words: code, key, and what was asked for and used. */
function diagItem(d: Diag): HTMLLIElement {
  const parts: string[] = [];
  if (d.requested !== undefined && d.actual !== undefined) parts.push(`asked for ${d.requested}, used ${d.actual}`);
  else if (d.requested !== undefined) parts.push(`asked for ${d.requested}`);
  else if (d.actual !== undefined) parts.push(`used ${d.actual}`);
  if (d.reason) parts.push(d.reason);
  return el('li', {}, el('code', {}, d.code), d.key ? el('span', { className: 'diag-key' }, ` ${d.key}`) : null, parts.length ? `: ${parts.join('; ')}` : null);
}

const attr = (s: string): string => s.replace(/&/g, '&amp;').replace(/"/g, '&quot;').replace(/</g, '&lt;').replace(/>/g, '&gt;');

export function initView(ctx: TabContext): ViewTab {
  const drop = $('view-drop'), figure = $('view-figure'), img = $<HTMLImageElement>('view-img');
  const wait = $('view-wait'), waitText = $('view-wait-text');
  const title = $('view-title'), meta = $('view-meta'), status = $('view-status'), modelsLink = $<HTMLButtonElement>('view-models');
  const actions = $('view-actions'), details = $('view-details'), diagBox = $('view-diag-box');
  const fallback = $('view-fallback'), kv = $('view-kv'), promptBox = $('view-prompt-box');
  const again = $<HTMLButtonElement>('view-again'), save = $<HTMLButtonElement>('view-save'), copy = $<HTMLButtonElement>('view-copy');
  let doc: { path: string; name: string } | null = null;
  /** The open document's description and preview, once it was read. */
  let read: { description: string; preview: string | null } | null = null;
  let shown: ViewerRender | null = null;
  let busy = false, seq = 0, timer = 0, startedAt = 0;
  let engines: StatusReply | null = null;

  function updateWait(): void {
    const v = engines ? engineView(engines.engines, 'image') : 'off';
    const what = v === 'starting' || v === 'waiting' || v === 'stopping' ? 'Loading the image model' : 'Rendering';
    waitText.textContent = `${what}… ${fmtClock(Date.now() - startedAt)}`;
  }

  function setBusy(on: boolean, label = 'Rendering'): void {
    busy = on;
    wait.hidden = !on;
    clearInterval(timer);
    if (on) {
      startedAt = Date.now();
      waitText.textContent = `${label}…`;
      timer = window.setInterval(updateWait, 1000);
    }
    for (const b of [again, save, copy]) b.disabled = on || !shown;
    // The src carrier needs a non-empty alt (spec: Requirements for the src carrier), and Studio uses the description.
    const noAlt = !!shown && !shown.description;
    if (noAlt) copy.disabled = true;
    copy.title = noAlt ? 'This document has no description to use as the alt text' : '';
  }

  /**
   * A readable document before its first render (spec: Rendering modes): its preview scaled to the
   * panel, and its description and text beside it.
   */
  function showDocument(text: string): void {
    const it = interpret(parse(text).document).interpreted;
    read = { description: it.description, preview: it.preview };
    $('view-desc').textContent = it.description || 'This document has no description.';
    $('view-doc').textContent = text;
    kv.hidden = promptBox.hidden = true;
    details.hidden = false;
    if (!it.preview) return;
    img.removeAttribute('width');
    img.removeAttribute('height');
    img.src = it.preview;
    img.alt = it.description || 'A preview of the image';
    img.classList.add('preview');
    img.hidden = false;
    figure.classList.remove('empty');
  }

  function showDiags(list: Diag[]): void {
    const ul = $('view-diags');
    ul.replaceChildren(...list.map(diagItem));
    $('view-diags-none').hidden = list.length > 0;
    diagBox.hidden = false;
  }

  /** A failed read or render. A failed first render leaves the preview, else the description, in place of the image (spec: Failure). */
  function fail(e: unknown): void {
    const message = errorText(e);
    say(status, message, 'err');
    modelsLink.hidden = !/download the models/i.test(message);
    if (shown) return;
    if (!read) { figure.hidden = true; drop.hidden = false; return; }
    if (!read.preview) { fallback.textContent = read.description || 'This document has no description.'; fallback.hidden = false; }
  }

  async function render(seed: string | undefined, my = ++seq): Promise<void> {
    if (!doc) return;
    setBusy(true);
    say(status, '');
    modelsLink.hidden = true;
    try {
      const r = await api.viewerRender(seed === undefined ? { path: doc.path } : { path: doc.path, seed });
      if (my !== seq) return;
      shown = r;
      img.width = r.width;
      img.height = r.height;
      img.src = r.dataUrl;
      img.alt = r.description || 'The rendered image';
      img.classList.remove('preview');
      img.hidden = false;
      fallback.hidden = true;
      figure.classList.remove('empty');
      $('view-desc').textContent = r.description || 'This document has no description.';
      $('view-seed').textContent = String(r.seed);
      $('view-size').textContent = `${r.width} × ${r.height}`;
      $('view-prompt').textContent = r.sentPrompt;
      $('view-doc').textContent = r.documentText;
      $('view-cached').hidden = !r.cached;
      showDiags(r.diagnostics);
      kv.hidden = promptBox.hidden = false;
      details.hidden = false;
      actions.hidden = false;
    } catch (e) {
      if (my === seq) fail(e);
    } finally {
      if (my === seq) setBusy(false);
      ctx.refreshStatus();
    }
  }

  async function open(path: string): Promise<void> {
    const my = ++seq;
    doc = { path, name: baseName(path) };
    read = null;
    shown = null;
    title.textContent = doc.name;
    meta.textContent = '';
    for (const e of [details, actions, diagBox, modelsLink, fallback]) e.hidden = true;
    drop.hidden = true;
    $('view-more').hidden = false;
    figure.hidden = false;
    figure.classList.add('empty');
    img.hidden = true;
    img.classList.remove('preview');
    img.removeAttribute('src');
    say(status, '');
    setBusy(true, 'Reading the document');
    try {
      const info = await api.readDocument(path);
      if (my !== seq) return;
      doc = { path: info.path || path, name: info.name || doc.name };
      title.textContent = doc.name;
      if (info.validate.fatal) {
        setBusy(false);
        figure.hidden = true;
        drop.hidden = false;
        showDiags(info.validate.diagnostics);
        say(status, 'This is not a readable .jai file, so there is nothing to render.', 'err');
        return;
      }
      showDocument(info.validate.text ?? '');
      await render(undefined, my);
    } catch (e) {
      if (my === seq) { setBusy(false); fail(e); }
    }
  }

  async function openDialog(): Promise<void> {
    try {
      const r = await api.openDialog('jai');
      if (r) void open(r.path);
    } catch (e) {
      toast(`Could not open the file dialog: ${errorText(e)}`, 'err');
    }
  }

  for (const id of ['view-open', 'view-open-2']) $(id).addEventListener('click', () => void openDialog());
  modelsLink.addEventListener('click', () => ctx.show('models', true));
  again.addEventListener('click', () => { if (!busy) void render(String(randomSeed())); });
  save.addEventListener('click', async () => {
    if (!shown || !doc) return;
    const ext = shown.dataUrl.startsWith('data:image/jpeg') ? 'jpg' : 'png';
    try {
      const r = await api.saveFile({ dataUrl: shown.dataUrl, suggestedName: `${stem(doc.name)}.${ext}` });
      if (r) toast(`Saved ${baseName(r.path)}`);
    } catch (e) {
      toast(`Could not save the image: ${errorText(e)}`, 'err');
    }
  });
  copy.addEventListener('click', async () => {
    if (!shown?.description || !doc) return;
    try {
      // The size has the document's aspect when it gives one, so the snippet's box agrees with it (spec: Size and aspect); the render sets the scale.
      const aspect = interpret(parse(shown.documentText).document).interpreted.aspect;
      const height = aspect ? Math.round(shown.width * aspect.height / aspect.width) : shown.height;
      await copyText(`<img src="${attr(encodeURIComponent(doc.name))}" alt="${attr(shown.description)}" width="${shown.width}" height="${height}">`);
      toast('Copied the HTML');
    } catch (e) {
      toast(errorText(e), 'err');
    }
  });
  ctx.onStatus(s => { engines = s; if (busy && Date.now() - startedAt > 400) updateWait(); });

  return { open: path => void open(path), openDialog: () => void openDialog() };
}
