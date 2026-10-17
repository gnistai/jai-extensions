// Background service worker of the JAI reference viewer.
//
// The only context that ever holds the Replicate token or talks to a provider.
// Content scripts send it a canonical JAI document plus, for transform, pixels
// the page itself could read; it applies the user's consent and budgets,
// chooses among the models the user enabled, renders, and returns bytes.
import { identity, interpret, readJSON, serialize, sha256Hex, type CatalogModel } from '../jai/index.ts';
import { FAMILIES, REPLICATE_MODELS, ReplicateRenderer } from '../renderers/replicate.ts';
import { HttpReplicateClient } from '../providers/replicate-client.ts';
import { PortLedger, allowedModel, sanitizeInput } from './encode-guard.ts';
import { RateLimit, privilegedFetchAllowed } from './fetch-guard.ts';
import { INTERPRETER_CODES, RenderError, type RenderIntent } from '../renderers/types.ts';
import { previewRatio } from '../renderers/image-size.ts';
import { idbAll, idbClear, idbDelete, idbGet, idbSet } from './idb.ts';
import { MAX_JAI_BYTES, b64decode, b64encode, getSettings, readCapped, saveSettings, siteMode, today, type BgRequest, type EncodePortReply, type EncodePortRequest, type RenderPortReply, type RenderPortRequest, type Settings, type StateReply } from './shared.ts';

// ---------------------------------------------------------------- token

const getToken = (): Promise<string | undefined> => idbGet<string>('secrets', 'replicate');

// ---------------------------------------------------------------- budgets and queue

const fetchLimit = new RateLimit(20, 60_000);

const perPage = new Map<string, number>();
const inflight = new Map<string, Promise<Extract<RenderPortReply, { type: 'done' }> | null>>();
let running = 0;
const waiting: Array<() => void> = [];

async function acquire(max: number, signal: AbortSignal): Promise<void> {
  if (running < max) { running++; return; }
  await new Promise<void>((resolve, reject) => {
    const go = (): void => { running++; resolve(); };
    waiting.push(go);
    signal.addEventListener('abort', () => { const i = waiting.indexOf(go); if (i >= 0) waiting.splice(i, 1); reject(new RenderError('Canceled', 'canceled')); }, { once: true });
  });
}
function release(): void { running--; waiting.shift()?.(); }

// Daily usage lives in memory once loaded, so that checking and reserving a render
// happen in one synchronous step (no await between them); writes are serialised.
let usage: { day: string; count: number } | null = null;
let usageLoad: Promise<void> | null = null;
let usageWrite: Promise<unknown> = Promise.resolve();
function loadUsage(): Promise<void> {
  usageLoad ??= chrome.storage.local.get('usage').then(({ usage: u }) => {
    const v = u as { day?: unknown; count?: unknown } | undefined;
    usage = v && v.day === today() && Number.isInteger(v.count) ? { day: today(), count: v.count as number } : { day: today(), count: 0 };
  });
  return usageLoad;
}
function usageNow(): number {
  if (!usage || usage.day !== today()) usage = { day: today(), count: 0 };
  return usage.count;
}
function addUsage(delta: number): void {
  usageNow();
  usage!.count = Math.max(0, usage!.count + delta);
  const snapshot = { ...usage! };
  usageWrite = usageWrite.then(() => chrome.storage.local.set({ usage: snapshot })).catch(() => undefined);
}
async function usageToday(): Promise<number> { await loadUsage(); return usageNow(); }

// ---------------------------------------------------------------- model choice

function catalog(s: Settings): CatalogModel[] {
  return REPLICATE_MODELS.filter(m => s.enabledModels.includes(m.id)).map(m => ({ id: m.id, aliases: m.aliases, family: m.family, imageInput: m.imageInput, default: m.id === s.defaultModel }));
}

// ---------------------------------------------------------------- render

interface CacheEntry { b64: string; mime: string; model: string; sentPrompt: string; sentNegative: string | null; sentInput: Record<string, unknown>; jobId: string | null; diagnostics: Array<{ code: string; key?: string }>; created: number }

async function handleRender(req: RenderPortRequest, post: (m: RenderPortReply) => void, signal: AbortSignal): Promise<void> {
  await loadUsage();
  const s = await getSettings();
  const token = await getToken();
  if (!token) return post({ type: 'error', kind: 'config', message: 'Add your Replicate API token in the extension options first.' });
  const mode = siteMode(s, req.origin);
  if (mode === 'never') return post({ type: 'error', kind: 'config', message: 'JAI rendering is turned off for this site.' });
  if (!s.disclosed[req.origin]) return post({ type: 'error', kind: 'consent', message: 'Accept the provider notice for this site first.' });
  if (req.automatic && mode !== 'auto') return post({ type: 'error', kind: 'consent', message: 'Automatic rendering is not enabled for this site.' });

  const read = readJSON(req.canonicalJSON);
  if (read.diagnostics.some(d => d.code === 'invalid-json')) return post({ type: 'error', kind: 'config', message: 'Invalid document.' });

  const hasInput = req.inputImage !== null;
  const { interpreted: it, diagnostics: idiag } = interpret(read.document, { catalog: catalog(s), families: FAMILIES, hasOriginal: hasInput, governingRatio: req.governingRatio });
  let modelId = req.modelId && s.enabledModels.includes(req.modelId) ? req.modelId : it.matched ?? null;
  if (!modelId) return post({ type: 'error', kind: 'config', message: 'No model is enabled. Choose one in the options.' });
  const model = REPLICATE_MODELS.find(m => m.id === modelId);
  if (!model) return post({ type: 'error', kind: 'config', message: 'Unknown model.' });
  modelId = model.id;
  if (it.mode === 'transform' && hasInput && !model.imageInput) {
    const alt = REPLICATE_MODELS.find(m => m.imageInput && s.enabledModels.includes(m.id));
    if (alt && !req.modelId) modelId = alt.id;
  }

  // Per-model refinements follow the model actually used, which may differ from the matched one.
  const merged = modelId === it.matched ? it : interpret(read.document, { catalog: catalog(s), families: FAMILIES, hasOriginal: hasInput, governingRatio: req.governingRatio, selected: modelId }).interpreted;

  // Cache: identity + renderer configuration + input image + top-level site (spec: Caches).
  const inputHash = req.inputImage ? await sha256Hex(b64decode(req.inputImage.b64)) : '-';
  // The ratio actually requested is part of the render configuration (spec: render cache key):
  // the governing ratio, else aspect, else the preview's ratio (spec §Size and aspect).
  const preview = previewRatio(it.preview);
  const ratio = it.ratio ?? preview;
  const ratioKey = ratio ? ratio.toFixed(3) : '-';
  const key = await sha256Hex(new TextEncoder().encode([await identity(read.document), modelId, model.version ?? '', s.foldAvoid ? 'fold' : 'omit', inputHash, ratioKey, req.topSite].join('|')));
  // Identical requests already in flight share one provider job (never pay twice).
  // Registration is synchronous, before any further await, so concurrent requests cannot race.
  const pending = inflight.get(key);
  if (pending) {
    post({ type: 'status', status: 'waiting for an identical render' });
    const r = await pending;
    if (r) return post({ ...r, cached: true, seconds: 0 });
    return post({ type: 'error', kind: 'provider', message: 'An identical render just failed; try again.' });
  }
  let settle!: (r: Extract<RenderPortReply, { type: 'done' }> | null) => void;
  inflight.set(key, new Promise(res => { settle = res; }));
  const finish = (r: Extract<RenderPortReply, { type: 'done' }> | null): void => { settle(r); inflight.delete(key); };

  const cached = await idbGet<CacheEntry>('cache', key);
  if (cached) {
    const done: Extract<RenderPortReply, { type: 'done' }> = { type: 'done', dataURL: `data:${cached.mime};base64,${cached.b64}`, mime: cached.mime, model: cached.model, modelLabel: model.label, sentPrompt: cached.sentPrompt, sentNegative: cached.sentNegative, sentInput: cached.sentInput, jobId: cached.jobId, diagnostics: cached.diagnostics, cached: true, seconds: 0 };
    finish(done);
    return post(done);
  }

  // Budgets (spec: Consent, budgets and cancellation). Check and reserve in one
  // synchronous step, so that a burst of requests cannot all pass the check.
  const pageCount = perPage.get(req.pageId) ?? 0;
  if (pageCount >= s.maxPerPage) { finish(null); return post({ type: 'error', kind: 'budget', message: `Page limit reached (${s.maxPerPage} renders). Change it in the options.` }); }
  if (usageNow() >= s.maxPerDay) { finish(null); return post({ type: 'error', kind: 'budget', message: `Daily limit reached (${s.maxPerDay} renders). Change it in the options.` }); }
  perPage.set(req.pageId, pageCount + 1);
  addUsage(1);

  post({ type: 'status', status: 'queued' });
  try {
    await acquire(s.maxConcurrent, signal);
  } catch {
    // Canceled before the provider was asked: give the reservation back.
    perPage.set(req.pageId, Math.max(0, (perPage.get(req.pageId) ?? 1) - 1));
    addUsage(-1);
    finish(null);
    return post({ type: 'error', kind: 'canceled', message: 'Canceled' });
  }
  const t0 = Date.now();
  try {
    const intent: RenderIntent = {
      description: merged.modelDescription, style: merged.modelStyle, avoid: merged.modelAvoid,
      aspect: it.aspect ? { width: it.aspect.width, height: it.aspect.height } : null,
      mode: it.mode, strength: it.strength, seed: it.seed,
    };
    const renderer = new ReplicateRenderer(token, { foldAvoid: s.foldAvoid });
    const result = await renderer.render({
      intent, modelId, signal,
      inputImage: req.inputImage ? { bytes: b64decode(req.inputImage.b64), mime: req.inputImage.mime } : null,
      governingRatio: req.governingRatio, previewRatio: preview, target: req.target,
      onStatus: st => post({ type: 'status', status: st }),
    });
    const interpreterCodes = idiag.filter(d => INTERPRETER_CODES.has(d.code));
    const diagnostics = [...interpreterCodes.map(d => ({ code: d.code, ...(d.key ? { key: d.key } : {}) })), ...result.diagnostics];
    const b64 = b64encode(result.bytes);
    const entry: CacheEntry = { b64, mime: result.mime, model: result.model, sentPrompt: result.sentPrompt, sentNegative: result.sentNegative, sentInput: result.sentInput, jobId: result.jobId, diagnostics, created: Date.now() };
    await idbSet('cache', key, entry);
    if (s.keepHistory) await idbSet('history', `${Date.now()}-${result.jobId ?? ''}`, { time: new Date().toISOString(), site: req.topSite, model: result.model, jobId: result.jobId, jai: serialize(read.document), costHint: model.costHint });
    const done: Extract<RenderPortReply, { type: 'done' }> = { type: 'done', dataURL: `data:${result.mime};base64,${b64}`, mime: result.mime, model: result.model, modelLabel: model.label, sentPrompt: result.sentPrompt, sentNegative: result.sentNegative, sentInput: result.sentInput, jobId: result.jobId, diagnostics, cached: false, seconds: Math.round((Date.now() - t0) / 100) / 10 };
    finish(done);
    post(done);
  } catch (e) {
    finish(null);
    const err = e instanceof RenderError ? e : new RenderError(String((e as Error)?.message ?? e), 'provider');
    post({ type: 'error', kind: err.kind, message: err.message });
  } finally {
    release();
  }
}

const originOf = (url: string): string | null => { try { const o = new URL(url).origin; return o === 'null' ? null : o; } catch { return null; } };
/** The origin of the frame that sent a message, as the browser reports it. */
function senderOrigin(sender: chrome.runtime.MessageSender | undefined): string | null {
  if (!sender) return null;
  if (sender.origin && sender.origin !== 'null') return sender.origin;
  return sender.url ? originOf(sender.url) : null;
}

// ---------------------------------------------------------------- encoder port

function handleEncodePort(port: chrome.runtime.Port): void {
  const jobs = new Map<number, AbortController>();
  const ledger = new PortLedger();
  let open = true;
  const post = (m: EncodePortReply): void => { if (open) port.postMessage(m); };
  port.onDisconnect.addListener(() => { open = false; for (const ac of jobs.values()) ac.abort(); jobs.clear(); });
  port.onMessage.addListener((msg: EncodePortRequest) => {
    if (msg?.type === 'begin') { ledger.begin(msg.budgetUSD); return; }
    if (msg?.type === 'cancel') { jobs.get(msg.id)?.abort(); return; }
    if (msg?.type !== 'predict' || !Number.isSafeInteger(msg.id) || jobs.has(msg.id)) return;
    const refuse = (message: string): void => post({ type: 'error', id: msg.id, kind: 'config', message, jobId: null });
    const model = allowedModel(msg.model ?? {});
    if (!model) return refuse('That model is not available to the encoder.');
    const clean = sanitizeInput(model, msg.input);
    if ('error' in clean) return refuse(clean.error);
    const denied = ledger.admit(model);
    if (denied) return post({ type: 'error', id: msg.id, kind: 'budget', message: denied, jobId: null });
    const ac = new AbortController();
    jobs.set(msg.id, ac);
    let metrics: Record<string, unknown> | null = null;
    void (async () => {
      try {
        const token = await getToken();
        if (!token) return post({ type: 'error', id: msg.id, kind: 'auth', message: 'Add your Replicate API token in the extension options first.', jobId: null });
        const p = await new HttpReplicateClient(token).predict(model, clean.input, { signal: ac.signal, onStatus: status => post({ type: 'status', id: msg.id, status }) });
        metrics = p.metrics ?? null;
        post({ type: 'result', id: msg.id, prediction: { id: p.id, status: p.status, output: p.output, metrics: p.metrics } });
      } catch (e) {
        const err = e as RenderError;
        post({ type: 'error', id: msg.id, kind: err.kind ?? 'provider', message: err.message, jobId: err.jobId ?? null });
      } finally { jobs.delete(msg.id); ledger.done(model, metrics); }
    })();
  });
}

chrome.runtime.onConnect.addListener(port => {
  if (port.name === 'encode') {
    // Only the extension's own encode page; content scripts report the web page's URL.
    const page = port.sender?.url?.split(/[?#]/)[0];
    if (port.sender?.id !== chrome.runtime.id || page !== chrome.runtime.getURL('encode.html')) return port.disconnect();
    return handleEncodePort(port);
  }
  if (port.name !== 'render' || !port.sender?.tab) return port.disconnect();
  const ac = new AbortController();
  let open = true;
  // The page went away or the user canceled: abort and cancel the provider job.
  port.onDisconnect.addListener(() => { open = false; ac.abort(); });
  port.onMessage.addListener((msg: RenderPortRequest | { type: 'cancel' }) => {
    if (msg.type === 'cancel') { ac.abort(); return; }
    if (msg.type !== 'render') return;
    // Origins come from the browser, not from the message (a compromised renderer could lie).
    const origin = senderOrigin(port.sender);
    const top = port.sender?.tab?.url ? originOf(port.sender.tab.url) : null;
    if (!origin || !top) { if (open) port.postMessage({ type: 'error', kind: 'config', message: 'Unknown page origin.' }); return; }
    void handleRender({ ...msg, origin, topSite: top }, m => { if (open) port.postMessage(m); }, ac.signal);
  });
});

// ---------------------------------------------------------------- automatic-mode registration

const scriptId = (origin: string): string => 'jai-auto-' + origin.replace(/[^a-z0-9]/gi, '_');

async function syncRegistrations(): Promise<void> {
  const s = await getSettings();
  const existing = await chrome.scripting.getRegisteredContentScripts();
  const wanted = Object.entries(s.sites).filter(([, m]) => m === 'auto').map(([o]) => o);
  for (const r of existing) if (r.id.startsWith('jai-auto-') && !wanted.some(o => scriptId(o) === r.id)) await chrome.scripting.unregisterContentScripts({ ids: [r.id] });
  for (const origin of wanted) {
    if (existing.some(r => r.id === scriptId(origin))) continue;
    const pattern = origin + '/*';
    if (!(await chrome.permissions.contains({ origins: [pattern] }))) continue;
    await chrome.scripting.registerContentScripts([{ id: scriptId(origin), matches: [pattern], js: ['content.js'], runAt: 'document_idle', allFrames: false }]);
  }
}
chrome.runtime.onStartup.addListener(() => { void syncRegistrations(); });
chrome.runtime.onInstalled.addListener(() => {
  void syncRegistrations();
  chrome.contextMenus.create({ id: 'jai-encode', title: 'Encode this image as JAI…', contexts: ['image'] }, () => void chrome.runtime.lastError);
});
chrome.contextMenus.onClicked.addListener(info => {
  if (info.menuItemId !== 'jai-encode') return;
  const src = info.srcUrl && /^(https?|data):/i.test(info.srcUrl) ? info.srcUrl : '';
  void chrome.tabs.create({ url: chrome.runtime.getURL('encode.html') + (src ? `#src=${encodeURIComponent(src)}` : '') });
});
chrome.permissions.onRemoved.addListener(() => { void syncRegistrations(); });

// ---------------------------------------------------------------- messages

async function onRequest(msg: BgRequest | { type: 'found'; count: number }, sender: chrome.runtime.MessageSender): Promise<unknown> {
  switch (msg.type) {
    case 'state': {
      const s = await getSettings();
      const reply: StateReply = {
        hasToken: !!(await getToken()), mode: siteMode(s, msg.origin), disclosed: !!s.disclosed[msg.origin],
        models: REPLICATE_MODELS.map(m => ({ id: m.id, label: m.label, imageInput: m.imageInput, costHint: m.costHint, enabled: s.enabledModels.includes(m.id), description: m.description })),
        defaultModel: s.defaultModel, usage: { today: await usageToday(), maxPerDay: s.maxPerDay, maxPerPage: s.maxPerPage }, minDisplayedPx: s.minDisplayedPx,
      };
      return reply;
    }
    case 'found': {
      if (sender.tab?.id !== undefined) await chrome.action.setBadgeText({ tabId: sender.tab.id, text: msg.count ? String(msg.count) : '' });
      await chrome.action.setBadgeBackgroundColor({ color: '#c2410c' });
      return true;
    }
    case 'openOptions': await chrome.runtime.openOptionsPage(); return true;
    case 'acceptDisclosure': {
      // A content script may accept only for its own origin.
      if (!sender.url?.startsWith(chrome.runtime.getURL('')) && senderOrigin(sender) !== msg.origin) return false;
      const s = await getSettings();
      await saveSettings({ disclosed: { ...s.disclosed, [msg.origin]: true } });
      return true;
    }
    case 'setSiteMode': {
      const s = await getSettings();
      await saveSettings({ sites: { ...s.sites, [msg.origin]: msg.mode } });
      await syncRegistrations();
      return true;
    }
    case 'fetchJai': {
      // A resource named by the picture or src carrier goes over the network at most once per page, so one
      // route is chosen before anything is sent (spec §Permitted inputs). The background fetches a cross-origin
      // .jai resource where the user granted host permission, within the rules that keep a page from reaching
      // other servers through it and within the rate limit per tab. For anything else it answers { page: true }
      // without touching the network, and the content script fetches with the page's own privileges.
      // Returns bytes, never executes anything.
      const pageOrigin = senderOrigin(sender);
      if (pageOrigin && siteMode(await getSettings(), pageOrigin) === 'never') return { error: 'JAI is turned off for this site' };
      if (!privilegedFetchAllowed(msg.url, pageOrigin) || originOf(msg.url) === pageOrigin
        || !(await chrome.permissions.contains({ origins: [`${originOf(msg.url)}/*`] }))
        || !sender.tab?.id || !fetchLimit.allow(String(sender.tab.id))) return { page: true };
      try {
        const res = await fetch(msg.url, { credentials: 'omit', referrerPolicy: 'no-referrer', redirect: 'error' });
        if (!res.ok) { void res.body?.cancel(); return { error: `HTTP ${res.status}` }; }
        if (/^text\/html\b/i.test(res.headers.get('content-type') ?? '')) { void res.body?.cancel(); return { notJai: true }; }
        const buf = await readCapped(res, MAX_JAI_BYTES);
        if (!buf) return { error: 'Too large' };
        return { b64: b64encode(buf) };
      } catch (e) { return { error: String((e as Error).message) }; }
    }
    case 'setToken': {
      const t = msg.token.trim();
      if (!/^r8_[A-Za-z0-9]{20,}$/.test(t)) return { ok: false, error: 'That does not look like a Replicate token (r8_…).' };
      await idbSet('secrets', 'replicate', t);
      return { ok: true };
    }
    case 'clearToken': await idbDelete('secrets', 'replicate'); return { ok: true };
    case 'tokenStatus': { const t = await getToken(); return { set: !!t, hint: t ? `r8_…${t.slice(-4)}` : null }; }
    case 'testToken': {
      const t = await getToken();
      if (!t) return { ok: false, error: 'No token saved.' };
      try {
        const res = await fetch('https://api.replicate.com/v1/account', { headers: { Authorization: `Bearer ${t}` }, credentials: 'omit' });
        if (!res.ok) return { ok: false, error: `Replicate answered ${res.status}` };
        const j = (await res.json()) as { username?: string };
        return { ok: true, username: j.username ?? '' };
      } catch (e) { return { ok: false, error: String((e as Error).message) }; }
    }
    case 'history': return idbAll('history');
    case 'clearHistory': await idbClear('history'); return true;
    case 'clearCache': await idbClear('cache'); return true;
  }
  return null;
}

chrome.runtime.onMessage.addListener((msg, sender, sendResponse) => {
  // Only the extension's own pages and content scripts can reach this listener.
  if (sender.id !== chrome.runtime.id) return false;
  // Pages other than the options page may not manage the token.
  // Content scripts report the web page's URL; only the extension's own pages have its origin.
  const fromExtensionPage = !!sender.url?.startsWith(chrome.runtime.getURL(''));
  if (['setToken', 'clearToken', 'testToken', 'tokenStatus', 'setSiteMode', 'history', 'clearHistory', 'clearCache'].includes((msg as { type: string }).type) && !fromExtensionPage) { sendResponse(null); return false; }
  onRequest(msg, sender).then(sendResponse, e => sendResponse({ error: String(e) }));
  return true;
});

// Fresh page loads start fresh page budgets.
chrome.tabs.onRemoved.addListener(() => { if (perPage.size > 500) perPage.clear(); });
