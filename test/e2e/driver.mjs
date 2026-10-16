// End-to-end test of the JAI reference viewer in real Chrome.
// Run with Node next to Chrome (puppeteer-core):
//   node driver.mjs <chrome.exe> <unpacked-extension-dir> <base-url> <env-file-with-REPLICATE_API_TOKEN> <out-dir>
// It renders ONE real image (identical documents on the page share one job via
// the viewer's in-flight de-duplication and cache), so it costs one prediction.
import puppeteer from 'puppeteer-core';
import { readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';

const [chrome, extDir, base, envFile, out] = process.argv.slice(2);
const token = /^REPLICATE_API_TOKEN=(.*)$/m.exec(readFileSync(envFile, 'utf8'))?.[1]?.trim().replace(/^["']|["']$/g, '');
if (!token) throw new Error('no token in env file');
const results = [];
const check = (name, ok, detail = '') => { results.push({ name, ok: !!ok, detail }); console.log(`${ok ? 'PASS' : 'FAIL'} ${name}${detail ? ' — ' + detail : ''}`); };
const sleep = ms => new Promise(r => setTimeout(r, ms));

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
  const sizeOf = () => page.evaluate(() => ['c1', 'c2', 'c3', 'c4'].map(id => { const r = document.getElementById(id).getBoundingClientRect(); return `${r.width}x${r.height}`; }));
  const sizesBefore = await sizeOf();

  const tabId = await sw.evaluate(async url => (await chrome.tabs.query({ url: url + '*' }))[0].id, `${base}/page.html`);
  // Inject the content script (the popup does this on the user's click via activeTab).
  await sw.evaluate(async id => { await chrome.scripting.executeScript({ target: { tabId: id }, files: ['content.js'] }); }, tabId);
  await sleep(500);
  const list = await sw.evaluate(async id => chrome.tabs.sendMessage(id, { type: 'list' }), tabId);
  check('discovers exactly the four carriers', list.length === 4 && [1, 2, 3, 4].every(c => list.some(i => i.carrier === c)), JSON.stringify(list.map(i => i.carrier)));
  check('documents parsed', list.every(i => i.jai === 'jai:A red lighthouse on a rocky island at sunset;style=watercolor'));
  check('no image changed before the user asks', await page.evaluate(() => [...document.images].every(i => !i.getAttribute('srcset') && !i.currentSrc.startsWith('data:image/webp'))));

  // Rendering is refused until the provider notice is accepted for the site.
  await sw.evaluate(async o => { const { settings = {} } = await chrome.storage.local.get('settings'); settings.disclosed = { ...(settings.disclosed || {}), [o]: true }; await chrome.storage.local.set({ settings }); }, origin);
  await sw.evaluate(async id => chrome.tabs.sendMessage(id, { type: 'renderAll' }), tabId);
  const t0 = Date.now();
  const ok = await page.waitForFunction(() => ['c1', 'c2', 'c3', 'c4'].every(id => { const i = document.getElementById(id); return /^data:image\/(webp|png|jpeg)/.test(i.currentSrc) && i.complete && i.naturalWidth > 0; }), { timeout: 120000, polling: 500 }).then(() => true, () => false);
  if (!ok) {
    const l2 = await sw.evaluate(async id => chrome.tabs.sendMessage(id, { type: 'list' }), tabId);
    const imgs = await page.evaluate(() => ['c1', 'c2', 'c3', 'c4'].map(id => { const i = document.getElementById(id); return { id, currentSrc: i.currentSrc.slice(0, 40), srcset: (i.getAttribute('srcset') || '').slice(0, 30), complete: i.complete, w: i.naturalWidth }; }));
    throw new Error('render did not appear: ' + JSON.stringify({ list: l2, imgs }));
  }
  check('all four images replaced', true, `${Math.round((Date.now() - t0) / 1000)}s`);
  const st = await page.evaluate(() => ({
    p1First: document.getElementById('p1').firstElementChild.localName === 'source' && document.getElementById('p1').firstElementChild.srcset.startsWith('data:image/'),
    c1src: document.getElementById('c1').getAttribute('src'),
    alts: ['c1', 'c2', 'c3', 'c4'].map(id => document.getElementById(id).getAttribute('alt')),
    desc: ['c1', 'c2', 'c3', 'c4'].map(id => !!document.getElementById(id).getAttribute('aria-description')),
    plain: document.getElementById('plain').currentSrc.endsWith('original.png') && !document.getElementById('plain').getAttribute('srcset'),
    sizes: ['c1', 'c2', 'c3', 'c4'].map(id => { const r = document.getElementById(id).getBoundingClientRect(); return `${r.width}x${r.height}`; }),
  }));
  check('carrier 1 replaced by a viewer <source>, src untouched', st.p1First && st.c1src === 'original.png');
  check('human alt kept; alt carrier replaced by the description', st.alts[0] === 'A placeholder' && st.alts[1] === 'A placeholder' && st.alts[3] === 'A red lighthouse on a rocky island at sunset', JSON.stringify(st.alts));
  check('AI-rendered state exposed', st.desc.every(Boolean));
  check('plain image untouched', st.plain);
  check('layout boxes preserved', st.sizes.join() === sizesBefore.join(), `${sizesBefore.join(' ')} -> ${st.sizes.join(' ')}`);
  const history = await sw.evaluate(async () => new Promise(r => { const q = indexedDB.open('jai-viewer'); q.onsuccess = () => { const g = q.result.transaction('history').objectStore('history').getAll(); g.onsuccess = () => r(g.result); }; }));
  check('identical documents shared one prediction', history.length <= 1, `${history.length} provider job(s) in this profile`);
  await page.screenshot({ path: join(out, 'after.png'), fullPage: true });

  await sw.evaluate(async id => chrome.tabs.sendMessage(id, { type: 'restoreAll' }), tabId);
  await sleep(800);
  const restored = await page.evaluate(() => ({
    src: ['c1', 'c2', 'c4'].every(id => document.getElementById(id).currentSrc.endsWith('original.png')),
    srcset: ['c1', 'c2', 'c3', 'c4'].every(id => !document.getElementById(id).hasAttribute('srcset')),
    alt4: document.getElementById('c4').getAttribute('alt'),
    sources: document.getElementById('p1').querySelectorAll('source').length,
  }));
  check('restore puts every original back', restored.src && restored.srcset && restored.sources === 1 && restored.alt4.startsWith('jai:'), JSON.stringify(restored));

  // Strict CSP page (img-src 'self'). Chrome exempts content-script loads from the page CSP; other engines may not,
  // in which case the viewer must paint a canvas. Either way the render must be visible. (Cache hit, no cost.)
  const csp = await browser.newPage();
  await csp.setViewport({ width: 900, height: 600 });
  await csp.goto(`${base}/csp.html`, { waitUntil: 'load' });
  const cspTab = await sw.evaluate(async url => (await chrome.tabs.query({ url }))[0].id, `${base}/csp.html`);
  await sw.evaluate(async id => { await chrome.scripting.executeScript({ target: { tabId: id }, files: ['content.js'] }); }, cspTab);
  await sleep(400);
  await sw.evaluate(async id => chrome.tabs.sendMessage(id, { type: 'renderAll' }), cspTab);
  await csp.waitForFunction(() => document.querySelector('canvas[role=img]') || /^data:image\//.test(document.getElementById('c2').currentSrc) && document.getElementById('c2').naturalWidth > 0, { timeout: 30000 }).catch(() => null);
  const cspState = await csp.evaluate(() => { const i = document.getElementById('c2'); return { viaImg: /^data:image\//.test(i.currentSrc) && i.naturalWidth > 0, viaCanvas: !!document.querySelector('canvas[role=img]') && i.style.display === 'none' }; });
  check('strict CSP page: render visible (image or canvas fallback)', cspState.viaImg || cspState.viaCanvas, JSON.stringify(cspState));
  await csp.screenshot({ path: join(out, 'csp.png') });
} catch (e) {
  check('driver', false, String(e && e.stack || e));
} finally {
  await browser.close();
  writeFileSync(join(out, 'results.json'), JSON.stringify(results, null, 1));
  const failed = results.filter(r => !r.ok).length;
  console.log(failed ? `${failed} check(s) failed` : `all ${results.length} checks passed`);
  process.exitCode = failed ? 1 : 0;
}
