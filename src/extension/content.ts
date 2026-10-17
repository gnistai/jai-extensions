// Content script of the JAI reference viewer.
//
// Discovers JAI documents on the page (spec §Discovery), marks them with a
// small control, and on the user's request renders and replaces the image
// (spec §Replacement) while keeping the original one click away. All UI lives
// in a closed shadow root; every document value is written as text.
import { QUALIFIABLE, canonicalJSON, decode, discover, discoverAll, firstSrcsetURL, interpret, parse, serialize, validate, validateDelivery, type Diagnostic, type Discovery, type ElementLike, type JaiDocument } from '../jai/index.ts';
import { imageSize, previewRatio } from '../renderers/image-size.ts';
import { MAX_JAI_BYTES, b64decode, b64encode, readCapped, type RenderPortReply, type RenderPortRequest, type StateReply } from './shared.ts';

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
    /** The page's own aria-busy, kept while the viewer marks a render pending (undefined otherwise). */
    busy?: string | null;
  }
  interface SavedState {
    srcset: string | null; sizes: string | null; alt: string | null; ariaDescription: string | null; insertedSource: HTMLSourceElement | null; canvas: HTMLCanvasElement | null; display: string; objectFit: string | null; sizeStyle: { width: string; aspectRatio: string } | null;
    /** The delivery advisories of the element as the page had it, before the viewer's first write. */
    advisories: Diagnostic[];
  }

  const items = new Map<HTMLImageElement, Item>();
  /** Whether an item is still the one held for its element (discovery may have re-run during an await). */
  const live = (item: Item): boolean => items.get(item.img) === item;
  /** Per element, the documents (canonical JSON, from which identity is computed) it has been replaced with. */
  const replacedWith = new WeakMap<HTMLImageElement, Set<string>>();
  /** Per element, the discovery for which the viewer removed its control for good: no document, or the viewer stood down. */
  const dismissed = new WeakMap<HTMLImageElement, Discovery>();
  let state: StateReply | null = null;
  let reported = -1;
  const report = (): void => { if (items.size !== reported) { reported = items.size; void send({ type: 'found', count: items.size }); } };

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
  document.documentElement.append(host);

  const el = <K extends keyof HTMLElementTagNameMap>(tag: K, props: Partial<HTMLElementTagNameMap[K]> = {}, ...kids: Array<Node | string>): HTMLElementTagNameMap[K] => {
    const e = document.createElement(tag);
    Object.assign(e, props);
    for (const k of kids) e.append(typeof k === 'string' ? document.createTextNode(k) : k);
    return e;
  };

  // ------------------------------------------------------------ discovery
  // A resource named by the picture or src carrier is fetched on use only (when the user opens
  // the panel or a render starts, never during discovery) and at most once per page (spec
  // §Permitted inputs). The background picks the one route before anything is sent: it fetches a
  // cross-origin .jai resource itself where the user granted host permission; otherwise it
  // declines without fetching, and the content script fetches with the page's own privileges.
  type Fetched = { bytes: Uint8Array | null; notJai: boolean; error: string | null };
  const fetches = new Map<string, Promise<Fetched>>();
  function fetchJai(address: string): Promise<Fetched> {
    // One resource per URL without its fragment, which never goes over the network.
    const url = address.split('#')[0] ?? address;
    let p = fetches.get(url);
    if (!p) {
      p = (async (): Promise<Fetched> => {
        try {
          const r = await send<{ b64?: string; error?: string; notJai?: boolean; page?: boolean } | undefined>({ type: 'fetchJai', url });
          if (!r?.page) return typeof r?.b64 === 'string' ? { bytes: b64decode(r.b64), notJai: false, error: null } : { bytes: null, notJai: !!r?.notJai, error: r?.error ?? 'blocked' };
          const res = await fetch(url, { mode: 'cors', credentials: 'omit', redirect: 'error' });
          if (!res.ok) { void res.body?.cancel(); return { bytes: null, notJai: false, error: `HTTP ${res.status}` }; }
          if (/^text\/html\b/i.test(res.headers.get('content-type') ?? '')) { void res.body?.cancel(); return { bytes: null, notJai: true, error: null }; }
          // The same bound as the background's route.
          const bytes = await readCapped(res, MAX_JAI_BYTES);
          return bytes ? { bytes, notJai: false, error: null } : { bytes: null, notJai: false, error: 'Too large' };
        } catch (e) { return { bytes: null, notJai: false, error: String((e as Error)?.message ?? e) }; }
      })();
      fetches.set(url, p);
    }
    return p;
  }

  /**
   * Load an item's document. False when there is none to use: the resource carries no document
   * (the item is dropped quietly), the fetch failed (the error is shown in the panel), or
   * discovery re-ran for the element meanwhile (the item is gone).
   */
  async function resolve(item: Item): Promise<boolean> {
    if (item.doc) return true;
    if (item.disc.kind === 'inline') { item.doc = item.disc.document ?? null; item.diagnostics = item.disc.diagnostics ?? []; return true; }
    item.status = 'loading'; placeBadge(item);
    const url = item.disc.url ?? '';
    const r = await fetchJai(url);
    if (!live(item)) return false;
    item.status = 'found';
    if (r.bytes) {
      // A .jai resource holds the file form and nothing else (spec §Decoding).
      const d = decode(r.bytes);
      if (d.diagnostics.some(x => x.code === 'malformed-container' || x.code === 'invalid-json')) { dismiss(item); return false; }
      item.doc = d.document; item.diagnostics = d.diagnostics;
      placeBadge(item);
      return true;
    }
    if (r.notJai) { dismiss(item); return false; }
    item.error = `Could not fetch ${url}: ${r.error}`;
    placeBadge(item);
    return false;
  }

  /** Forget an item, cancel its render (the background then cancels the provider job) and remove its control. */
  function drop(item: Item): void {
    item.port?.disconnect(); item.port = undefined;
    unbusy(item);
    item.badge?.remove();
    if (panel?.isConnected && panelItem === item) panel.remove();
    if (!live(item)) return;
    items.delete(item.img);
    io.unobserve(item.img);
    report();
  }

  /**
   * Remove an item's control without reporting a failure (spec §Discovery): its resource carries no
   * document, or its address delivered pixels. It stays removed while discovery finds the same.
   */
  function dismiss(item: Item): void {
    if (live(item)) dismissed.set(item.img, item.disc);
    drop(item);
  }

  /**
   * Spec §Discovery: the viewer stands down when the element shows the URL that the picture or src
   * carrier selected, complete and with a natural width: the user agent decoded it itself, or the
   * address delivered pixels.
   */
  function pixelsArrived(item: Item): boolean {
    const { img, disc } = item;
    if (disc.carrier === 'attribute' || !img.complete || img.naturalWidth === 0) return false;
    // Discovery keeps the URL of a resource; a data: URL is the <source>'s first candidate or the src.
    const selected = disc.kind === 'url' ? disc.url : disc.carrier === 'picture' ? firstSrcsetURL(disc.source?.getAttribute('srcset') ?? null) : img.getAttribute('src');
    if (!selected) return false;
    try { return img.currentSrc === new URL(selected, document.baseURI).href; } catch { return false; }
  }

  /** Track an <img> that carries a document; discovery runs here unless the caller ran it. */
  function add(img: HTMLImageElement, disc: Discovery | null = discover(img, { baseURL: document.baseURI })): void {
    if (off || !disc || items.has(img) || !img.isConnected) return;
    // An inline document that cannot be read carries no document, like a .jai resource that does not decode (spec §Discovery).
    if (disc.kind === 'inline' && disc.diagnostics?.some(d => d.code === 'undecodable')) return;
    const was = dismissed.get(img);
    if (was) { if (sameDocument(was, disc)) return; dismissed.delete(img); }
    const item: Item = { img, disc, doc: null, diagnostics: [], status: 'found' };
    items.set(img, item);
    if (disc.kind === 'inline') { item.doc = disc.document ?? null; item.diagnostics = disc.diagnostics ?? []; }
    placeBadge(item);
    maybeAuto(item);
  }

  /** Discovery over the document in tree order, open shadow roots included (spec §Discovery). */
  function scan(): void {
    if (off) return; // the user set this site to ignore JAI
    watchShadows(document);
    for (const d of discoverAll(document, { baseURL: document.baseURI })) add(d.element as HTMLImageElement, d);
    report();
  }

  // ------------------------------------------------------------ badges
  function placeBadge(item: Item): void {
    if (!live(item)) return;
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
    if (pixelsArrived(item)) { dismiss(item); return; }
    if (!(await resolve(item)) && !live(item)) return;
    panelItem = item;
    state = await send<StateReply>({ type: 'state', origin });
    if (!live(item)) return;
    const focusedRole = refocus ? (shadow.activeElement as HTMLElement | null)?.dataset.role ?? null : null;
    panel?.remove();
    const p = el('div', { className: 'panel' });
    p.setAttribute('role', 'dialog'); p.setAttribute('aria-label', 'JAI image');
    panel = p;
    const close = el('button', { className: 'close', type: 'button', textContent: '×' });
    close.setAttribute('aria-label', 'Close');
    close.addEventListener('click', () => { p.remove(); item.badge?.focus(); });
    p.addEventListener('keydown', e => { if (e.key === 'Escape') { p.remove(); item.badge?.focus(); } });
    const carriers: Record<string, string> = { picture: '<picture><source type="image/jai">', attribute: 'data-jai', src: item.disc.kind === 'url' ? 'a .jai file in src' : 'data:image/jai in src' };
    p.append(el('h2', {}, el('span', {}, 'JAI image'), close));
    p.append(el('div', { className: 'muted' }, `Found via ${carriers[item.disc.carrier]} on ${origin}`));
    // Attribute carrier: alt is the prompt unless data-jai gives one (spec §Carriers).
    if (item.disc.carrier === 'attribute' && item.doc?.has('prompt') && !parse(item.img.getAttribute('data-jai') ?? '').document.has('prompt')) {
      p.append(el('div', { className: 'muted' }, 'The prompt is the image’s alt text, since data-jai gives none.'));
    }
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
      const diags = [...item.diagnostics, ...validate(doc, item.disc.info), ...delivery(item, doc)];
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
    // A src image has no pixels: without a render the reader gets its alt text (spec §Carriers).
    const page = item.disc.carrier === 'src' ? 'This image has no pixels; the page shows its alt text.' : 'The page shows its own image.';
    return { found: `Not rendered. ${page}`, loading: 'Loading the .jai file…', rendering: 'Rendering…', rendered: 'Showing an AI render of the description.', failed: `Render failed. ${page}`, original: `Render kept. ${page}` }[item.status];
  }

  /** Delivery advisories (spec §Carriers): for a replaced element, those of the page's own state, never of the viewer's writes. */
  const delivery = (item: Item, doc: JaiDocument): Diagnostic[] => item.saved?.advisories ?? validateDelivery({ carrier: item.disc.carrier, element: item.img, decoded: { document: doc } });

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
    if (pixelsArrived(item)) { dismiss(item); return; }
    if (!(await resolve(item)) || !item.doc) return;
    const it = interpret(item.doc).interpreted;
    // Never automatically: keep (spec §Rendering modes), a document with unsupported-version
    // (§Consent, budgets and cancellation), or an element already replaced with this document (§Replacement).
    if (automatic && (it.mode === 'keep' || item.diagnostics.some(d => d.code === 'unsupported-version') || replacedWith.get(item.img)?.has(canonicalJSON(item.doc)))) return;
    // The src carrier has no original pixels (spec §Modes and the original image).
    const hasOriginal = item.disc.carrier !== 'src' && item.img.naturalWidth > 0;
    const input = it.mode === 'transform' && hasOriginal ? await readableInput(item.img) : null;
    if (!live(item) || item.port) return; // gone, or another render started meanwhile
    const rect = item.img.getBoundingClientRect();
    const req: RenderPortRequest = {
      type: 'render', canonicalJSON: canonicalJSON(item.doc), origin, topSite, pageId, modelId, automatic,
      inputImage: input,
      governingRatio: governingRatio(item.img, hasOriginal),
      target: rect.width ? { width: Math.round(rect.width * devicePixelRatio), height: Math.round(rect.height * devicePixelRatio) } : null,
    };
    item.status = 'rendering'; placeBadge(item);
    item.busy = item.img.getAttribute('aria-busy');
    item.img.setAttribute('aria-busy', 'true');
    const port = chrome.runtime.connect({ name: 'render' });
    item.port = port;
    // Spec §Accessibility: individual replacements are not announced; a render started from the panel reports there.
    const setStatus = (t: string): void => { if (!statusEl) return; statusEl.textContent = t; announce(t); };
    setStatus('Rendering…');
    port.onMessage.addListener((m: RenderPortReply) => {
      if (!live(item)) return;
      if (m.type === 'status') { setStatus(`Rendering… (${m.status})`); return; }
      item.port = undefined; port.disconnect();
      if (m.type === 'done') { item.render = m; void apply(item, m).then(() => { if (!live(item) || !statusEl) return; announce(statusText(item)); if (panel?.isConnected) void openPanel(item, true); }); }
      else { item.status = 'failed'; item.error = m.message; restore(item, false); setStatus(m.message); placeBadge(item); }
    });
    port.onDisconnect.addListener(() => { if (!live(item) || item.port !== port) return; item.port = undefined; if (item.status === 'rendering') { item.status = 'failed'; restore(item, false); placeBadge(item); } });
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

  function save(item: Item): SavedState {
    const img = item.img;
    return { srcset: img.getAttribute('srcset'), sizes: img.getAttribute('sizes'), alt: img.getAttribute('alt'), ariaDescription: img.getAttribute('aria-description'), insertedSource: null, canvas: null, display: img.style.display, objectFit: null, sizeStyle: null, advisories: item.doc ? delivery(item, item.doc) : [] };
  }

  /** Spec §Size and aspect without a governing ratio: aspect, the preview's ratio, else the renderer's default, which is the render's own ratio. */
  function layoutRatio(doc: JaiDocument, r: Extract<RenderPortReply, { type: 'done' }>): number | null {
    const it = interpret(doc).interpreted;
    if (it.aspect) return it.aspect.width / it.aspect.height;
    const size = imageSize(b64decode(r.dataURL.split(',')[1] ?? ''));
    return previewRatio(it.preview) ?? (size ? size.width / size.height : null);
  }

  async function apply(item: Item, r: Extract<RenderPortReply, { type: 'done' }>): Promise<void> {
    const img = item.img;
    item.saved ??= save(item);
    const saved = item.saved;
    const picture = img.parentElement?.localName === 'picture' ? img.parentElement : null;
    own(() => {
      // A src image with no size (for example Markdown output) is laid out at the available width, at the ratio spec §Size and aspect chooses.
      const box = img.getBoundingClientRect();
      const ratio = item.disc.carrier === 'src' && item.doc && (box.width < 2 || box.height < 2) && saved.sizeStyle === null ? layoutRatio(item.doc, r) : null;
      if (ratio) {
        saved.sizeStyle = { width: img.style.width, aspectRatio: img.style.aspectRatio };
        img.style.width = '100%';
        img.style.aspectRatio = String(ratio);
      }
      // Spec §Replacement: setting src alone is a no-op under srcset or a matching <source>.
      if (picture) {
        if (saved.insertedSource) setAttr(saved.insertedSource, 'srcset', r.dataURL);
        else {
          const s = document.createElement('source');
          s.srcset = r.dataURL;
          placeNode(picture, () => picture.insertBefore(s, picture.firstChild));
          saved.insertedSource = s;
        }
      } else {
        setAttr(img, 'sizes', null);
        setAttr(img, 'srcset', r.dataURL);
      }
    });
    // Counted from the first write, so a page that undoes the replacement cannot make automatic rendering loop.
    if (item.doc) { let docs = replacedWith.get(img); if (!docs) replacedWith.set(img, (docs = new Set())); docs.add(canonicalJSON(item.doc)); }
    const loaded = await new Promise<boolean>(res => {
      if (img.currentSrc === r.dataURL && img.complete && img.naturalWidth > 0) return res(true);
      const done = (ok: boolean): void => { img.removeEventListener('load', onLoad); img.removeEventListener('error', onErr); res(ok); };
      const onLoad = (): void => done(img.currentSrc === r.dataURL);
      // Only an error about our own image counts; a late error from the original .jai load does not.
      const onErr = (): void => { if (img.currentSrc === r.dataURL) done(false); };
      img.addEventListener('load', onLoad); img.addEventListener('error', onErr);
      setTimeout(() => done(img.currentSrc === r.dataURL && img.naturalWidth > 0), 4000);
    });
    if (!live(item)) return;
    if (!loaded) {
      // The page's CSP refused data: images. Paint into a canvas instead: no page-initiated fetch.
      undoReplacement(img, saved);
      try {
        const bmp = await createImageBitmap(new Blob([b64decode(r.dataURL.split(',')[1] ?? '')], { type: r.mime }));
        if (!live(item)) return;
        const rect = img.getBoundingClientRect();
        const c = document.createElement('canvas');
        c.width = Math.round(rect.width * devicePixelRatio) || bmp.width; c.height = Math.round(rect.height * devicePixelRatio) || bmp.height;
        c.style.cssText = `width:${rect.width}px;height:${rect.height}px;display:inline-block;vertical-align:${getComputedStyle(img).verticalAlign}`;
        // Cover-crop, centred: never stretch (spec §Size and aspect).
        const scale = Math.max(c.width / bmp.width, c.height / bmp.height);
        const sw = c.width / scale, sh = c.height / scale;
        c.getContext('2d')?.drawImage(bmp, (bmp.width - sw) / 2, (bmp.height - sh) / 2, sw, sh, 0, 0, c.width, c.height);
        c.setAttribute('role', 'img');
        describe(c, item);
        const parent = img.parentNode;
        own(() => { if (parent) placeNode(parent, () => img.after(c)); img.style.display = 'none'; });
        saved.canvas = c;
      } catch { item.status = 'failed'; item.error = 'This page does not allow replacement images.'; restore(item, false); return; }
    } else {
      own(() => {
        describe(img, item);
        // Never stretch (spec §Size and aspect): honour the page's object-fit, else cover-crop.
        if (getComputedStyle(img).objectFit === 'fill' && saved.objectFit === null) {
          saved.objectFit = img.style.objectFit;
          img.style.objectFit = 'cover';
        }
      });
    }
    unbusy(item);
    item.status = 'rendered';
    placeBadge(item);
  }

  /** Put back the page's own aria-busy once the viewer no longer marks a render pending. */
  function unbusy(item: Item): void {
    if (item.busy === undefined) return;
    if (item.busy === null) item.img.removeAttribute('aria-busy'); else item.img.setAttribute('aria-busy', item.busy);
    item.busy = undefined;
  }

  /** Accessible name and the AI-rendered state (spec §Accessibility). */
  function describe(target: Element, item: Item): void {
    const saved = item.saved;
    const noAlt = saved?.alt === null;
    // Spec §Accessibility: no alt → the description, in full (no length limit), control characters removed.
    const name = noAlt ? ((item.doc?.get('prompt') ?? []).join('\n') || 'Image').replace(/[\u0000-\u0009\u000b-\u001f\u007f]/g, ' ') : saved?.alt ?? '';
    own(() => {
      if (target === item.img) setAttr(target, 'alt', name); else target.setAttribute('aria-label', name);
      target.setAttribute('aria-description', 'Rendered by an AI model from its description; it may differ from the publisher’s image. The original is available from the JAI control.');
    });
  }

  function undoReplacement(img: HTMLImageElement, saved: SavedState): void {
    own(() => {
      const s = saved.insertedSource, parent = s?.parentNode;
      if (s && parent) placeNode(parent, () => s.remove());
      saved.insertedSource = null;
      setAttr(img, 'srcset', saved.srcset);
      setAttr(img, 'sizes', saved.sizes);
    });
  }

  function restore(item: Item, keepRender: boolean): void {
    const img = item.img, saved = item.saved;
    unbusy(item);
    if (saved) own(() => {
      undoReplacement(img, saved);
      const c = saved.canvas, parent = c?.parentNode;
      if (c && parent) placeNode(parent, () => c.remove());
      saved.canvas = null;
      img.style.display = saved.display;
      if (saved.objectFit !== null) img.style.objectFit = saved.objectFit;
      if (saved.sizeStyle) { img.style.width = saved.sizeStyle.width; img.style.aspectRatio = saved.sizeStyle.aspectRatio; }
      setAttr(img, 'alt', saved.alt);
      if (saved.ariaDescription === null) img.removeAttribute('aria-description'); else img.setAttribute('aria-description', saved.ariaDescription);
    });
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
      void render(item, null, true);
    }
  }, { rootMargin: '0px', threshold: [0.5], trackVisibility: true, delay: 100 } as IntersectionObserverInit);
  function maybeAuto(item: Item): void { if (autoOn) io.observe(item.img); }

  // ------------------------------------------------------------ mutations (spec §Discovery, §Replacement)
  // Discovery re-runs, in every state of an item, on the page's changes to an <img>'s src, srcset,
  // sizes, alt and data-jai, to the children of a <picture>, and to srcset, media and type of a
  // <source> in one (its sizes never changes the document the <source> carries). It never runs on
  // the viewer's own changes (spec §Untrusted input and bounds): every DOM write the observer can
  // see goes through own().
  const IMG_ATTRS = ['src', 'srcset', 'sizes', 'alt', 'data-jai'];
  const SOURCE_ATTRS = ['srcset', 'media', 'type'];
  const WATCH: MutationObserverInit = { childList: true, subtree: true, attributes: true, attributeFilter: [...new Set([...IMG_ATTRS, ...SOURCE_ATTRS])] };
  /**
   * Images whose discovery re-runs at the next flush, with the attributes the page changed on them
   * (the viewer leaves those alone) and PICTURE when the children of the <picture> they are in, or left, changed.
   */
  const dirty = new Map<HTMLImageElement, Set<string>>();
  const PICTURE = '<picture>';
  let rescan = false, flushQueued = false, off = false;
  /** While own() runs: the writes it made, as [target, attribute name or 'childList']. */
  let writes: Array<[Node, string]> | null = null;

  function mark(img: HTMLImageElement, change: string): void {
    let changed = dirty.get(img);
    if (!changed) dirty.set(img, (changed = new Set()));
    changed.add(change);
  }
  const markPicture = (picture: Element): void => { const img = Array.from(picture.children).find(c => c instanceof HTMLImageElement); if (img) mark(img, PICTURE); };
  /** Whether an added subtree may hold images: an <img>, or a shadow host (its shadow tree is not in the subtree). */
  function mayHoldImages(n: Element): boolean {
    const walk = document.createTreeWalker(n, NodeFilter.SHOW_ELEMENT);
    for (let e: Node | null = n; e; e = walk.nextNode()) if ((e as Element).localName === 'img' || (e as Element).shadowRoot) return true;
    return false;
  }

  /** Take in the page's mutation records; discovery re-runs in a microtask. */
  function note(muts: MutationRecord[]): void {
    for (const m of muts) {
      const t = m.target as Element;
      if (m.type === 'attributes') {
        const a = m.attributeName ?? '';
        if (t instanceof HTMLImageElement && IMG_ATTRS.includes(a)) mark(t, a);
        else if (t.localName === 'source' && SOURCE_ATTRS.includes(a) && t.parentElement?.localName === 'picture') markPicture(t.parentElement);
        continue;
      }
      if (t.localName === 'picture' && [...Array.from(m.addedNodes), ...Array.from(m.removedNodes)].some(n => n instanceof Element)) {
        markPicture(t);
        for (const n of Array.from(m.removedNodes)) if (n instanceof HTMLImageElement) mark(n, PICTURE);
      }
      // A moved <img> outside any <picture> keeps its item; the scan finds images new to the viewer.
      for (const n of Array.from(m.addedNodes)) if (n instanceof Element && mayHoldImages(n)) rescan = true;
    }
    if ((dirty.size || rescan) && !flushQueued) { flushQueued = true; queueMicrotask(flush); }
  }

  function flush(): void {
    flushQueued = false;
    for (const [img, changed] of dirty) { rediscover(img, changed); dirty.delete(img); }
    if (rescan) { rescan = false; scan(); } else report();
  }

  const sameDocument = (a: Discovery, b: Discovery): boolean => a.carrier === b.carrier && a.kind === b.kind
    && (a.kind === 'url' ? a.url === b.url : !!a.document && !!b.document && canonicalJSON(a.document) === canonicalJSON(b.document));

  /** Discovery re-runs for one element, after the page made the changes in `changed`. */
  function rediscover(img: HTMLImageElement, changed: Set<string>): void {
    const item = items.get(img), saved = item?.saved;
    // On a replaced element, what the page changed is the state to restore and what discovery reads.
    if (saved) {
      if (changed.has('sizes')) saved.sizes = img.getAttribute('sizes');
      if (changed.has('alt')) saved.alt = img.getAttribute('alt');
    }
    if (item && keeps(item, changed)) {
      // A replaced element keeps its render, named by the page's alt (setAttr writes alt once it is out of `changed`).
      if (saved && changed.delete('alt')) describe(saved.canvas ?? img, item);
      return;
    }
    if (item) {
      // A new element: put back what the viewer changed, leaving the attributes the page changed,
      // cancel any render and release the item (spec §Replacement).
      restore(item, false);
      drop(item);
    }
    add(img);
  }

  /** Whether an item survives the page's changes (it then takes in the fresh discovery): the same document, and nothing requires treating the element as new. */
  function keeps(item: Item, changed: Set<string>): boolean {
    const shown = changed.has('src') || changed.has('srcset') || changed.has(PICTURE);
    if (item.saved) {
      // Spec §Replacement: src, srcset or <picture> children changed on a replaced element: it is new.
      if (shown) return false;
      // Otherwise only alt, sizes or data-jai changed. sizes never matters to discovery; a JAI <source>
      // outranks data-jai and alt; data-jai outranks the src carrier, whose document alt never touches. The
      // attribute carrier is discovered afresh from the element as the page has it: the viewer's srcset,
      // <source> and alt play no part in it.
      if (item.disc.carrier === 'picture') return true;
      if (item.disc.carrier === 'src') return !changed.has('data-jai');
    } else if (item.port && shown && item.disc.carrier !== 'src' && item.doc && interpret(item.doc).interpreted.mode === 'transform') {
      return false; // the render in flight was asked for with the pixels shown before
    }
    const next = discover(item.saved ? pageView(item.img, item.saved) : item.img, { baseURL: document.baseURI });
    if (!next || !sameDocument(item.disc, next)) return false;
    item.disc = next;
    if (next.kind === 'inline') { item.doc = next.document ?? null; item.diagnostics = next.diagnostics ?? []; }
    return true;
  }

  /**
   * A replaced element as the page has it: the attributes the viewer writes on it read as the page left
   * them, and its parent lists this view in the element's place, without the viewer's inserted <source>,
   * so discovery stops at the element exactly as it does for the real one.
   */
  function pageView(img: HTMLImageElement, saved: SavedState): ElementLike {
    const page = new Map([['srcset', saved.srcset], ['sizes', saved.sizes], ['alt', saved.alt]]);
    const view: ElementLike = { localName: img.localName, parentElement: null, children: img.children, getAttribute: n => page.has(n) ? page.get(n) ?? null : img.getAttribute(n) };
    const parent = img.parentElement;
    if (parent) {
      const siblings = Array.from(parent.children).filter(c => c !== saved.insertedSource).map(c => (c === img ? view : c));
      view.parentElement = { localName: parent.localName, parentElement: parent.parentElement, children: siblings, getAttribute: n => parent.getAttribute(n) };
    }
    return view;
  }

  /**
   * Run the viewer's own DOM writes. The page's records queued before them are taken in first, so
   * the writes leave what the page changed alone (setAttr); of the records taken right after, those
   * that match a write made here are dropped, and any other (a page reaction that ran synchronously
   * inside a write) is still the page's.
   */
  function own(fn: () => void): void {
    if (writes) { fn(); return; }
    note(mo.takeRecords());
    const mine: Array<[Node, string]> = (writes = []);
    try { fn(); } finally { writes = null; }
    note(mo.takeRecords().filter(m => {
      const i = mine.findIndex(([n, what]) => n === m.target && what === (m.attributeName ?? m.type));
      if (i >= 0) mine.splice(i, 1);
      return i < 0;
    }));
  }
  /** Set an attribute (remove it for null), unless it already has that value or the page changed it since discovery last ran. */
  function setAttr(el: Element, name: string, value: string | null): void {
    if (el.getAttribute(name) === value || (el instanceof HTMLImageElement && dirty.get(el)?.has(name))) return;
    if (value === null) el.removeAttribute(name); else el.setAttribute(name, value);
    writes?.push([el, name]);
  }
  /** Insert or remove a viewer-owned node under `parent`. */
  function placeNode(parent: Node, change: () => void): void { change(); writes?.push([parent, 'childList']); }

  // Open shadow roots are observed like the document; closed ones stay out of reach (spec §Discovery).
  const shadows = new WeakSet<ShadowRoot>();
  function watchShadows(root: Document | ShadowRoot): void {
    const walk = document.createTreeWalker(root, NodeFilter.SHOW_ELEMENT);
    for (let n = walk.nextNode(); n; n = walk.nextNode()) {
      const sr = (n as Element).shadowRoot;
      if (!sr) continue;
      if (!shadows.has(sr)) { shadows.add(sr); mo.observe(sr, WATCH); }
      watchShadows(sr);
    }
  }

  const mo = new MutationObserver(note);
  mo.observe(document.documentElement, WATCH);

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
    if (s.mode === 'never') { off = true; for (const i of items.values()) i.badge?.remove(); mo.disconnect(); return; }
    if (s.mode === 'auto' && s.hasToken && s.disclosed && window === window.top) { autoOn = true; for (const i of items.values()) maybeAuto(i); }
  });
}
