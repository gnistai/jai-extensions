// Content script of the JAI reference viewer.
//
// Discovers JAI documents on the page (spec §Discovery), marks them with a
// small control, and on the user's request renders and replaces the image
// (spec §Replacement) while keeping the original one click away. All UI lives
// in a closed shadow root; every document value is written as text.
import { QUALIFIABLE, canonicalJSON, decode, discover, interpret, serialize, validate, validateDelivery, type Diagnostic, type Discovery, type JaiDocument } from '../jai/index.ts';
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
  interface SavedState { srcset: string | null; sizes: string | null; alt: string | null; ariaDescription: string | null; ariaBusy: string | null; insertedSource: HTMLSourceElement | null; canvas: HTMLCanvasElement | null; display: string; objectFit: string | null; sizeStyle: { width: string; aspectRatio: string } | null }

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
    @media (prefers-color-scheme: dark) { .panel { background: #1c1a16; color: #ece7de; border-color: #2f2b25; } .muted, h3 { color: #a69f93 !important; } pre, .vals span { background: #14120f !important; border-color: #2f2b25 !important; } a, button.act.secondary { color: #fb923c !important; border-color: #fb923c !important; } button.act:not(.secondary) { background: #fb923c !important; border-color: #fb923c !important; color: #1d1b18 !important; } .notice { background: #2a1a10 !important; color: #fdba74 !important; } }
    .sr { position: absolute; width: 1px; height: 1px; overflow: hidden; clip: rect(0 0 0 0); white-space: nowrap; }
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
  // One live region that survives panel rebuilds, so status changes are always announced.
  const announcer = document.createElement('div');
  announcer.className = 'sr';
  announcer.setAttribute('role', 'status');
  shadow.append(announcer);
  const announce = (t: string): void => { announcer.textContent = t; };

  /**
   * Clickjacking guard (spec: Viewer UI hygiene): a consent or paid action counts
   * only if its button has been on screen for a moment and is actually visible,
   * not covered by page content (IntersectionObserver v2 where available).
   */
  function arm(btn: HTMLElement): () => boolean {
    const shownAt = performance.now();
    let visible = false;
    try {
      const o = new IntersectionObserver(es => { for (const e of es) visible = (e as IntersectionObserverEntry & { isVisible?: boolean }).isVisible ?? e.isIntersecting; },
        { threshold: [1], trackVisibility: true, delay: 100 } as IntersectionObserverInit);
      o.observe(btn);
    } catch { visible = true; }
    return () => visible && performance.now() - shownAt > 400;
  }
  ours.add(host);
  document.documentElement.append(host);

  const el = <K extends keyof HTMLElementTagNameMap>(tag: K, props: Partial<HTMLElementTagNameMap[K]> = {}, ...kids: Array<Node | string>): HTMLElementTagNameMap[K] => {
    const e = document.createElement(tag);
    Object.assign(e, props);
    for (const k of kids) e.append(typeof k === 'string' ? document.createTextNode(k) : k);
    return e;
  };

  // ------------------------------------------------------------ discovery
  // .jai resources (picture or src carrier) are fetched on use only: when the user opens the
  // panel or a render starts, never during discovery (spec §Replacement, §Permitted inputs).
  const fetches = new Map<string, Promise<{ bytes: Uint8Array | null; notJai: boolean; error: string | null }>>();
  function fetchJai(url: string): Promise<{ bytes: Uint8Array | null; notJai: boolean; error: string | null }> {
    let p = fetches.get(url);
    if (!p) {
      p = (async () => {
        try {
          const res = await fetch(url, { mode: 'cors', credentials: 'omit', redirect: 'error' });
          if (res.ok) {
            if (/^text\/html\b/i.test(res.headers.get('content-type') ?? '')) return { bytes: null, notJai: true, error: null };
            return { bytes: new Uint8Array(await res.arrayBuffer()), notJai: false, error: null };
          }
        } catch { /* fall back to the background, which applies its own checks */ }
        const r = await send<{ b64?: string; error?: string; notJai?: boolean }>({ type: 'fetchJai', url });
        if (r?.b64) return { bytes: b64decode(r.b64), notJai: false, error: null };
        return { bytes: null, notJai: !!r?.notJai, error: r?.error ?? 'blocked' };
      })();
      fetches.set(url, p);
    }
    return p;
  }

  /** Load the document of an item; false when the resource turned out not to be JAI (the item is dropped quietly). */
  async function resolve(item: Item): Promise<boolean> {
    if (item.doc) return true;
    if (item.disc.kind === 'inline') { item.doc = item.disc.document ?? null; item.diagnostics = item.disc.diagnostics ?? []; return true; }
    item.status = 'loading'; placeBadge(item);
    const url = item.disc.url ?? '';
    const r = await fetchJai(url);
    item.status = 'found';
    if (r.bytes) {
      const d = decode(r.bytes, { inContext: true });
      if (d.diagnostics.some(x => ['undecodable', 'malformed-container', 'invalid-json'].includes(x.code))) { drop(item); return false; }
      item.doc = d.document; item.diagnostics = [...d.diagnostics, ...validateDelivery({ decoded: d })];
      placeBadge(item);
      return true;
    }
    if (r.notJai) { drop(item); return false; }
    item.error = `Could not fetch ${url}: ${r.error}`;
    placeBadge(item);
    return false;
  }

  /** Not a JAI image after all (an HTML page or pixels at a .jai address): remove our control, leave the page alone. */
  function drop(item: Item): void {
    item.badge?.remove();
    if (panel?.isConnected && panelItem === item) panel.remove();
    items.delete(item.img);
    void send({ type: 'found', count: items.size });
  }

  /** Spec §Discovery: a src .jai address that delivered pixels (service worker, gateway) is the page's image. */
  function pixelsArrived(item: Item): boolean {
    return item.disc.carrier === 'src' && item.disc.kind === 'url' && item.img.currentSrc === item.disc.url && item.img.complete && item.img.naturalWidth > 0;
  }

  function scan(): void {
    for (const img of Array.from(document.images)) {
      if (ours.has(img) || items.has(img)) continue;
      const disc = discover(img, { baseURL: document.baseURI });
      if (!disc) continue;
      const item: Item = { img, disc, doc: null, diagnostics: [], status: 'found' };
      items.set(img, item);
      if (disc.kind === 'inline') { item.doc = disc.document ?? null; item.diagnostics = disc.diagnostics ?? []; }
      placeBadge(item);
      maybeAuto(item);
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
    // A src image without pixels may have no size (Markdown output): keep its control reachable.
    const visible = item.img.isConnected && ((r.width >= 24 && r.height >= 16) || item.disc.carrier === 'src');
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
  let panelItem: Item | null = null;
  async function openPanel(item: Item, refocus = false): Promise<void> {
    if (pixelsArrived(item)) { drop(item); return; }
    if (!(await resolve(item)) && !items.has(item.img)) return;
    panelItem = item;
    state = await send<StateReply>({ type: 'state', origin });
    const focusedRole = refocus ? (shadow.activeElement as HTMLElement | null)?.dataset.role ?? null : null;
    panel?.remove();
    const p = el('div', { className: 'panel' });
    p.setAttribute('role', 'dialog'); p.setAttribute('aria-label', 'JAI image');
    panel = p;
    const close = el('button', { className: 'close', type: 'button', textContent: '×' });
    close.setAttribute('aria-label', 'Close');
    close.addEventListener('click', () => { p.remove(); item.badge?.focus(); });
    p.addEventListener('keydown', e => { if (e.key === 'Escape') { p.remove(); item.badge?.focus(); } });
    const carriers: Record<string, string> = { picture: '<picture><source type="image/jai">', attribute: 'alt + data-jai', src: item.disc.kind === 'url' ? 'a .jai file in src' : 'data:image/jai in src' };
    p.append(el('h2', {}, el('span', {}, 'JAI image'), close));
    p.append(el('div', { className: 'muted' }, `Found via ${carriers[item.disc.carrier]} on ${origin}`));
    if (item.error) p.append(el('div', { className: 'notice' }, item.error));
    const doc = item.doc;
    if (doc) {
      const it = interpret(doc).interpreted;
      p.append(el('h3', {}, 'Shows'), el('pre', {}, it.description || '(no description)'));
      const vals = (label: string, v: string[]): void => { if (v.length) { const w = el('div', { className: 'vals' }); v.forEach(x => w.append(el('span', {}, x))); p.append(el('h3', {}, label), w); } };
      vals('Style', it.style); vals('Avoid', it.avoid); vals('Model preferences (advisory)', it.model);
      // Per-model refinements: added only when the render uses a matching model.
      const refinements = [...doc].filter(([k]) => k.includes('/') && QUALIFIABLE.has(k.split('/')[0] ?? '')).map(([k, v]) => `${k.replace('/', ' for ')}: ${v.join(' | ')}`);
      vals('Only for matching models', refinements);
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
        const okArmed = arm(ok);
        ok.addEventListener('click', async e => {
          if (!e.isTrusted) return;
          if (!okArmed()) { announce('Click again: the button was not fully visible.'); return; }
          await send({ type: 'acceptDisclosure', origin }); void openPanel(item);
        });
        notice.append(ok);
        p.append(notice);
        renderBtn.disabled = true;
      }
      const renderArmed = arm(renderBtn);
      renderBtn.dataset.role = 'render'; cancelBtn.dataset.role = 'cancel'; toggleBtn.dataset.role = 'toggle';
      renderBtn.addEventListener('click', e => {
        if (!e.isTrusted) return;
        if (!renderArmed()) { status.textContent = 'Click again: the button was not fully visible.'; return; }
        void render(item, sel.value || null, false, status);
      });
      cancelBtn.addEventListener('click', () => item.port?.postMessage({ type: 'cancel' }));
      toggleBtn.addEventListener('click', () => { if (item.status === 'rendered') restore(item, true); else if (item.render) void apply(item, item.render); void openPanel(item, true); });
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
    const again = focusedRole ? p.querySelector<HTMLElement>(`[data-role="${focusedRole}"]`) : null;
    (again && !(again as HTMLButtonElement).disabled ? again : close).focus();
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
    if (item.status === 'rendering') return;
    if (pixelsArrived(item)) { drop(item); return; }
    if (!(await resolve(item)) || !item.doc) return;
    const it = interpret(item.doc).interpreted;
    // The src carrier has no original pixels (spec §Modes and the original image).
    const hasOriginal = item.disc.carrier !== 'src' && item.img.naturalWidth > 0;
    const input = it.mode === 'transform' && hasOriginal ? await readableInput(item.img) : null;
    const rect = item.img.getBoundingClientRect();
    const req: RenderPortRequest = {
      type: 'render', canonicalJSON: canonicalJSON(item.doc), origin, topSite, pageId, modelId, automatic,
      inputImage: input,
      governingRatio: governingRatio(item.img, hasOriginal),
      target: rect.width ? { width: Math.round(rect.width * devicePixelRatio), height: Math.round(rect.height * devicePixelRatio) } : null,
    };
    item.status = 'rendering'; placeBadge(item);
    item.img.setAttribute('aria-busy', 'true');
    const port = chrome.runtime.connect({ name: 'render' });
    item.port = port;
    const setStatus = (t: string): void => { if (statusEl) statusEl.textContent = t; announce(t); };
    setStatus('Rendering…');
    port.onMessage.addListener((m: RenderPortReply) => {
      if (m.type === 'status') setStatus(`Rendering… (${m.status})`);
      else if (m.type === 'done') { item.render = m; void apply(item, m).then(() => { announce(statusText(item)); if (panel?.isConnected && statusEl) void openPanel(item, true); }); port.disconnect(); }
      else { item.status = 'failed'; item.error = m.message; restore(item, false); setStatus(m.message); placeBadge(item); port.disconnect(); }
    });
    port.onDisconnect.addListener(() => { item.port = undefined; if (item.status === 'rendering') { item.status = 'failed'; restore(item, false); placeBadge(item); } });
    port.postMessage(req);
  }

  /** Spec §Size and aspect: an author-sized box (width+height or CSS aspect-ratio) first, else the original's ratio. */
  function governingRatio(img: HTMLImageElement, hasOriginal: boolean): number | null {
    const w = Number(img.getAttribute('width')), h = Number(img.getAttribute('height'));
    if (w > 0 && h > 0) return w / h;
    const ar = getComputedStyle(img).aspectRatio; // "16 / 9", "auto 4 / 3", "1.5" or "auto"
    // "auto w / h" comes from the image's own size, not from the author; only an explicit ratio counts.
    const m = /^\s*([0-9.]+)\s*(?:\/\s*([0-9.]+))?\s*$/.exec(ar);
    if (m) { const a = Number(m[1]), b = m[2] ? Number(m[2]) : 1; if (a > 0 && b > 0) return a / b; }
    if (hasOriginal && img.naturalWidth && img.naturalHeight) return img.naturalWidth / img.naturalHeight;
    return null;
  }

  function save(img: HTMLImageElement): SavedState {
    return { srcset: img.getAttribute('srcset'), sizes: img.getAttribute('sizes'), alt: img.getAttribute('alt'), ariaDescription: img.getAttribute('aria-description'), ariaBusy: img.getAttribute('aria-busy'), insertedSource: null, canvas: null, display: img.style.display, objectFit: null, sizeStyle: null };
  }

  async function apply(item: Item, r: Extract<RenderPortReply, { type: 'done' }>): Promise<void> {
    const img = item.img;
    item.saved ??= save(img);
    const saved = item.saved;
    const picture = img.parentElement?.localName === 'picture' ? img.parentElement : null;
    // A src image with no size (for example Markdown output) gets the document's shape at the available width.
    const box = img.getBoundingClientRect();
    if (item.disc.carrier === 'src' && (box.width < 2 || box.height < 2) && saved.sizeStyle === null) {
      saved.sizeStyle = { width: img.style.width, aspectRatio: img.style.aspectRatio };
      const a = item.doc ? interpret(item.doc).interpreted.aspect : null;
      img.style.width = '100%';
      img.style.aspectRatio = a ? `${a.width} / ${a.height}` : '1 / 1';
    }
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
      // Only an error about our own image counts; a late error from the original .jai load does not.
      const onErr = (): void => { if (img.currentSrc === r.dataURL) done(false); };
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
        // Cover-crop, centred: never stretch (spec §Size and aspect).
        const scale = Math.max(c.width / bmp.width, c.height / bmp.height);
        const sw = c.width / scale, sh = c.height / scale;
        c.getContext('2d')?.drawImage(bmp, (bmp.width - sw) / 2, (bmp.height - sh) / 2, sw, sh, 0, 0, c.width, c.height);
        c.setAttribute('role', 'img');
        img.after(c);
        img.style.display = 'none';
        saved.canvas = c;
        describe(c, item);
      } catch { item.status = 'failed'; item.error = 'This page does not allow replacement images.'; placeBadge(item); return; }
    } else {
      describe(img, item);
      // Never stretch (spec §Size and aspect): honour the page's object-fit, else cover-crop.
      if (getComputedStyle(img).objectFit === 'fill' && saved.objectFit === null) {
        saved.objectFit = img.style.objectFit;
        img.style.objectFit = 'cover';
      }
    }
    img.removeAttribute('aria-busy');
    item.status = 'rendered';
    placeBadge(item);
  }

  /** Accessible name and the AI-rendered state (spec §Accessibility). */
  function describe(target: Element, item: Item): void {
    const saved = item.saved;
    const noAlt = saved?.alt === null;
    // Spec §Accessibility: no alt → the description, in full (no length limit), control characters removed.
    const name = noAlt ? ((item.doc?.get('prompt') ?? []).join('\n') || 'Image').replace(/[\u0000-\u0009\u000b-\u001f\u007f]/g, ' ') : saved?.alt ?? '';
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
    if (saved.objectFit !== null) img.style.objectFit = saved.objectFit;
    if (saved.sizeStyle) { img.style.width = saved.sizeStyle.width; img.style.aspectRatio = saved.sizeStyle.aspectRatio; }
    if (saved.alt === null) img.removeAttribute('alt'); else img.setAttribute('alt', saved.alt);
    if (saved.ariaDescription === null) img.removeAttribute('aria-description'); else img.setAttribute('aria-description', saved.ariaDescription);
    item.saved = undefined;
    if (keepRender) item.status = 'original';
    placeBadge(item);
  }

  // ------------------------------------------------------------ automatic mode (per-site opt-in, budgets in background)
  let autoOn = false;
  // Spec (Consent, budgets and cancellation): elements not displayed to the user MUST NOT be rendered.
  // So: inside the viewport (no margin), visible and not covered (IntersectionObserver v2 where available),
  // not transparent or hidden, and at least the configured size.
  const io = new IntersectionObserver(entries => {
    for (const e of entries) {
      const v = (e as IntersectionObserverEntry & { isVisible?: boolean }).isVisible;
      if (!e.isIntersecting || v === false) continue;
      const img = e.target as HTMLImageElement;
      const item = items.get(img);
      if (!item || item.status !== 'found' || item.render) continue;
      const min = state?.minDisplayedPx ?? 48;
      const r = e.intersectionRect;
      if (r.width < min || r.height < min) continue;
      if (typeof img.checkVisibility === 'function' && !img.checkVisibility({ opacityProperty: true, visibilityProperty: true } as CheckVisibilityOptions)) continue;
      io.unobserve(img);
      // A .jai resource is fetched now: the image is on screen and the user enabled automatic rendering for this site.
      void (async () => {
        if (!(await resolve(item)) || !item.doc) return;
        if (interpret(item.doc).interpreted.mode === 'keep') return; // never automatic for keep
        await render(item, null, true);
      })();
    }
  }, { rootMargin: '0px', threshold: [0.5], trackVisibility: true, delay: 100 } as IntersectionObserverInit);
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
    renderAll: () => { for (const i of items.values()) if (i.status === 'found') void render(i, null, false); },
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
