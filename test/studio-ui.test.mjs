// JAI Studio's webview UI: the rules it shares with the app (src/studio/ui/rules.ts,
// built to dist/test/studio-ui-rules.js), and the texts of index.html that state them.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { ENCODE_LIMITS, IMAGE_FILE, IMAGE_FORMATS, JAI_FILE, checkEncodeSettings, checkPort, stopSentence, vcInstallNote } from '../dist/test/studio-ui-rules.js';

const HTML = readFileSync(new URL('../src/studio/ui/index.html', import.meta.url), 'utf8');
const DEFAULTS = { generations: '8', population: '4', seeds: '1, 2', restarts: '1', goal: '0.90', scale: '1', thinking: 'off' };
const check = o => checkEncodeSettings({ ...DEFAULTS, ...o });

test('encode settings: the defaults; render scale has the app\'s range, 0.25 to 1.25', () => {
  assert.deepEqual(check({}), { ok: true, settings: { generations: 8, population: 4, seeds: [1, 2], restarts: 1, goal: 0.9, scale: 1, thinking: 'off' } });
  assert.deepEqual(ENCODE_LIMITS.scale, [0.25, 1.25], 'studio/app/src/encoding.rs SCALE');
  for (const scale of ['0.25', '1.25', '0.5']) assert.equal(check({ scale }).ok, true, scale);
  for (const scale of ['0.2', '1.3', '2', '', 'big']) assert.deepEqual(check({ scale }), { ok: false, field: 'scale', message: 'Render scale: from 0.25 to 1.25.' }, scale);
});

test('encode settings: each field\'s range, the first bad field named', () => {
  assert.equal(check({ generations: '31' }).field, 'generations');
  assert.equal(check({ generations: '2.5' }).field, 'generations');
  assert.equal(check({ population: '10' }).field, 'population');
  assert.equal(check({ seeds: '1,2,3,4,5,6,7,8,9' }).field, 'seeds');
  assert.equal(check({ seeds: '-1' }).field, 'seeds');
  assert.equal(check({ seeds: '4294967296' }).field, 'seeds');
  assert.deepEqual(check({ seeds: ' 7 ,8 9' }).settings.seeds, [7, 8, 9]);
  assert.equal(check({ restarts: '4' }).field, 'restarts');
  assert.equal(check({ goal: '0' }).field, 'goal');
  assert.equal(check({ goal: '1.01' }).field, 'goal');
  assert.equal(check({ generations: '0', scale: '9' }).field, 'generations', 'the first field in the form');
  assert.equal(check({ thinking: 'propose' }).settings.thinking, 'propose');
  assert.equal(check({ thinking: 'sometimes' }).settings.thinking, 'off');
});

test('images: the formats the app decodes (PNG, JPEG, WebP, GIF); no BMP', () => {
  for (const f of ['a.png', 'b.JPG', 'c.jpeg', 'd.webp', 'e.gif']) assert.ok(IMAGE_FILE.test(f), f);
  for (const f of ['photo.bmp', 'x.tiff', 'doc.jai', 'png']) assert.ok(!IMAGE_FILE.test(f), f);
  assert.equal(IMAGE_FORMATS, 'PNG, JPEG, WebP or GIF');
  assert.ok(HTML.includes(`<p class="muted">${IMAGE_FORMATS}. `), 'the Encode drop zone names the same formats');
  assert.doesNotMatch(HTML, /BMP/);
  assert.match(HTML, /id="enc-scale" min="0\.25" max="1\.25"/);
});

test('documents: only .jai files, by name; no JSON or text files', () => {
  for (const f of ['castle.jai', 'C:\\Docs\\Castle.JAI']) assert.ok(JAI_FILE.test(f), f);
  for (const f of ['castle.json', 'castle.txt', 'castle.jai.png', 'jai']) assert.ok(!JAI_FILE.test(f), f);
});

test('stop sentence: why a run with a result stopped, with the error when it stopped on one', () => {
  assert.equal(stopSentence('goal', null), 'Stopped because the goal was reached.');
  assert.equal(stopSentence('error', undefined), 'Stopped because of an error.');
  assert.equal(stopSentence('error', 'Chat completion failed (HTTP 500): llama-server exited.'), 'Stopped because of an error: Chat completion failed (HTTP 500): llama-server exited.');
  assert.equal(stopSentence('canceled', 'ignored'), 'Stopped because you canceled.');
  assert.equal(stopSentence(undefined, 'x'), '');
});

test('settings port: a new port from 1024 to 65535; the stored one stays allowed, as the app accepts 1 to 65535', () => {
  assert.equal(checkPort('5240', 5240), 5240);
  assert.equal(checkPort(' 6000 ', 5240), 6000);
  for (const bad of ['80', '1023', '65536', '0', '-1', '', ' ', '52.5', 'port']) assert.equal(checkPort(bad, 5240), null, bad);
  assert.equal(checkPort('80', 80), 80, 'a hand-edited low port does not block saving other settings');
  assert.equal(checkPort('81', 80), null);
  assert.equal(checkPort('0', 0), null);
});

test('Server tab texts: the browser extension takes the token in its options and finds JAI Studio on its usual ports', () => {
  assert.match(HTML, /To use JAI Studio from the JAI browser extension, copy the token and paste it into the extension's options; the extension finds JAI Studio on ports 5240 to 5249\./);
  for (const id of ['srv-licenses-box', 'srv-cuda-note', 'set-config-problem']) assert.match(HTML, new RegExp(`id="${id}"`), id);
});

test('VC++ installer outcome: what the Models tab says while the runtime still looks out of date', () => {
  const note = (outcome, exitCode = null) => vcInstallNote({ outcome, exitCode });
  assert.deepEqual(note('declined'), { message: 'Nothing was installed. You can install it again.', kind: 'warn', watch: false });
  assert.equal(note('started').watch, true, 'no process to wait for: keep checking');
  assert.match(note('finished', 0).message, /If this warning stays, restart Windows/);
  assert.equal(note('finished', 0).watch, true);
  for (const code of [3010, 1641]) assert.deepEqual(note('finished', code), { message: 'Installed. Restart Windows to finish, then open JAI Studio again.', kind: 'warn', watch: true }, String(code));
  assert.equal(note('finished', 1602).watch, false);
  assert.match(note('finished', 1638).message, /newer version/);
  assert.deepEqual(note('finished', 1603), { message: 'The installer failed (exit code 1603). You can install it again.', kind: 'err', watch: false });
});
