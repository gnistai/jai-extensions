// Background service worker of the JAI reference viewer.
//
// The only context that ever holds the Replicate token or talks to a provider.
// Content scripts send it a canonical JAI document plus, for transform, pixels
// the page itself could read; it applies the user's consent and budgets,
// chooses among the models the user enabled, renders, and returns bytes.
import { identity, interpret, readJSON, sha256Hex, type CatalogModel } from '../jai/index.ts';
import { FAMILIES, REPLICATE_MODELS, ReplicateRenderer } from '../renderers/replicate.ts';
import { RenderError, type RenderIntent } from '../renderers/types.ts';
import { idbAll, idbClear, idbDelete, idbGet, idbSet } from './idb.ts';
import { b64decode, b64encode, getSettings, saveSettings, siteMode, today, type BgRequest, type RenderPortReply, type RenderPortRequest, type Settings, type StateReply } from './shared.ts';

// ---------------------------------------------------------------- token

const getToken = (): Promise<string | undefined> => idbGet<string>('secrets', 'replicate');

// ---------------------------------------------------------------- budgets and queue

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

async function usageToday(): Promise<number> {
  const { usage } = await chrome.storage.local.get('usage');
  const u = usage as { day: string; count: number } | undefined;
  return u && u.day === today() ? u.count : 0;
}
async function countUsage(): Promise<void> {
  await chrome.storage.local.set({ usage: { day: today(), count: (await usageToday()) + 1 } });
}

// ---------------------------------------------------------------- model choice

function catalog(s: Settings): CatalogModel[] {
  return REPLICATE_MODELS.filter(m => s.enabledModels.includes(m.id)).map(m => ({ id: m.id, aliases: m.aliases, family: m.family, imageInput: m.imageInput, default: m.id === s.defaultModel }));
}

// ---------------------------------------------------------------- render

interface CacheEntry { b64: string; mime: string; model: string; sentPrompt: string; sentNegative: string | null; sentInput: Record<string, unknown>; jobId: string | null; diagnostics: Array<{ code: string; key?: string }>; created: number }

async function handleRender(req: RenderPortRequest, post: (m: RenderPortReply) => void, signal: AbortSignal): Promise<void> {
  const s = await getSettings();
  const token = await getToken();
  if (!token) return post({ type: 'error', kind: 'config', message: 'Add your Replicate API token in the extension options first.' });
  const mode = siteMode(s, req.origin);
  if (mode === 'never') return post({ type: 'error', kind: 'config', message: 'JAI rendering is turned off for this site.' });
  if (!s.disclosed[req.origin]) return post({ type: 'error', kind: 'consent', message: 'Accept the provider notice for this site first.' });
  if (req.automatic && mode !== 'auto') return post({ type: 'error', kind: 'consent', message: 'Automatic rendering is not enabled for this site.' });

  const read = readJSON(req.canonicalJSON, { inContext: true });
  if (read.diagnostics.some(d => d.code === 'invalid-json')) return post({ type: 'error', kind: 'config', message: 'Invalid document.' });
  if (req.automatic && read.diagnostics.some(d => d.code === 'unsupported-version')) return post({ type: 'error', kind: 'consent', message: 'Unknown JAI version: not rendered automatically.' });

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
  // The ratio actually requested is part of the render configuration (spec: render cache key).
  const ratioKey = req.governingRatio ? req.governingRatio.toFixed(3) : (it.ratio ? it.ratio.toFixed(3) : '-');
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

  // Budgets (spec: Consent, budgets and cancellation).
  const pageCount = perPage.get(req.pageId) ?? 0;
  if (pageCount >= s.maxPerPage) { finish(null); return post({ type: 'error', kind: 'budget', message: `Page limit reached (${s.maxPerPage} renders). Change it in the options.` }); }
  if ((await usageToday()) >= s.maxPerDay) { finish(null); return post({ type: 'error', kind: 'budget', message: `Daily limit reached (${s.maxPerDay} renders). Change it in the options.` }); }
  perPage.set(req.pageId, pageCount + 1);

  post({ type: 'status', status: 'queued' });
  try {
    await acquire(s.maxConcurrent, signal);
  } catch {
    finish(null);
    return post({ type: 'error', kind: 'canceled', message: 'Canceled' });
  }
  const t0 = Date.now();
  try {
    await countUsage();
    const intent: RenderIntent = {
      description: merged.description, style: merged.style, avoid: merged.avoid,
      aspect: it.aspect ? { width: it.aspect.width, height: it.aspect.height } : null,
      mode: it.mode, strength: it.strength, seed: it.seed, animate: it.animate,
    };
    const renderer = new ReplicateRenderer(token, { foldAvoid: s.foldAvoid });
    const result = await renderer.render({
      intent, modelId, signal,
      inputImage: req.inputImage ? { bytes: b64decode(req.inputImage.b64), mime: req.inputImage.mime } : null,
      governingRatio: req.governingRatio, target: req.target,
      onStatus: st => post({ type: 'status', status: st }),
    });
    const interpreterCodes = idiag.filter(d => ['repeated-single', 'invalid-value', 'adjusted-value', 'ignored-value', 'invalid-image', 'unsupported-preview', 'no-matching-model', 'substituted-model', 'version-ignored'].includes(d.code));
    const diagnostics = [...interpreterCodes.map(d => ({ code: d.code, ...(d.key ? { key: d.key } : {}) })), ...result.diagnostics];
    const b64 = b64encode(result.bytes);
    const entry: CacheEntry = { b64, mime: result.mime, model: result.model, sentPrompt: result.sentPrompt, sentNegative: result.sentNegative, sentInput: result.sentInput, jobId: result.jobId, diagnostics, created: Date.now() };
    await idbSet('cache', key, entry);
    if (s.keepHistory) await idbSet('history', `${Date.now()}-${result.jobId ?? ''}`, { time: new Date().toISOString(), site: req.topSite, model: result.model, jobId: result.jobId, canonicalJSON: req.canonicalJSON, costHint: model.costHint });
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

chrome.runtime.onConnect.addListener(port => {
  if (port.name !== 'render' || !port.sender?.tab) return port.disconnect();
  const ac = new AbortController();
  let open = true;
  // The page went away or the user canceled: abort and cancel the provider job.
  port.onDisconnect.addListener(() => { open = false; ac.abort(); });
  port.onMessage.addListener((msg: RenderPortRequest | { type: 'cancel' }) => {
    if (msg.type === 'cancel') { ac.abort(); return; }
    if (msg.type !== 'render') return;
    void handleRender(msg, m => { if (open) port.postMessage(m); }, ac.signal);
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
chrome.runtime.onInstalled.addListener(() => { void syncRegistrations(); });
chrome.permissions.onRemoved.addListener(() => { void syncRegistrations(); });

// ---------------------------------------------------------------- messages

async function onRequest(msg: BgRequest | { type: 'found'; count: number }, sender: chrome.runtime.MessageSender): Promise<unknown> {
  switch (msg.type) {
    case 'state': {
      const s = await getSettings();
      const reply: StateReply = {
        hasToken: !!(await getToken()), mode: siteMode(s, msg.origin), disclosed: !!s.disclosed[msg.origin],
        models: REPLICATE_MODELS.map(m => ({ id: m.id, label: m.label, imageInput: m.imageInput, costHint: m.costHint, enabled: s.enabledModels.includes(m.id), description: m.description })),
        defaultModel: s.defaultModel, usage: { today: await usageToday(), maxPerDay: s.maxPerDay, maxPerPage: s.maxPerPage },
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
      // Privileged fallback for carrier 1 resources when the page's CORS fetch fails.
      // Works only where the user granted host permission; returns bytes, never executes anything.
      if (!/^https?:\/\//i.test(msg.url)) return { error: 'Only http(s) URLs' };
      try {
        const res = await fetch(msg.url, { credentials: 'omit', referrerPolicy: 'no-referrer' });
        if (!res.ok) return { error: `HTTP ${res.status}` };
        const buf = new Uint8Array(await res.arrayBuffer());
        if (buf.length > 16 * 1024 * 1024) return { error: 'Too large' };
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
  if (['setToken', 'clearToken', 'testToken', 'history', 'clearHistory', 'clearCache'].includes((msg as { type: string }).type) && !fromExtensionPage) { sendResponse(null); return false; }
  onRequest(msg, sender).then(sendResponse, e => sendResponse({ error: String(e) }));
  return true;
});

// Fresh page loads start fresh page budgets.
chrome.tabs.onRemoved.addListener(() => { if (perPage.size > 500) perPage.clear(); });
