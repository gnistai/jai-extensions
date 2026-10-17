// View tab (studio/DESIGN.md section 11): open or drop a .jai document, render
// it on this PC, and show the image fitted to the panel with the description,
// the exact sent prompt, the diagnostics and the seed; Render again (new seed),
// Save image and Copy HTML.
import { api, engineView, type Diag, type StatusReply, type ViewerRender } from './api.ts';
import { $, baseName, copyText, el, errorText, fmtClock, randomSeed, say, stem, toast } from './dom.ts';
import type { TabContext } from './main.ts';

export interface ViewTab {
  open(path: string): void;
  openDialog(): void;
}

const FORMS: Record<string, string> = { gzip: '.jai file (gzip)', json: 'JSON document', text: 'text document' };

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
  const again = $<HTMLButtonElement>('view-again'), save = $<HTMLButtonElement>('view-save'), copy = $<HTMLButtonElement>('view-copy');
  let doc: { path: string; name: string } | null = null;
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
  }

  function showDiags(list: Diag[]): void {
    const ul = $('view-diags');
    ul.replaceChildren(...list.map(diagItem));
    $('view-diags-none').hidden = list.length > 0;
    diagBox.hidden = false;
  }

  function fail(e: unknown): void {
    const message = errorText(e);
    say(status, message, 'err');
    modelsLink.hidden = !/download the models/i.test(message);
    if (!shown) { figure.hidden = true; drop.hidden = false; }
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
      img.hidden = false;
      figure.classList.remove('empty');
      $('view-desc').textContent = r.description || 'This document has no description.';
      $('view-seed').textContent = String(r.seed);
      $('view-size').textContent = `${r.width} × ${r.height}`;
      $('view-prompt').textContent = r.sentPrompt;
      $('view-doc').textContent = r.documentText;
      $('view-cached').hidden = !r.cached;
      showDiags(r.diagnostics);
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
    shown = null;
    title.textContent = doc.name;
    meta.textContent = '';
    for (const e of [details, actions, diagBox, modelsLink]) e.hidden = true;
    drop.hidden = true;
    $('view-more').hidden = false;
    figure.hidden = false;
    figure.classList.add('empty');
    img.hidden = true;
    img.removeAttribute('src');
    say(status, '');
    setBusy(true, 'Reading the document');
    try {
      const info = await api.readDocument(path);
      if (my !== seq) return;
      doc = { path: info.path || path, name: info.name || doc.name };
      title.textContent = doc.name;
      meta.textContent = info.validate.form ? FORMS[info.validate.form] ?? info.validate.form : '';
      if (info.validate.fatal) {
        setBusy(false);
        figure.hidden = true;
        drop.hidden = false;
        showDiags(info.validate.diagnostics);
        say(status, 'This file cannot be read as a JAI document, so there is nothing to render.', 'err');
        return;
      }
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
    if (!shown || !doc) return;
    try {
      await copyText(`<img src="${attr(encodeURIComponent(doc.name))}" alt="${attr(shown.description)}">`);
      toast('Copied the HTML');
    } catch (e) {
      toast(errorText(e), 'err');
    }
  });
  ctx.onStatus(s => { engines = s; if (busy && Date.now() - startedAt > 400) updateWait(); });

  return { open: path => void open(path), openDialog: () => void openDialog() };
}
