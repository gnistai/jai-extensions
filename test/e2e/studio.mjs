// End-to-end test of the JAI reference viewer with JAI Studio as its renderer, in real Chrome.
// Run with Node next to Chrome (puppeteer-core):
//   node studio.mjs <chrome> <unpacked-extension-copy> <this-folder> <out-dir> [--real]
// The extension copy needs "http://127.0.0.1/*" and "http://localhost:8767/*" in host_permissions.
// This script serves the test pages on http://localhost:8767 itself.
//
// Without --real it starts test/fake-studio.mjs on JAI Studio's first free port (no GPU, no
// cost) and also checks a refused token, a reply that takes longer than Chrome lets an idle
// worker live, and a stopped Studio; the encode page runs either way. With --real it uses the JAI Studio running
// on this computer, reading the token from Studio's data folder (never printed), in a visible
// window, so a browser prompt (Local Network Access) would show.
//
// Puppeteer never attaches to the extension's service worker: an attached debugger keeps a
// worker alive, which would hide exactly what the slow-reply check is for. Extension APIs are
// called from the options page instead, and the worker is watched with Target.getTargets.
import puppeteer from 'puppeteer-core';
import { createServer } from 'node:http';
import { readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { pathToFileURL } from 'node:url';

const [chrome, extDir, pages, out, flag] = process.argv.slice(2);
const real = flag === '--real';
const { startFakeStudio, solidPNG } = await import(pathToFileURL(join(pages, '..', 'fake-studio.mjs')).href);
const results = [];
const check = (name, ok, detail = '') => { results.push({ name, ok: !!ok, detail }); console.log(`${ok ? 'PASS' : 'FAIL'} ${name}${detail ? ' — ' + detail : ''}`); };
const sleep = ms => new Promise(r => setTimeout(r, ms));
const BASE = 'http://localhost:8767';
const SIX = ['picture-img', 'attribute-alt', 'src-data', 'src-aspect', 'src-file', 'attribute-own'];

// The test pages, plus a transform document whose original the page can read.
const TRANSFORM = '<!doctype html><title>transform</title><img id="t" src="original.png" alt="A castle" width="300" height="200" data-jai="A castle on a hill at dusk;mode=transform">';
const site = createServer((req, res) => {
  const path = new URL(req.url, BASE).pathname;
  if (path === '/transform.html') { res.writeHead(200, { 'content-type': 'text/html' }); return res.end(TRANSFORM); }
  const types = { '.html': 'text/html', '.png': 'image/png', '.jai': 'application/octet-stream' };
  const ext = /\.[a-z]+$/.exec(path)?.[0] ?? '';
  try { const body = readFileSync(join(pages, path.replace(/^\/+/, ''))); res.writeHead(200, { 'content-type': types[ext] ?? 'application/octet-stream' }); res.end(body); }
  catch { res.writeHead(404); res.end(); }
});
await new Promise(r => site.listen(8767, 'localhost', r));

const token = real ? readFileSync(join(process.env.LOCALAPPDATA, 'JAI', 'token'), 'utf8').trim() : `jai_fake-${Date.now().toString(36)}-0123456789abcdef`;
let fake = real ? null : await startFakeStudio({ token, ports: [5240, 5241, 5242, 5243, 5244, 5245, 5246, 5247, 5248, 5249] });

const browser = await puppeteer.launch({
  executablePath: chrome, headless: !real, userDataDir: join(out, 'profile'),
  ignoreDefaultArgs: ['--disable-extensions'],
  args: ['--disable-features=DisableLoadExtensionCommandLineSwitch', `--load-extension=${extDir}`, `--disable-extensions-except=${extDir}`, '--no-first-run'],
  targetFilter: t => t.type !== 'service_worker',
});
const cdp = await browser.target().createCDPSession();
/** The extension's worker as the browser lists it: running or not, and whether a debugger is attached. */
const worker = async () => (await cdp.send('Target.getTargets')).targetInfos.find(t => t.type === 'service_worker' && t.url.endsWith('/background.js')) ?? null;
try {
  let w = null;
  for (let i = 0; i < 50 && (!w || w.attached); i++) { w = await worker(); if (!w || w.attached) await sleep(200); }
  const extId = w ? new URL(w.url).host : null;
  check('extension loaded; no debugger on its worker', extId && !w.attached, `${extId ?? 'no worker'}, attached=${w?.attached}`);

  const opt = await browser.newPage();
  await opt.goto(`chrome-extension://${extId}/options.html`);
  await opt.waitForFunction(() => !/Checking/.test(document.getElementById('studio-status').textContent));
  const send = m => opt.evaluate(msg => chrome.runtime.sendMessage(msg), m);
  const radios = () => opt.evaluate(() => ['use-studio', 'use-replicate'].map(id => { const r = document.getElementById(id); return { disabled: r.disabled, checked: r.checked }; }));
  /** Waits until the Render with radios show [studio, replicate] as expected (they update after the status line). */
  const radiosAre = async want => opt.waitForFunction(w => JSON.stringify(['use-studio', 'use-replicate'].map(id => { const r = document.getElementById(id); return { disabled: r.disabled, checked: r.checked }; })) === w, { timeout: 15000 }, JSON.stringify(want)).then(() => true, () => false);
  check('Render with: nothing to choose before a renderer is set up', await radiosAre([{ disabled: true, checked: false }, { disabled: true, checked: true }]), JSON.stringify(await radios()));

  const connect = async t => {
    await opt.$eval('#studio-token', i => { i.value = ''; });
    await opt.type('#studio-token', t);
    await opt.click('#studio-connect');
    await opt.waitForFunction(() => /Connected to|No JAI Studio|does not look|permission/.test(document.getElementById('studio-status').textContent), { timeout: 15000 });
    return opt.$eval('#studio-status', e => e.textContent);
  };
  if (!real) {
    const refused = await connect('jai_not-the-token-of-this-studio-000');
    check('connect: a token no JAI Studio here holds is refused and not saved', /No JAI Studio with this token answers/.test(refused) && !(await send({ type: 'studioStatus' })).saved, refused);
    check('connect: only the proof was asked for, never with a token', fake.requests.length > 0 && fake.requests.every(q => q.url.startsWith('/health?nonce=') && !q.headers.authorization), JSON.stringify(fake.requests.map(q => q.url.slice(0, 20))));
  }
  const connected = await connect(token);
  check('connect: JAI Studio found and chosen', /Connected to JAI Studio \d/.test(connected) && await radiosAre([{ disabled: false, checked: true }, { disabled: true, checked: false }]), `${connected} ${JSON.stringify(await radios())}`);
  const leaked = await opt.evaluate(async t => JSON.stringify(await chrome.storage.local.get(null)).includes(t), token);
  check('the Studio token is not in chrome.storage (content scripts can read that)', !leaked);
  const state = await send({ type: 'state', origin: BASE });
  check('state: JAI Studio ready, no provider notice pending, one free model', state.renderer === 'studio' && state.ready && state.disclosed && state.models.length === 1 && state.models[0].label === 'FLUX.1 schnell · JAI Studio' && state.models[0].costHint === 0, JSON.stringify(state));

  // Render page.html through Studio: no provider notice to accept, nothing stored about the site.
  let opened = 0;
  const openAndRender = async (where, ids, timeout = 300000) => {
    const url = `${where}?run=${++opened}`;   // a tab of its own for each run
    const page = await browser.newPage();
    await page.setViewport({ width: 900, height: 1400 });
    await page.goto(url, { waitUntil: 'load' });
    const tabId = await opt.evaluate(async u => (await chrome.tabs.query({ url: u }))[0].id, url);
    await opt.evaluate(async id => { await chrome.scripting.executeScript({ target: { tabId: id }, files: ['content.js'] }); }, tabId);
    await sleep(500);
    const t0 = Date.now();
    await opt.evaluate(async id => chrome.tabs.sendMessage(id, { type: 'renderAll' }), tabId);
    const done = await page.waitForFunction(list => list.every(id => { const i = document.getElementById(id); return /^data:image\/(webp|png|jpeg)/.test(i.currentSrc) && i.complete && i.naturalWidth > 0; }), { timeout, polling: 500 }, ids).then(() => true, () => false);
    const list = await opt.evaluate(async id => chrome.tabs.sendMessage(id, { type: 'list' }), tabId);
    return { page, tabId, done, seconds: Math.round((Date.now() - t0) / 1000), list };
  };
  const idb = store => opt.evaluate(s => new Promise(r => { const q = indexedDB.open('jai-viewer'); q.onsuccess = () => { const g = q.result.transaction(s).objectStore(s).getAll(); g.onsuccess = () => r(g.result); }; }), store);

  const first = await openAndRender(`${BASE}/page.html`, SIX);
  check('all six JAI images rendered by JAI Studio, with no notice accepted', first.done, `${first.seconds}s ${first.done ? '' : JSON.stringify(first.list)}`);
  await first.page.screenshot({ path: join(out, 'studio-page.png'), fullPage: true });
  const settings = await opt.evaluate(async () => (await chrome.storage.local.get('settings')).settings);
  check('no provider notice stored for the site', !settings.disclosed?.[BASE], JSON.stringify(settings.disclosed));
  const history = await idb('history');
  check('history: renders by JAI Studio at no cost; identical documents shared one render', history.length >= 1 && history.length <= 2 && history.every(h => h.renderer === 'studio' && h.costHint === 0 && h.model === 'black-forest-labs/flux-schnell'), JSON.stringify(history.map(h => [h.renderer, h.costHint])));
  const sent = (await idb('cache')).map(e => e.sentPrompt);
  check('the flux refinement only, folded the same way as for Replicate', sent.some(p => p.includes('seen from the sea')) && !sent.some(p => p.includes('cliff top')), JSON.stringify(sent));
  if (fake) {
    const images = fake.requests.filter(q => q.url === '/v1/images/generations');
    check('Studio got the token and the extension\'s origin (or none)', images.length >= 1 && images.length <= 2 && images.every(q => q.headers.authorization === `Bearer ${token}` && (q.headers.origin === undefined || q.headers.origin === `chrome-extension://${extId}`)), JSON.stringify(images.map(q => q.headers.origin ?? '(none)')));
    check('JPEG asked for, at the FLUX sizes', images.every(q => q.body.output_format === 'jpeg' && q.body.width % 64 === 0 && q.body.height % 64 === 0), JSON.stringify(images.map(q => q.body)));
  }

  // A transform document: Studio renders from the text and says so.
  const tr = await openAndRender(`${BASE}/transform.html`, ['t']);
  const trEntry = (await idb('cache')).find(e => e.sentPrompt.startsWith('A castle on a hill'));
  check('transform: rendered from the text, reported as degraded', tr.done && trEntry?.diagnostics.some(d => d.code === 'mode-degraded'), `${tr.seconds}s ${JSON.stringify(trEntry?.diagnostics ?? tr.list)}`);

  if (!real) {
    // A reply slower than an idle worker may live (30 s): the background keeps itself alive while it waits.
    await send({ type: 'clearCache' });
    fake.setDelay(70_000);
    let mid = null;
    const watch = sleep(40_000).then(async () => { mid = await worker(); });
    const slow = await openAndRender(`${BASE}/csp.html`, ['attribute-alt'], 150000);
    await watch;
    check('a 70 s reply arrives; the worker was alive and undebugged meanwhile', slow.done && slow.seconds >= 69 && mid && !mid.attached, `${slow.seconds}s, at 40 s: ${mid ? `running, attached=${mid.attached}` : 'not running'}`);
    fake.setDelay(0);
    // And nothing keeps it alive afterwards: an idle worker stops within about 30 s.
    await sleep(45_000);
    check('the worker stops when idle afterwards (no keep-alive left running)', !(await worker()));

    // A refused token (401 after the proof), then no Studio at all: the same advice, never HTTP details.
    const port = fake.port;
    await fake.close();
    fake = await startFakeStudio({ token: 'jai_a-new-token-after-regenerate-0000', proofToken: token, port });
    await send({ type: 'clearCache' });
    const refused = await openAndRender(`${BASE}/page.html`, SIX, 15000);
    const refusedErr = refused.list.find(i => i.error)?.error ?? '';
    check('a refused token: what to do, not the HTTP error', /JAI Studio isn't running, or it has a new token/.test(refusedErr) && !/HTTP/.test(refusedErr), refusedErr);
    await fake.close();
    fake = null;
    const gone = await openAndRender(`${BASE}/page.html`, SIX, 15000);
    const goneErr = gone.list.find(i => i.error)?.error ?? '';
    check('JAI Studio stopped: what to do', /JAI Studio isn't running/.test(goneErr), goneErr);
    fake = await startFakeStudio({ token, port });

    // A content script cannot reach Studio through the encoder's port.
    const probe = await opt.evaluate(async id => (await chrome.scripting.executeScript({ target: { tabId: id }, func: () => new Promise(res => {
      const p = chrome.runtime.connect({ name: 'encode' });
      let replied = false;
      p.onMessage.addListener(() => { replied = true; });
      p.onDisconnect.addListener(() => res({ disconnected: true, replied }));
      p.postMessage({ type: 'begin', renderer: 'studio' });
      p.postMessage({ type: 'studio', id: 1, path: '/v1/images/generations', body: { prompt: 'x', width: 256, height: 256 } });
      setTimeout(() => res({ disconnected: false, replied }), 3000);
    }) }))[0].result, gone.tabId);
    check('content scripts cannot use the encoder port for Studio', probe.disconnected && !probe.replied, JSON.stringify(probe));
  }

  // The encode page follows Render with: Studio, free, no spending limit or consent box.
  writeFileSync(join(out, 'encode-target.png'), real ? readFileSync(join(pages, 'original.png')) : solidPNG(600, 400, [90, 140, 200]));
  const enc = await browser.newPage();
  await enc.setViewport({ width: 1000, height: 1400 });
  await enc.goto(`chrome-extension://${extId}/encode.html`);
  await enc.waitForFunction(() => /JAI Studio/.test(document.getElementById('renderer-name').textContent));
  // The token note is settled once Studio has answered: its text is emptied only then.
  await enc.waitForFunction(() => document.getElementById('token-note-text').textContent === '', { timeout: 15000 });
  const shown = await enc.evaluate(() => ({ budget: !document.getElementById('budget').closest('label').hidden, consent: !document.getElementById('consent').closest('label').hidden, models: !document.getElementById('models').closest('fieldset').hidden, studioNote: !document.querySelector('.studio-only').hidden, tokenNote: !document.getElementById('token-note').hidden }));
  check('encode page: JAI Studio, no spending limit, analyst or consent box', !shown.budget && !shown.consent && !shown.models && shown.studioNote && !shown.tokenNote, JSON.stringify(shown));
  await (await enc.$('#file')).uploadFile(join(out, 'encode-target.png'));
  await enc.waitForFunction(() => /pixels/.test(document.getElementById('dims').textContent), { timeout: 10000 });
  check('encode page: ready to start without consent', !(await enc.$eval('#start', b => b.disabled)));
  await enc.$eval('#population', i => { i.value = '2'; });
  await enc.$eval('#generations', i => { i.value = '1'; });
  await enc.click('#start');
  await enc.waitForFunction(() => !document.getElementById('result-card').hidden || /Encoding failed/.test(document.getElementById('log').textContent), { timeout: real ? 900000 : 120000 });
  const encState = await enc.evaluate(() => ({ text: document.getElementById('result-text').textContent, note: document.getElementById('result-note').textContent, log: document.getElementById('log').textContent, renders: document.querySelectorAll('.cand img').length, failed: [...document.querySelectorAll('.cand .err')].map(e => e.title), spend: document.getElementById('spend').textContent }));
  check('encode page: a run on JAI Studio returns a JAI document', /(?:^|;)model=black-forest-labs\/flux-schnell(?:;|$)/.test(encState.text) && encState.renders === 4, JSON.stringify({ renders: encState.renders, failed: encState.failed, text: encState.text.slice(-160) }));
  check('encode page: the note names JAI Studio; no spend shown', /JAI Studio's local models/.test(encState.note) && !encState.spend && !/spent|\$/.test(encState.log), encState.note);
  if (fake) {
    const chats = fake.requests.filter(q => q.url === '/v1/chat/completions');
    check('encode page: Studio saw only data: images, with the token', chats.length > 0 && chats.every(q => q.headers.authorization === `Bearer ${token}` && q.body.messages.flatMap(m => Array.isArray(m.content) ? m.content : []).filter(p => p.type === 'image_url').every(p => p.image_url.url.startsWith('data:image/'))), `${chats.length} chat requests`);
  }
  await enc.screenshot({ path: join(out, 'studio-encode.png'), fullPage: true });

  // Remove: the token is gone, and the renderer stays JAI Studio, not set up: nothing switches to Replicate by itself.
  await opt.bringToFront();
  await opt.click('#studio-remove');
  await opt.waitForFunction(() => /Not connected/.test(document.getElementById('studio-status').textContent), { timeout: 10000 });
  const after = await send({ type: 'state', origin: BASE });
  check('remove: the token is gone; nothing renders, and nothing switches to Replicate', after.renderer === 'studio' && !after.ready && !(await send({ type: 'studioStatus' })).saved && await radiosAre([{ disabled: true, checked: true }, { disabled: true, checked: false }]), JSON.stringify({ renderer: after.renderer, ready: after.ready, radios: await radios() }));
} catch (e) {
  check('driver', false, String(e && e.stack || e));
} finally {
  await browser.close();
  await fake?.close();
  site.close();
  writeFileSync(join(out, 'results.json'), JSON.stringify(results, null, 1));
  const failed = results.filter(r => !r.ok).length;
  console.log(failed ? `${failed} check(s) failed` : `all ${results.length} checks passed`);
  process.exitCode = failed ? 1 : 0;
}
