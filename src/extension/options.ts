// Options page: the renderer, JAI Studio's and Replicate's tokens (stored by the
// background in extension-private IndexedDB), enabled models, limits, per-site
// modes, history.
import { REPLICATE_MODELS } from '../renderers/replicate.ts';
import { STUDIO_ORIGINS, getSettings, saveSettings, type RendererChoice, type SiteMode, type StudioStatus } from './shared.ts';

const $ = <T extends HTMLElement>(id: string): T => document.getElementById(id) as T;
const send = <T>(m: unknown): Promise<T> => chrome.runtime.sendMessage(m) as Promise<T>;
const td = (text: string): HTMLTableCellElement => { const c = document.createElement('td'); c.textContent = text; return c; };

/** The Render with choice: each renderer can be chosen once it is set up. */
async function refreshRenderer(): Promise<void> {
  const [s, replicate, studio] = await Promise.all([getSettings(), send<{ set: boolean }>({ type: 'tokenStatus' }), send<StudioStatus>({ type: 'studioStatus' })]);
  const choices: Array<[RendererChoice, boolean]> = [['studio', studio.saved], ['replicate', replicate.set]];
  for (const [value, ready] of choices) {
    const radio = $<HTMLInputElement>(value === 'studio' ? 'use-studio' : 'use-replicate');
    radio.disabled = !ready;
    radio.checked = s.renderer === value;
  }
}

async function refreshToken(): Promise<void> {
  const s = await send<{ set: boolean; hint: string | null }>({ type: 'tokenStatus' });
  $('token-status').textContent = s.set ? `A token is saved (${s.hint}).` : 'No token saved.';
  await refreshRenderer();
}

/** Whether a JAI Studio token is saved, known before a click so that Connect can ask for the permission at once. */
let studioSaved = false;

async function refreshStudio(): Promise<void> {
  const st = await send<StudioStatus>({ type: 'studioStatus' });
  studioSaved = st.saved;
  $('studio-status').textContent = !st.saved ? 'Not connected.'
    : !st.permission ? 'The permission to reach JAI Studio was removed. Connect again.'
    : st.running ? `Connected to JAI Studio ${st.version}.`
    : 'Connected, but no JAI Studio with this token answers now. Start JAI Studio, or connect again with its new token.';
  await refreshRenderer();
}

async function main(): Promise<void> {
  await Promise.all([refreshToken(), refreshStudio()]);
  for (const id of ['use-studio', 'use-replicate']) {
    $<HTMLInputElement>(id).addEventListener('change', async e => { await saveSettings({ renderer: (e.target as HTMLInputElement).value as RendererChoice }); await refreshRenderer(); });
  }
  $('studio-connect').addEventListener('click', async () => {
    const input = $<HTMLInputElement>('studio-token');
    // Studio's own rule for a token (16 to 512 printable ASCII characters), checked before asking for anything.
    if (!/^[\x21-\x7e]{16,512}$/.test(input.value.trim())) { $('studio-status').textContent = 'That does not look like a JAI Studio token (jai_…). Copy it from JAI Studio.'; return; }
    // Asked in the click itself: Chrome grants permissions only during a user action.
    if (!(await chrome.permissions.request({ origins: STUDIO_ORIGINS }))) { $('studio-status').textContent = 'JAI Studio cannot be used without permission to reach this computer (127.0.0.1).'; return; }
    $('studio-status').textContent = 'Looking for JAI Studio…';
    const r = await send<{ ok: boolean; version?: string; error?: string }>({ type: 'studioConnect', token: input.value });
    if (r.ok) { input.value = ''; studioSaved = true; }
    // A first connection that failed gives the permission back (it opens every loopback port to the extension).
    else if (!studioSaved) await chrome.permissions.remove({ origins: STUDIO_ORIGINS }).catch(() => false);
    $('studio-status').textContent = r.ok ? `Connected to JAI Studio ${r.version}. Renders now use it.` : r.error ?? 'Could not connect.';
    await refreshRenderer();
  });
  $('studio-remove').addEventListener('click', async () => {
    await send({ type: 'studioDisconnect' });
    // Throws when the permission is a required one (as in the e2e test build).
    await chrome.permissions.remove({ origins: STUDIO_ORIGINS }).catch(() => false);
    await refreshStudio();
  });
  $('save-token').addEventListener('click', async () => {
    const r = await send<{ ok: boolean; error?: string }>({ type: 'setToken', token: $<HTMLInputElement>('token').value });
    $<HTMLInputElement>('token').value = '';
    $('token-status').textContent = r.ok ? 'Saved.' : r.error ?? 'Could not save.';
    if (r.ok) setTimeout(refreshToken, 800);
  });
  $('test-token').addEventListener('click', async () => {
    $('token-status').textContent = 'Testing…';
    const r = await send<{ ok: boolean; username?: string; error?: string }>({ type: 'testToken' });
    $('token-status').textContent = r.ok ? `The token works (Replicate account ${r.username}).` : `Test failed: ${r.error}`;
  });
  $('clear-token').addEventListener('click', async () => { await send({ type: 'clearToken' }); await refreshToken(); });

  const s = await getSettings();
  const tbody = $('models');
  for (const m of REPLICATE_MODELS) {
    const tr = document.createElement('tr');
    const use = document.createElement('input'); use.type = 'checkbox'; use.checked = s.enabledModels.includes(m.id);
    const def = document.createElement('input'); def.type = 'radio'; def.name = 'default'; def.checked = s.defaultModel === m.id;
    use.setAttribute('aria-label', `Use ${m.label}`);
    def.setAttribute('aria-label', `Make ${m.label} the default`);
    const c1 = document.createElement('td'); c1.append(use);
    const c2 = td(`${m.label} — ${m.description}${m.imageInput ? ' (accepts an input image)' : ''}`);
    const c3 = document.createElement('td'); c3.append(def);
    tr.append(c1, c2, c3, td(`~$${m.costHint}`));
    tbody.append(tr);
    use.addEventListener('change', async () => {
      const cur = await getSettings();
      const enabled = use.checked ? [...new Set([...cur.enabledModels, m.id])] : cur.enabledModels.filter(x => x !== m.id);
      await saveSettings({ enabledModels: enabled, defaultModel: enabled.includes(cur.defaultModel) ? cur.defaultModel : enabled[0] ?? '' });
    });
    def.addEventListener('change', async () => { if (def.checked) { use.checked = true; const cur = await getSettings(); await saveSettings({ defaultModel: m.id, enabledModels: [...new Set([...cur.enabledModels, m.id])] }); } });
  }
  const fold = $<HTMLInputElement>('fold-avoid'); fold.checked = s.foldAvoid;
  fold.addEventListener('change', () => { void saveSettings({ foldAvoid: fold.checked }); });

  $<HTMLInputElement>('max-page').value = String(s.maxPerPage);
  $<HTMLInputElement>('max-day').value = String(s.maxPerDay);
  $<HTMLInputElement>('max-conc').value = String(s.maxConcurrent);
  $('save-limits').addEventListener('click', async () => {
    const clamp = (v: string, lo: number, hi: number, d: number): number => { const n = Math.round(Number(v)); return Number.isFinite(n) ? Math.min(hi, Math.max(lo, n)) : d; };
    await saveSettings({ maxPerPage: clamp($<HTMLInputElement>('max-page').value, 1, 200, 10), maxPerDay: clamp($<HTMLInputElement>('max-day').value, 1, 2000, 50), maxConcurrent: clamp($<HTMLInputElement>('max-conc').value, 1, 4, 2) });
    $('limits-saved').hidden = false; setTimeout(() => { $('limits-saved').hidden = true; }, 1500);
  });

  const sites = $('sites');
  const origins = [...new Set([...Object.keys(s.sites), ...Object.keys(s.disclosed)])].sort();
  if (!origins.length) { const tr = document.createElement('tr'); tr.append(td('No site settings yet.')); sites.append(tr); }
  for (const o of origins) {
    const tr = document.createElement('tr');
    const sel = document.createElement('select');
    for (const [v, l] of [['on-request', 'When I ask'], ['auto', 'Automatically'], ['never', 'Ignore']] as const) sel.append(new Option(l, v));
    sel.value = s.sites[o] ?? s.defaultSiteMode;
    sel.addEventListener('change', async () => {
      if (sel.value === 'auto' && !(await chrome.permissions.request({ origins: [o + '/*'] }))) { sel.value = s.sites[o] ?? 'on-request'; return; }
      await send({ type: 'setSiteMode', origin: o, mode: sel.value as SiteMode });
    });
    const forget = document.createElement('button'); forget.className = 'secondary'; forget.type = 'button'; forget.textContent = 'Forget';
    forget.addEventListener('click', async () => {
      const cur = await getSettings();
      const { [o]: _a, ...sitesRest } = cur.sites; const { [o]: _b, ...discRest } = cur.disclosed;
      await saveSettings({ sites: sitesRest, disclosed: discRest });
      await send({ type: 'setSiteMode', origin: o, mode: cur.defaultSiteMode });
      await chrome.permissions.remove({ origins: [o + '/*'] }).catch(() => false);
      tr.remove();
    });
    const c = document.createElement('td'); c.append(sel);
    const c4 = document.createElement('td'); c4.append(forget);
    tr.append(td(o), c, td(s.disclosed[o] ? 'yes' : 'no'), c4);
    sites.append(tr);
  }

  const kh = $<HTMLInputElement>('keep-history'); kh.checked = s.keepHistory;
  kh.addEventListener('change', () => { void saveSettings({ keepHistory: kh.checked }); });
  $('show-history').addEventListener('click', async () => {
    const h = await send<Array<{ time: string; site: string; renderer?: RendererChoice; model: string; jai: string; costHint: number }>>({ type: 'history' });
    const box = $('history'); box.textContent = '';
    if (!h.length) { box.textContent = 'No renders recorded.'; return; }
    const total = h.reduce((a, x) => a + (x.costHint || 0), 0);
    const p = document.createElement('p'); p.className = 'muted'; p.textContent = `${h.length} renders, roughly $${total.toFixed(2)}.`;
    box.append(p);
    for (const x of h.slice(-50).reverse()) { const pre = document.createElement('pre'); pre.textContent = `${x.time}  ${x.site}  ${x.renderer === 'studio' ? 'JAI Studio' : 'Replicate'}  ${x.model}\n${x.jai ?? ''}`; box.append(pre); }
  });
  $('clear-history').addEventListener('click', async () => { await send({ type: 'clearHistory' }); $('history').textContent = 'Cleared.'; });
  $('clear-cache').addEventListener('click', async () => { await send({ type: 'clearCache' }); $('history').textContent = 'Cached renders cleared.'; });
}
void main();
