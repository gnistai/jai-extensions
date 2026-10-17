// End-to-end test of the JAI reference viewer in real Chrome.
// Run with Node next to Chrome (puppeteer-core):
//   node driver.mjs <chrome.exe> <unpacked-extension-dir> <base-url> <env-file-with-REPLICATE_API_TOKEN> <out-dir>
// It renders two real images (identical documents on the page share one job via
// the viewer's in-flight de-duplication and cache) and runs one tiny encode:
// about $0.03 in all.
import puppeteer from 'puppeteer-core';
import { readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';

const [chrome, extDir, base, envFile, out] = process.argv.slice(2);
const token = /^REPLICATE_API_TOKEN=(.*)$/m.exec(readFileSync(envFile, 'utf8'))?.[1]?.trim().replace(/^["']|["']$/g, '');
if (!token) throw new Error('no token in env file');
const results = [];
const check = (name, ok, detail = '') => { results.push({ name, ok: !!ok, detail }); console.log(`${ok ? 'PASS' : 'FAIL'} ${name}${detail ? ' — ' + detail : ''}`); };
const sleep = ms => new Promise(r => setTimeout(r, ms));
/** page.html's alt-only image: JAI text in an alt, with no data-jai, so not a carrier. */
const ALT_ONLY = 'A red lighthouse on a rocky island at sunset;style=watercolor';

const browser = await puppeteer.launch({
  executablePath: chrome, headless: true, userDataDir: join(out, 'profile'),
  ignoreDefaultArgs: ['--disable-extensions'],
  args: ['--disable-features=DisableLoadExtensionCommandLineSwitch', `--load-extension=${extDir}`, `--disable-extensions-except=${extDir}`, '--no-first-run'],
});
try {
  const swTarget = await browser.waitForTarget(t => t.type() === 'service_worker' && t.url().endsWith('/background.js'), { timeout: 15000 });
  const extId = new URL(swTarget.url()).host;
  check('extension loaded', extId, extId);
  const sw = await swTarget.worker();

  // Save the token through the options page, as a user would.
  const opt = await browser.newPage();
  await opt.goto(`chrome-extension://${extId}/options.html`);
  await opt.waitForSelector('#token');
  await opt.type('#token', token);
  await opt.click('#save-token');
  await opt.waitForFunction(() => /Saved|token is saved/.test(document.querySelector('#token-status').textContent), { timeout: 5000 });
  const tokenInStorage = await sw.evaluate(async () => JSON.stringify(await chrome.storage.local.get(null)).includes('r8_'));
  check('token is not in chrome.storage (content scripts can read that)', !tokenInStorage);
  await opt.click('#test-token');
  await opt.waitForFunction(() => /works|failed/.test(document.querySelector('#token-status').textContent), { timeout: 15000 });
  check('token test against Replicate', /works/.test(await opt.$eval('#token-status', e => e.textContent)));
  await opt.close();

  const page = await browser.newPage();
  await page.setViewport({ width: 900, height: 1400 });
  await page.goto(`${base}/page.html`, { waitUntil: 'load' });
  const origin = new URL(base).origin;
  await page.screenshot({ path: join(out, 'before.png'), fullPage: true });
  const sizeOf = () => page.evaluate(() => ['picture-img', 'attribute-alt', 'src-data', 'alt-only'].map(id => { const r = document.getElementById(id).getBoundingClientRect(); return `${r.width}x${r.height}`; }));
  const sizesBefore = await sizeOf();

  const tabId = await sw.evaluate(async url => (await chrome.tabs.query({ url: url + '*' }))[0].id, `${base}/page.html`);
  // Inject the content script (the popup does this on the user's click via activeTab).
  await sw.evaluate(async id => { await chrome.scripting.executeScript({ target: { tabId: id }, files: ['content.js'] }); }, tabId);
  await sleep(500);
  const list = await sw.evaluate(async id => chrome.tabs.sendMessage(id, { type: 'list' }), tabId);
  check('discovers the three carriers and ignores an alt without data-jai', list.length === 6 && ['picture', 'attribute', 'src'].every(c => list.some(i => i.carrier === c)), JSON.stringify(list.map(i => i.carrier)));
  check('a src .jai file is not fetched before use', list.filter(i => i.carrier === 'src' && i.jai === null).length === 1, JSON.stringify(list.map(i => [i.carrier, i.jai && i.jai.slice(0, 20)])));
  // The privileged route, on a second host the test copy grants: a cross-origin .jai resource comes back as bytes,
  // and any other path on the same host is declined ({ page: true }) without a fetch.
  const other = new URL(base); other.hostname = other.hostname === 'localhost' ? '127.0.0.1' : 'localhost';
  const probe = await sw.evaluate(async (id, o) => ({
    granted: await chrome.permissions.contains({ origins: [o + '/*'] }),
    ...(await chrome.scripting.executeScript({ target: { tabId: id }, args: [o], func: async o => ({ jai: await chrome.runtime.sendMessage({ type: 'fetchJai', url: o + '/lighthouse.jai' }), html: await chrome.runtime.sendMessage({ type: 'fetchJai', url: o + '/page.html' }) }) }))[0].result,
  }), tabId, other.origin);
  check('the privileged fetch declines anything but a .jai path', probe.granted && typeof probe.jai?.b64 === 'string' && probe.html?.page === true && !probe.html.b64, JSON.stringify({ granted: probe.granted, jai: probe.jai && Object.keys(probe.jai), html: probe.html }));
  // attribute-alt takes its prompt from the alt, attribute-own from data-jai (its alt plays no part): the same document as picture-img and src-data.
  check('documents parsed; alt is the prompt unless data-jai gives one', list.filter(i => i.jai === 'A red lighthouse on a rocky island at sunset;style=watercolor').length === 4 && list.some(i => i.jai === 'A red lighthouse on a rocky island at sunset;prompt/flux=seen from the sea;prompt/sdxl=from a cliff top;aspect=1:1;style=watercolor'), JSON.stringify(list.map(i => i.jai)));
  check('no image changed before the user asks', await page.evaluate(() => [...document.images].every(i => !i.getAttribute('srcset') && !i.currentSrc.startsWith('data:image/webp'))));

  // Rendering is refused until the provider notice is accepted for the site.
  await sw.evaluate(async o => { const { settings = {} } = await chrome.storage.local.get('settings'); settings.disclosed = { ...(settings.disclosed || {}), [o]: true }; await chrome.storage.local.set({ settings }); }, origin);
  await sw.evaluate(async id => chrome.tabs.sendMessage(id, { type: 'renderAll' }), tabId);
  const t0 = Date.now();
  const ok = await page.waitForFunction(() => ['picture-img', 'attribute-alt', 'src-data', 'src-aspect', 'src-file', 'attribute-own'].every(id => { const i = document.getElementById(id); return /^data:image\/(webp|png|jpeg)/.test(i.currentSrc) && i.complete && i.naturalWidth > 0; }), { timeout: 120000, polling: 500 }).then(() => true, () => false);
  if (!ok) {
    const l2 = await sw.evaluate(async id => chrome.tabs.sendMessage(id, { type: 'list' }), tabId);
    const imgs = await page.evaluate(() => ['picture-img', 'attribute-alt', 'src-data', 'src-aspect', 'src-file', 'attribute-own'].map(id => { const i = document.getElementById(id); return { id, currentSrc: i.currentSrc.slice(0, 40), srcset: (i.getAttribute('srcset') || '').slice(0, 30), complete: i.complete, w: i.naturalWidth }; }));
    throw new Error('render did not appear: ' + JSON.stringify({ list: l2, imgs }));
  }
  check('all six JAI images replaced, including the .jai file in src', true, `${Math.round((Date.now() - t0) / 1000)}s`);
  const st = await page.evaluate(() => ({
    pictureSource: document.getElementById('picture').firstElementChild.localName === 'source' && document.getElementById('picture').firstElementChild.srcset.startsWith('data:image/'),
    pictureImgSrc: document.getElementById('picture-img').getAttribute('src'),
    alts: ['picture-img', 'attribute-alt', 'src-data', 'alt-only', 'src-file', 'attribute-own'].map(id => document.getElementById(id).getAttribute('alt')),
    desc: ['picture-img', 'attribute-alt', 'src-data', 'attribute-own'].map(id => !!document.getElementById(id).getAttribute('aria-description')),
    altOnly: { src: document.getElementById('alt-only').currentSrc.endsWith('original.png'), srcset: document.getElementById('alt-only').hasAttribute('srcset'), desc: document.getElementById('alt-only').hasAttribute('aria-description') },
    plain: document.getElementById('plain').currentSrc.endsWith('original.png') && !document.getElementById('plain').getAttribute('srcset'),
    sizes: ['picture-img', 'attribute-alt', 'src-data', 'alt-only'].map(id => { const r = document.getElementById(id).getBoundingClientRect(); return `${r.width}x${r.height}`; }),
  }));
  check('picture carrier replaced by a viewer <source>, src untouched', st.pictureSource && st.pictureImgSrc === 'original.png');
  check('human alt kept everywhere', st.alts[0] === 'A placeholder' && st.alts[1] === 'A red lighthouse on a rocky island at sunset' && st.alts[2] === 'A red lighthouse on a rocky island at sunset' && st.alts[3] === ALT_ONLY && st.alts[4] === 'Lighthouse at sunset' && st.alts[5] === 'Lighthouse', JSON.stringify(st.alts));
  check('JAI text in an alt without data-jai is never rendered', st.altOnly.src && !st.altOnly.srcset && !st.altOnly.desc, JSON.stringify(st.altOnly));
  check('AI-rendered state exposed', st.desc.every(Boolean));
  check('plain image untouched', st.plain);
  check('layout boxes preserved', st.sizes.join() === sizesBefore.join(), `${sizesBefore.join(' ')} -> ${st.sizes.join(' ')}`);
  const history = await sw.evaluate(async () => new Promise(r => { const q = indexedDB.open('jai-viewer'); q.onsuccess = () => { const g = q.result.transaction('history').objectStore('history').getAll(); g.onsuccess = () => r(g.result); }; }));
  check('identical documents shared one prediction', history.length <= 2, `${history.length} provider job(s) in this profile (5 identical, one of them a .jai file, + 1 distinct document)`);
  const aspect = await page.evaluate(() => { const i = document.getElementById('src-aspect'); const r = i.getBoundingClientRect(); return { natural: i.naturalWidth / i.naturalHeight, fit: getComputedStyle(i).objectFit, box: `${r.width}x${r.height}` }; });
  check('sized 3:2 box with aspect=1:1: rendered near 3:2 and never stretched', Math.abs(Math.log(aspect.natural / 1.5)) < 0.08 && aspect.fit !== 'fill', JSON.stringify(aspect));
  const sent = await sw.evaluate(async () => new Promise(r => { const q = indexedDB.open('jai-viewer'); q.onsuccess = () => { const g = q.result.transaction('cache').objectStore('cache').getAll(); g.onsuccess = () => r(g.result.map(e => e.sentPrompt)); }; }));
  check('flux render got the flux refinement only', sent.some(p => p.includes('seen from the sea')) && !sent.some(p => p.includes('cliff top')), JSON.stringify(sent));
  await page.screenshot({ path: join(out, 'after.png'), fullPage: true });

  await sw.evaluate(async id => chrome.tabs.sendMessage(id, { type: 'restoreAll' }), tabId);
  await sleep(800);
  const restored = await page.evaluate(() => ({
    src: ['picture-img', 'attribute-alt', 'alt-only', 'attribute-own'].every(id => document.getElementById(id).currentSrc.endsWith('original.png')),
    srcset: ['picture-img', 'attribute-alt', 'src-data', 'alt-only', 'attribute-own'].every(id => !document.getElementById(id).hasAttribute('srcset')),
    altOnly: document.getElementById('alt-only').getAttribute('alt'),
    sources: document.getElementById('picture').querySelectorAll('source').length,
  }));
  check('restore puts every original back', restored.src && restored.srcset && restored.sources === 1 && restored.altOnly === ALT_ONLY, JSON.stringify(restored));

  // Strict CSP page (img-src 'self'). Chrome exempts content-script loads from the page CSP; other engines may not,
  // in which case the viewer must paint a canvas. Either way the render must be visible. (Cache hit, no cost.)
  const csp = await browser.newPage();
  await csp.setViewport({ width: 900, height: 600 });
  await csp.goto(`${base}/csp.html`, { waitUntil: 'load' });
  const cspTab = await sw.evaluate(async url => (await chrome.tabs.query({ url }))[0].id, `${base}/csp.html`);
  await sw.evaluate(async id => { await chrome.scripting.executeScript({ target: { tabId: id }, files: ['content.js'] }); }, cspTab);
  await sleep(400);
  await sw.evaluate(async id => chrome.tabs.sendMessage(id, { type: 'renderAll' }), cspTab);
  await csp.waitForFunction(() => document.querySelector('canvas[role=img]') || /^data:image\//.test(document.getElementById('attribute-alt').currentSrc) && document.getElementById('attribute-alt').naturalWidth > 0, { timeout: 30000 }).catch(() => null);
  const cspState = await csp.evaluate(() => { const i = document.getElementById('attribute-alt'); return { viaImg: /^data:image\//.test(i.currentSrc) && i.naturalWidth > 0, viaCanvas: !!document.querySelector('canvas[role=img]') && i.style.display === 'none' }; });
  check('strict CSP page: render visible (image or canvas fallback)', cspState.viaImg || cspState.viaCanvas, JSON.stringify(cspState));
  const srcFile = await csp.waitForFunction(() => { const i = document.getElementById('src-file'); return (/^data:image\//.test(i.currentSrc) && i.naturalWidth > 0) || (i.style.display === 'none' && i.nextElementSibling?.localName === 'canvas'); }, { timeout: 30000 }).then(() => true, () => false);
  check('strict CSP page: a .jai file in src renders too (no original pixels)', srcFile);
  await csp.screenshot({ path: join(out, 'csp.png') });

  // A content script must not be able to use the encoder's provider port.
  const portProbe = await sw.evaluate(async id => (await chrome.scripting.executeScript({ target: { tabId: id }, func: () => new Promise(res => {
    const p = chrome.runtime.connect({ name: 'encode' });
    let replied = false;
    p.onMessage.addListener(() => { replied = true; });
    p.onDisconnect.addListener(() => res({ disconnected: true, replied }));
    p.postMessage({ type: 'predict', id: 1, model: { id: 'black-forest-labs/flux-schnell' }, input: { prompt: 'x' } });
    setTimeout(() => res({ disconnected: false, replied }), 3000);
  }) }))[0].result, cspTab);
  check('content scripts cannot use the encoder port', portProbe.disconnected && !portProbe.replied, JSON.stringify(portProbe));

  // Encode page: a tiny real run (one generation of two candidates, two seeds, FLUX schnell, GPT-5.6 luna measuring): about $0.03.
  const png = Buffer.from(await (await fetch(`${base}/original.png`)).arrayBuffer());
  writeFileSync(join(out, 'encode-target.png'), png);
  const enc = await browser.newPage();
  await enc.setViewport({ width: 1000, height: 1400 });
  await enc.goto(`chrome-extension://${extId}/encode.html`);
  await (await enc.$('#file')).uploadFile(join(out, 'encode-target.png'));
  await enc.waitForFunction(() => /pixels/.test(document.getElementById('dims').textContent), { timeout: 10000 });
  check('encode page: start needs consent', await enc.$eval('#start', b => b.disabled));
  await enc.click('#consent');
  await enc.$eval('#budget', i => { i.value = '0.10'; });
  await enc.$eval('#population', i => { i.value = '2'; });
  await enc.$eval('#generations', i => { i.value = '1'; });
  await enc.click('#start');
  await enc.waitForFunction(() => !document.getElementById('result-card').hidden || /Encoding failed/.test(document.getElementById('log').textContent), { timeout: 240000 });
  const encState = await enc.evaluate(() => ({ text: document.getElementById('result-text').textContent, note: document.getElementById('result-note').textContent, log: document.getElementById('log').textContent, renders: document.querySelectorAll('.cand img').length }));
  check('encode page: tiny real run returns a JAI document as canonical text', /^(?!jai:).+;aspect=3:2;(?:.*;)?model=black-forest-labs\/flux-schnell(?:;|$)/.test(encState.text) && encState.renders === 4, JSON.stringify(encState).slice(0, 400));
  const spent = Number(/\$(\d+\.\d+) in/.exec(encState.note)?.[1] ?? NaN);
  check('encode page: estimated spend within the limit', spent > 0 && spent <= 0.10, encState.note);
  await enc.screenshot({ path: join(out, 'encode.png'), fullPage: true });
} catch (e) {
  check('driver', false, String(e && e.stack || e));
} finally {
  await browser.close();
  writeFileSync(join(out, 'results.json'), JSON.stringify(results, null, 1));
  const failed = results.filter(r => !r.ok).length;
  console.log(failed ? `${failed} check(s) failed` : `all ${results.length} checks passed`);
  process.exitCode = failed ? 1 : 0;
}
