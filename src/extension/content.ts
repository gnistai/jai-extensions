// Content script of the JAI reference viewer.
//
// Discovers JAI documents on the page (spec §Discovery), marks them with a
// small control, and on the user's request renders and replaces the image
// (spec §Replacement) while keeping the original one click away. All UI lives
// in a closed shadow root; every document value is written as text.
import { canonicalJSON, decode, discover, interpret, serialize, validate, validateDelivery, type Diagnostic, type Discovery, type JaiDocument } from '../jai/index.ts';
import { b64decode, b64encode, type RenderPortReply, type RenderPortRequest, type StateReply } from './shared.ts';

declare global { interface Window { __jaiViewer?: { rescan(): void; list(): unknown[]; renderAll(): void; restoreAll(): void } } }

if (!window.__jaiViewer) init();

function init(): void {
  const origin = location.origin;
  const topSite = (() => { try { return window.top?.location.origin ?? origin; } catch { return 'cross-origin-frame'; } })();
  const pageId = crypto.randomUUID();
  const send = <T>(msg: unknown): Promise<T> => chrome.runtime.sendMessage(msg) as Promise<T>;

  interface Item {
    img: HTMLImageElement;
    disc: Discovery;
    doc: JaiDocument | null;
    diagnostics: Diagnostic[];
    error?: string;
    status: 'found' | 'loading' | 'rendering' | 'rendered' | 'failed' | 'original';
    port?: chrome.runtime.Port;
    saved?: SavedState;
    render?: Extract<RenderPortReply, { type: 'done' }>;
    badge?: HTMLButtonElement;
  }
  interface SavedState { srcset: string | null; sizes: string | null; alt: string | null; ariaDescription: string | null; ariaBusy: string | null; insertedSource: HTMLSourceElement | null; canvas: HTMLCanvasElement | null; display: string }

  const items = new Map<HTMLImageElement, Item>();
  const ours = new WeakSet<Element>();
  let state: StateReply | null = null;

  // ------------------------------------------------------------ overlay UI (closed shadow root)
  const host = document.createElement('jai-viewer-ui');
  host.style.cssText = 'position:absolute;top:0;left:0;width:0;height:0;z-index:2147483646;';
  const shadow = host.attachShadow({ mode: 'closed' });
  const sheet = new CSSStyleSheet();
  sheet.replaceSync(`
    :host { all: initial; }
    * { box-sizing: border-box; font-family: system-ui, -apple-system, "Segoe UI", sans-serif; }
    .badge { position: absolute; font: 600 11px/1 ui-monospace, monospace; padding: 4px 6px; border-radius: 6px; border: 1px solid rgba(255,255,255,.6); background: #c2410c; color: #fff; cursor: pointer; box-shadow: 0 1px 4px rgba(0,0,0,.35); }
    .badge[data-status="rendered"] { background: #15803d; }
    .badge[data-status="rendering"], .badge[data-status="loading"] { background: #1d4ed8; }
    .badge[data-status="failed"] { background: #b91c1c; }
    .badge:focus-visible { outline: 2px solid #fff; outline-offset: 2px; }
    .panel { position: fixed; right: 16px; top: 16px; width: min(420px, calc(100vw - 32px)); max-height: calc(100vh - 32px); overflow: auto; background: #fffdf9; color: #1d1b18; border: 1px solid #e3ddd2; border-radius: 14px; box-shadow: 0 12px 40px rgba(0,0,0,.25); padding: 14px 16px; font-size: 13px; line-height: 1.45; }
    @media (prefers-color-scheme: dark) { .panel { background: #1c1a16; color: #ece7de; border-color: #2f2b25; } .muted { color: #a69f93 !important; } pre, .vals span { background: #14120f !important; border-color: #2f2b25 !important; } }
    h2 { font-size: 15px; margin: 0 0 8px; display: flex; justify-content: space-between; align-items: center; gap: 8px; }
    h3 { font-size: 11px; text-transform: uppercase; letter-spacing: .06em; color: #5f5a52; margin: 12px 0 4px; }
    pre { background: #f4f1ea; border: 1px solid #e3ddd2; border-radius: 8px; padding: 8px; white-space: pre-wrap; overflow-wrap: anywhere; margin: 0; font: 12px/1.45 ui-monospace, monospace; unicode-bidi: plaintext; }
    .muted { color: #5f5a52; }
    .row { display: flex; gap: 8px; flex-wrap: wrap; margin-top: 10px; align-items: center; }
    button.act { border: 1px solid #c2410c; background: #c2410c; color: #fff; padding: 6px 12px; border-radius: 8px; cursor: pointer; font-size: 13px; font-weight: 600; }
    button.act.secondary { background: transparent; color: #c2410c; }
    button.act:disabled { opacity: .5; cursor: default; }
    button.close { border: 0; background: transparent; font-size: 18px; cursor: pointer; color: inherit; }
    select { font: inherit; padding: 4px; border-radius: 6px; max-width: 100%; }
    ul.diag { list-style: none; padding: 0; margin: 0; display: grid; gap: 4px; }
    ul.diag li { font: 12px ui-monospace, monospace; border-left: 3px solid #b45309; padding-left: 6px; }
    .vals { display: flex; flex-wrap: wrap; gap: 4px; }
    .vals span { background: #f4f1ea; border: 1px solid #e3ddd2; border-radius: 6px; padding: 0 6px; unicode-bidi: isolate; }
    .notice { background: #fdeee4; color: #9a3412; border-radius: 8px; padding: 8px 10px; margin-top: 10px; }
    .status { margin-top: 8px; font-weight: 600; }
    a { color: #c2410c; }
  `);
  shadow.adoptedStyleSheets = [sheet];
  const layer = document.createElement('div');
  shadow.append(layer);
  ours.add(host);
  document.documentElement.append(host);

  const el = <K extends keyof HTMLElementTagNameMap>(tag: K, props: Partial<HTMLElementTagNameMap[K]> = {}, ...kids: Array<Node | string>): HTMLElementTagNameMap[K] => {
    const e = document.createElement(tag);
    Object.assign(e, props);
    for (const k of kids) e.append(typeof k === 'string' ? document.createTextNode(k) : k);
    return e;
  };

  // ------------------------------------------------------------ discovery
  async function resolve(item: Item): Promise<void> {
    if (item.disc.kind === 'inline') { item.doc = item.disc.document ?? null; item.diagnostics = item.disc.diagnostics ?? []; return; }
    item.status = 'loading';
    const url = item.disc.url ?? '';
    let bytes: Uint8Array | null = null;
    try {
      const res = await fetch(url, { mode: 'cors', credentials: 'omit' });
      if (res.ok) bytes = new Uint8Array(await res.arrayBuffer());
    } catch { /* fall back */ }
    if (!bytes) {
      const r = await send<{ b64?: string; error?: string }>({ type: 'fetchJai', url });
      if (r?.b64) bytes = b64decode(r.b64);
      else item.error = `Could not fetch ${url}: ${r?.error ?? 'blocked'}`;
    }
    if (bytes) {
      const d = decode(bytes, { inContext: true });
      item.doc = d.document; item.diagnostics = [...d.diagnostics, ...validateDelivery({ decoded: d })];
    }
    item.status = 'found';
  }

  function scan(): void {
    for (const img of Array.from(document.images)) {
      if (ours.has(img) || items.has(img)) continue;
      const disc = discover(img, { baseURL: document.baseURI });
      if (!disc) continue;
      const item: Item = { img, disc, doc: null, diagnostics: [], status: 'found' };
      items.set(img, item);
      void resolve(item).then(() => { placeBadge(item); maybeAuto(item); });
      placeBadge(item);
    }
    void send({ type: 'found', count: items.size });
  }

  // ------------------------------------------------------------ badges
  function placeBadge(item: Item): void {
    if (!item.badge) {
      const b = el('button', { className: 'badge', type: 'button', textContent: 'JAI' });
      b.addEventListener('click', e => { if (e.isTrusted) openPanel(item); });
      layer.append(b);
      item.badge = b;
    }
    const b = item.badge;
    const prompt = item.doc?.get('prompt')?.[0] ?? '';
    b.dataset.status = item.status;
    b.setAttribute('aria-label', `JAI image${prompt ? ': ' + prompt.slice(0, 80) : ''}. Open JAI controls. Status: ${item.status}`);
    const r = (item.saved?.canvas ?? item.img).getBoundingClientRect();
    const visible = r.width >= 24 && r.height >= 16 && item.img.isConnected;
    b.hidden = !visible;
    b.style.left = `${r.right + window.scrollX - 40}px`;
    b.style.top = `${r.top + window.scrollY + 6}px`;
  }
  let raf = 0;
  const reposition = (): void => { cancelAnimationFrame(raf); raf = requestAnimationFrame(() => items.forEach(placeBadge)); };
  addEventListener('scroll', reposition, { passive: true });
  addEventListener('resize', reposition);

  // ------------------------------------------------------------ panel
  let panel: HTMLElement | null = null;
  async function openPanel(item: Item): Promise<void> {
    state = await send<StateReply>({ type: 'state', origin });
    panel?.remove();
    const p = el('div', { className: 'panel' });
    p.setAttribute('role', 'dialog'); p.setAttribute('aria-label', 'JAI image');
    panel = p;
    const close = el('button', { className: 'close', type: 'button', textContent: '×' });
    close.setAttribute('aria-label', 'Close');
    close.addEventListener('click', () => { p.remove(); item.badge?.focus(); });
    p.addEventListener('keydown', e => { if (e.key === 'Escape') { p.remove(); item.badge?.focus(); } });
    const carriers = { 1: '<picture><source type="image/jai">', 2: 'data-jai', 3: 'data:image/jai in src', 4: 'jai: in alt (deprecated)' };
    p.append(el('h2', {}, el('span', {}, 'JAI image'), close));
    p.append(el('div', { className: 'muted' }, `Found via ${carriers[item.disc.carrier]} on ${origin}`));
    if (item.error) p.append(el('div', { className: 'notice' }, item.error));
    const doc = item.doc;
    if (doc) {
      const it = interpret(doc).interpreted;
      p.append(el('h3', {}, 'Shows'), el('pre', {}, it.description || '(no description)'));
      const vals = (label: string, v: string[]): void => { if (v.length) { const w = el('div', { className: 'vals' }); v.forEach(x => w.append(el('span', {}, x))); p.append(el('h3', {}, label), w); } };
      vals('Style', it.style); vals('Avoid', it.avoid); vals('Model preferences (advisory)', it.model);
      const settings: string[] = [];
      if (it.aspect) settings.push(`aspect ${it.aspect.width}:${it.aspect.height}`);
      if (doc.has('mode')) settings.push(`mode ${it.mode}`);
      if (it.strength !== null) settings.push(`strength ${it.strength}`);
      if (it.seed) settings.push(`seed ${it.seed}`);
      vals('Settings', settings);
      p.append(el('h3', {}, 'Canonical JAI'), el('pre', {}, serialize(doc)));
      const diags = [...item.diagnostics, ...validate(doc, item.disc.info), ...validateDelivery({ carrier: item.disc.carrier, element: item.img, decoded: { document: doc } })];
      if (diags.length) {
        const ul = el('ul', { className: 'diag' });
        for (const d of diags) ul.append(el('li', {}, d.code + (d.key ? ':' + d.key : '')));
        p.append(el('h3', {}, 'Diagnostics'), ul);
      }
      // Model choice: only models the user enabled (selectors, never locators).
      const sel = el('select');
      const enabled = state.models.filter(m => m.enabled);
      sel.append(new Option('Best match for the document', ''));
      for (const m of enabled) sel.append(new Option(`${m.label}${m.imageInput ? ' · image input' : ''} · ~$${m.costHint}`, m.id));
      p.append(el('h3', {}, 'Renderer'), sel);
      const status = el('div', { className: 'status', textContent: statusText(item) });
      status.setAttribute('aria-live', 'polite');
      p.append(status);
      const row = el('div', { className: 'row' });
      const renderBtn = el('button', { className: 'act', type: 'button', textContent: item.status === 'rendered' ? 'Render again' : 'Render' });
      const cancelBtn = el('button', { className: 'act secondary', type: 'button', textContent: 'Cancel', disabled: item.status !== 'rendering' });
      const toggleBtn = el('button', { className: 'act secondary', type: 'button', textContent: item.status === 'rendered' ? 'Show original' : 'Show render', disabled: !item.render });
      row.append(renderBtn, cancelBtn, toggleBtn);
      p.append(row);
      if (!state.hasToken) {
        const a = el('a', { href: '#', textContent: 'Add your Replicate token' });
        a.addEventListener('click', e => { e.preventDefault(); void send({ type: 'openOptions' }); });
        p.append(el('div', { className: 'notice' }, a, ' to render. Without it, JAI images stay as they are.'));
        renderBtn.disabled = true;
      } else if (!state.disclosed) {
        const notice = el('div', { className: 'notice' },
          'Rendering sends this description', it.mode === 'transform' ? ' and the image shown here' : '', ` to Replicate under your account. It costs money (about $${enabled[0]?.costHint ?? 0.003} per image) and Replicate's terms apply. `,
          el('br'));
        const ok = el('button', { className: 'act', type: 'button', textContent: `Allow on ${origin}` });
        ok.addEventListener('click', async e => { if (!e.isTrusted) return; await send({ type: 'acceptDisclosure', origin }); void openPanel(item); });
        notice.append(ok);
        p.append(notice);
        renderBtn.disabled = true;
      }
      renderBtn.addEventListener('click', e => { if (e.isTrusted) void render(item, sel.value || null, false, status); });
      cancelBtn.addEventListener('click', () => item.port?.postMessage({ type: 'cancel' }));
      toggleBtn.addEventListener('click', () => { if (item.status === 'rendered') restore(item, true); else if (item.render) void apply(item, item.render); void openPanel(item); });
      if (item.render) {
        p.append(el('h3', {}, 'Sent to the model'), el('pre', {}, `${item.render.modelLabel}${item.render.cached ? ' (cached)' : ` · ${item.render.seconds}s`}\n\n${item.render.sentPrompt}${item.render.sentNegative ? `\n\nNegative prompt: ${item.render.sentNegative}` : ''}`));
        if (item.render.diagnostics.length) {
          const ul = el('ul', { className: 'diag' });
          for (const d of item.render.diagnostics) ul.append(el('li', {}, d.code + (d.key ? ':' + d.key : '') + (d.actual ? ` → ${d.actual}` : '')));
          p.append(el('h3', {}, 'What the renderer could not honour'), ul);
        }
      }
    }
    layer.append(p);
    close.focus();
  }

  function statusText(item: Item): string {
    return { found: 'Not rendered. The page shows its own image.', loading: 'Loading the .jai file…', rendering: 'Rendering…', rendered: 'Showing an AI render of the description.', failed: 'Render failed; the original is shown.', original: 'Showing the original (render kept).' }[item.status];
  }

  // ------------------------------------------------------------ input image (only pixels the page can read)
  async function readableInput(img: HTMLImageElement): Promise<{ b64: string; mime: string } | null> {
    if (!img.complete || !img.naturalWidth) return null;
    try {
      const scale = Math.min(1, 2048 / Math.max(img.naturalWidth, img.naturalHeight));
      const c = document.createElement('canvas');
      c.width = Math.round(img.naturalWidth * scale); c.height = Math.round(img.naturalHeight * scale);
      const ctx = c.getContext('2d');
      if (!ctx) return null;
      ctx.drawImage(img, 0, 0, c.width, c.height);
      // toBlob throws SecurityError on a tainted canvas: cross-origin pixels without CORS are never used.
      const blob = await new Promise<Blob | null>((res, rej) => { try { c.toBlob(res, 'image/webp', 0.9); } catch (e) { rej(e); } });
      if (!blob) return null;
      return { b64: b64encode(new Uint8Array(await blob.arrayBuffer())), mime: blob.type || 'image/webp' };
    } catch { return null; }
  }

  // ------------------------------------------------------------ render and replace
  async function render(item: Item, modelId: string | null, automatic: boolean, statusEl?: HTMLElement): Promise<void> {
    if (!item.doc || item.status === 'rendering') return;
    const it = interpret(item.doc).interpreted;
    const hasOriginal = item.disc.carrier !== 3 && item.img.naturalWidth > 0;
    const input = it.mode === 'transform' && hasOriginal ? await readableInput(item.img) : null;
    const rect = item.img.getBoundingClientRect();
    const req: RenderPortRequest = {
      type: 'render', canonicalJSON: canonicalJSON(item.doc), origin, topSite, pageId, modelId, automatic,
      inputImage: input,
      originalRatio: hasOriginal ? item.img.naturalWidth / item.img.naturalHeight : null,
      target: rect.width ? { width: Math.round(rect.width * devicePixelRatio), height: Math.round(rect.height * devicePixelRatio) } : null,
    };
    item.status = 'rendering'; placeBadge(item);
    item.img.setAttribute('aria-busy', 'true');
    const port = chrome.runtime.connect({ name: 'render' });
    item.port = port;
    const setStatus = (t: string): void => { if (statusEl) statusEl.textContent = t; };
    setStatus('Rendering…');
    port.onMessage.addListener((m: RenderPortReply) => {
      if (m.type === 'status') setStatus(`Rendering… (${m.status})`);
      else if (m.type === 'done') { item.render = m; void apply(item, m).then(() => { if (panel?.isConnected && statusEl) void openPanel(item); }); port.disconnect(); }
      else { item.status = 'failed'; item.error = m.message; restore(item, false); setStatus(m.message); placeBadge(item); port.disconnect(); }
    });
    port.onDisconnect.addListener(() => { item.port = undefined; if (item.status === 'rendering') { item.status = 'failed'; restore(item, false); placeBadge(item); } });
    port.postMessage(req);
  }

  function save(img: HTMLImageElement): SavedState {
    return { srcset: img.getAttribute('srcset'), sizes: img.getAttribute('sizes'), alt: img.getAttribute('alt'), ariaDescription: img.getAttribute('aria-description'), ariaBusy: img.getAttribute('aria-busy'), insertedSource: null, canvas: null, display: img.style.display };
  }

  async function apply(item: Item, r: Extract<RenderPortReply, { type: 'done' }>): Promise<void> {
    const img = item.img;
    item.saved ??= save(img);
    const saved = item.saved;
    const picture = img.parentElement?.localName === 'picture' ? img.parentElement : null;
    // Spec §Replacement: setting src alone is a no-op under srcset or a matching <source>.
    if (picture) {
      if (!saved.insertedSource) {
        const s = document.createElement('source');
        ours.add(s);
        picture.insertBefore(s, picture.firstChild);
        saved.insertedSource = s;
      }
      saved.insertedSource.srcset = r.dataURL;
    } else {
      img.removeAttribute('sizes');
      img.setAttribute('srcset', r.dataURL);
    }
    const loaded = await new Promise<boolean>(res => {
      if (img.currentSrc === r.dataURL && img.complete && img.naturalWidth > 0) return res(true);
      const done = (ok: boolean): void => { img.removeEventListener('load', onLoad); img.removeEventListener('error', onErr); res(ok); };
      const onLoad = (): void => done(img.currentSrc === r.dataURL);
      const onErr = (): void => done(false);
      img.addEventListener('load', onLoad); img.addEventListener('error', onErr);
      setTimeout(() => done(img.currentSrc === r.dataURL && img.naturalWidth > 0), 4000);
    });
    if (!loaded) {
      // The page's CSP refused data: images. Paint into a canvas instead: no page-initiated fetch.
      undoReplacement(img, saved);
      try {
        const bmp = await createImageBitmap(new Blob([b64decode(r.dataURL.split(',')[1] ?? '')], { type: r.mime }));
        const rect = img.getBoundingClientRect();
        const c = document.createElement('canvas');
        ours.add(c);
        c.width = Math.round(rect.width * devicePixelRatio) || bmp.width; c.height = Math.round(rect.height * devicePixelRatio) || bmp.height;
        c.style.cssText = `width:${rect.width}px;height:${rect.height}px;display:inline-block;vertical-align:${getComputedStyle(img).verticalAlign}`;
        c.getContext('2d')?.drawImage(bmp, 0, 0, c.width, c.height);
        c.setAttribute('role', 'img');
        img.after(c);
        img.style.display = 'none';
        saved.canvas = c;
        describe(c, item);
      } catch { item.status = 'failed'; item.error = 'This page does not allow replacement images.'; placeBadge(item); return; }
    } else describe(img, item);
    img.removeAttribute('aria-busy');
    item.status = 'rendered';
    placeBadge(item);
  }

  /** Accessible name and the AI-rendered state (spec §Accessibility). */
  function describe(target: Element, item: Item): void {
    const saved = item.saved;
    const altWasJai = item.disc.carrier === 4 || saved?.alt === null;
    const name = altWasJai ? (item.doc?.get('prompt')?.[0] ?? 'Image').replace(/[\u0000-\u001f\u007f]/g, ' ').slice(0, 250) : saved?.alt ?? '';
    target.setAttribute(target === item.img ? 'alt' : 'aria-label', name);
    target.setAttribute('aria-description', 'Rendered by an AI model from its description; it may differ from the publisher’s image. The original is available from the JAI control.');
  }

  function undoReplacement(img: HTMLImageElement, saved: SavedState): void {
    saved.insertedSource?.remove(); saved.insertedSource = null;
    if (saved.srcset === null) img.removeAttribute('srcset'); else img.setAttribute('srcset', saved.srcset);
    if (saved.sizes === null) img.removeAttribute('sizes'); else img.setAttribute('sizes', saved.sizes);
  }

  function restore(item: Item, keepRender: boolean): void {
    const img = item.img, saved = item.saved;
    img.removeAttribute('aria-busy');
    if (!saved) return;
    undoReplacement(img, saved);
    saved.canvas?.remove(); saved.canvas = null;
    img.style.display = saved.display;
    if (saved.alt === null) img.removeAttribute('alt'); else img.setAttribute('alt', saved.alt);
    if (saved.ariaDescription === null) img.removeAttribute('aria-description'); else img.setAttribute('aria-description', saved.ariaDescription);
    item.saved = undefined;
    if (keepRender) item.status = 'original';
    placeBadge(item);
  }

  // ------------------------------------------------------------ automatic mode (per-site opt-in, budgets in background)
  let autoOn = false;
  const io = new IntersectionObserver(entries => {
    for (const e of entries) {
      if (!e.isIntersecting) continue;
      const item = items.get(e.target as HTMLImageElement);
      if (!item || !item.doc || item.status !== 'found' || item.render) continue;
      const it = interpret(item.doc).interpreted;
      if (it.mode === 'keep' || item.disc.carrier === 4) continue; // never automatic for keep or the alt carrier
      const r = e.boundingClientRect;
      if (r.width < 48 || r.height < 48) continue;
      io.unobserve(e.target);
      void render(item, null, true);
    }
  }, { rootMargin: '200px' });
  function maybeAuto(item: Item): void { if (autoOn) io.observe(item.img); }

  // ------------------------------------------------------------ mutations: new images, external src changes
  const mo = new MutationObserver(muts => {
    let rescan = false;
    for (const m of muts) {
      if (m.type === 'childList') { for (const n of Array.from(m.addedNodes)) if (n instanceof Element && !ours.has(n) && (n.localName === 'img' || n.querySelector?.('img'))) rescan = true; }
      else if (m.type === 'attributes' && m.target instanceof HTMLImageElement) {
        const item = items.get(m.target);
        // The page changed a replaced element: treat it as new and release saved state.
        if (item && item.status === 'rendered' && m.attributeName === 'src') { item.saved = undefined; items.delete(m.target); item.badge?.remove(); rescan = true; }
        else if (!item && ['data-jai', 'alt', 'src'].includes(m.attributeName ?? '')) rescan = true;
      }
    }
    if (rescan) scan();
  });
  mo.observe(document.documentElement, { childList: true, subtree: true, attributes: true, attributeFilter: ['src', 'srcset', 'data-jai', 'alt', 'type', 'media'] });

  window.__jaiViewer = {
    rescan: scan,
    list: () => [...items.values()].map(i => ({ carrier: i.disc.carrier, status: i.status, jai: i.doc ? serialize(i.doc) : null, error: i.error ?? null })),
    renderAll: () => { for (const i of items.values()) if (i.status === 'found' && i.doc) void render(i, null, false); },
    restoreAll: () => { for (const i of items.values()) if (i.status === 'rendered') restore(i, true); },
  };
  chrome.runtime.onMessage.addListener((msg: { type: string }, _s, reply) => {
    const v = window.__jaiViewer!;
    if (msg.type === 'list') { v.rescan(); reply(v.list()); }
    else if (msg.type === 'renderAll') { v.renderAll(); reply(true); }
    else if (msg.type === 'restoreAll') { v.restoreAll(); reply(true); }
    return false;
  });

  scan();
  void send<StateReply>({ type: 'state', origin }).then(s => {
    state = s;
    if (s.mode === 'never') { for (const i of items.values()) i.badge?.remove(); mo.disconnect(); return; }
    if (s.mode === 'auto' && s.hasToken && s.disclosed && window === window.top) { autoOn = true; for (const i of items.values()) maybeAuto(i); }
  });
}
