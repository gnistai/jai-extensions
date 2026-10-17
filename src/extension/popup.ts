// Popup: scans the active tab on the user's gesture (activeTab), lists JAI
// images, and sets the per-site mode.
import type { SiteMode, StateReply } from './shared.ts';

const $ = <T extends HTMLElement>(id: string): T => document.getElementById(id) as T;
const send = <T>(m: unknown): Promise<T> => chrome.runtime.sendMessage(m) as Promise<T>;

document.getElementById('encode-link')?.addEventListener('click', e => { e.preventDefault(); void chrome.tabs.create({ url: chrome.runtime.getURL('encode.html') }); });

async function main(): Promise<void> {
  const [tab] = await chrome.tabs.query({ active: true, currentWindow: true });
  if (!tab?.id || !tab.url || !/^https?:/.test(tab.url)) { $('list').textContent = 'JAI works on http and https pages.'; return; }
  const origin = new URL(tab.url).origin;
  $('site').textContent = origin;
  const state = await send<StateReply>({ type: 'state', origin });
  $('token-note').hidden = state.hasToken;
  ($('mode') as HTMLSelectElement).value = state.mode;
  $('usage').textContent = `Today: ${state.usage.today} of ${state.usage.maxPerDay} renders · at most ${state.usage.maxPerPage} per page`;
  for (const id of ['open-options', 'options-link']) $(id).addEventListener('click', e => { e.preventDefault(); void chrome.runtime.openOptionsPage(); });

  try { await chrome.scripting.executeScript({ target: { tabId: tab.id }, files: ['content.js'] }); }
  catch { $('list').textContent = 'This page cannot be scanned.'; return; }
  const items = (await chrome.tabs.sendMessage(tab.id, { type: 'list' })) as Array<{ carrier: string; status: string; jai: string | null; error: string | null }>;
  // Render all renders only the images not rendered yet.
  const renderable = (items ?? []).filter(i => i.status === 'found').length;
  const list = $('list');
  list.textContent = '';
  if (!items?.length) { list.textContent = 'No JAI images found. Ordinary images are never touched.'; }
  else {
    const ol = document.createElement('ol');
    for (const i of items) {
      const li = document.createElement('li');
      const code = document.createElement('code');
      code.textContent = i.jai ?? '(.jai file, read when you open or render it)';
      li.append(code, document.createTextNode(` — ${i.status}`));
      if (i.error) { const e = document.createElement('div'); e.className = 'err'; e.textContent = i.error; li.append(e); }
      ol.append(li);
    }
    list.append(ol);
    const canRender = state.hasToken && state.mode !== 'never';
    $<HTMLButtonElement>('render-all').disabled = !canRender || !state.disclosed || !renderable;
    $<HTMLButtonElement>('restore-all').disabled = false;
    if (canRender && !state.disclosed) $('usage').textContent += ' · open an image’s JAI control once to accept the provider notice for this site';
  }
  $('render-all').addEventListener('click', async () => {
    if (!confirm(`Render ${renderable} image(s) with Replicate? This costs money under your account.`)) return;
    await chrome.tabs.sendMessage(tab.id!, { type: 'renderAll' });
    window.close();
  });
  $('restore-all').addEventListener('click', async () => { await chrome.tabs.sendMessage(tab.id!, { type: 'restoreAll' }); window.close(); });
  $('mode').addEventListener('change', async () => {
    const mode = ($('mode') as HTMLSelectElement).value as SiteMode;
    if (mode === 'auto') {
      // Automatic mode needs permission to run on this site without a click.
      const granted = await chrome.permissions.request({ origins: [origin + '/*'] });
      if (!granted) { ($('mode') as HTMLSelectElement).value = state.mode; return; }
    }
    await send({ type: 'setSiteMode', origin, mode });
  });
}
void main();
