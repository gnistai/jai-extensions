// dist/studio/host.mjs, the bundle JAI Studio runs inside QuickJS, exercised in
// Node: a fake globalThis.__jai stands in for the Rust host functions and a
// stubbed global fetch for the local server. No network, no GPU.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import * as host from '../dist/studio/host.mjs';
import * as J from '../dist/jai.mjs';

const SERVER = { url: 'http://127.0.0.1:5240', token: 'jai_host-test-token' };
const TARGET = { url: 'data:image/jpeg;base64,/9j/4AAQSkZJRg==', width: 1200, height: 800 };
const PNG = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];
const JPEG = [0xff, 0xd8, 0xff, 0xe0];
const imageBytes = (format, text) => new Uint8Array([...(format === 'jpeg' ? JPEG : PNG), ...new TextEncoder().encode(text)]);
const promptIn = dataURL => { const bytes = Buffer.from(dataURL.slice(dataURL.indexOf(',') + 1), 'base64'); return bytes.subarray(bytes[0] === 0xff ? 4 : 8).toString('utf8'); };
const codes = ds => ds.map(d => d.code + (d.key ? ':' + d.key : ''));
const CLI = fileURLToPath(new URL('../bin/jai.mjs', import.meta.url));

/** A mocked JAI Studio as the global fetch, for the duration of `run`. */
async function withStudio({ images = body => imageReply(body), chat = mockQwen } = {}, run) {
  const calls = [];
  let inflight = 0, maxInflight = 0;
  const original = globalThis.fetch;
  globalThis.fetch = async (url, init = {}) => {
    const body = init.body ? JSON.parse(init.body) : null;
    calls.push({ url, headers: init.headers ?? {}, body });
    const path = new URL(url).pathname;
    let reply;
    if (path === '/v1/images/generations') {
      inflight++; maxInflight = Math.max(maxInflight, inflight);
      await new Promise(r => setTimeout(r, 2));
      inflight--;
      reply = await images(body);
    } else if (path === '/v1/chat/completions') reply = await chat(body);
    else reply = { status: 404, json: {} };
    return new Response(JSON.stringify(reply.json ?? {}), { status: reply.status ?? 200 });
  };
  try { return await run({ calls, maxInflight: () => maxInflight }); } finally { globalThis.fetch = original; }
}
const imageReply = (body, seed = body.seed ?? 99) => ({ json: { created: 1, model: 'flux-schnell', width: body.width, height: body.height, steps: 4, output_format: body.output_format, data: [{ b64_json: Buffer.from(imageBytes(body.output_format, body.prompt)).toString('base64'), seed }], timings: { queueMs: 0, generateMs: 900 } } });
const answer = obj => ({ json: { choices: [{ index: 0, message: { role: 'assistant', content: JSON.stringify(obj) }, finish_reason: 'stop' }] } });

const LAYOUT = /w=([0-9.]+) cx=([0-9.]+) cy=([0-9.]+) hz=([0-9.]+)/;
const TRUE = { w: 0.15, cx: 0.565, cy: 0.515, hz: 0.8 };
const layoutOf = s => { const m = LAYOUT.exec(s); return { w: +m[1], cx: +m[2], cy: +m[3], hz: +m[4] }; };
const draftOf = p => ({ prompt: `Photograph of Earth over the Moon; w=${p.w.toFixed(3)} cx=${p.cx.toFixed(3)} cy=${p.cy.toFixed(3)} hz=${p.hz.toFixed(3)}`, style: ['photo'], avoid: [] });
const SCENE = { horizon_shape: 'flat', camera: 'wide', lighting: 'sun', medium: 'photograph' };
/** The mocked analyst: the target measures as TRUE, a render's layout is read from its bytes, proposals step halfway to TRUE. */
function mockQwen(body) {
  const { name, schema } = body.response_format.json_schema;
  const task = body.messages[1].content.find(p => p.type === 'text');
  const image = body.messages[1].content.find(p => p.type === 'image_url');
  if (name === 'jai_target_inspection') return answer({ subjects: [{ name: 'Earth', bbox_2d: [490, 460, 640, 570] }], horizon_y: 800, ...SCENE });
  if (name === 'jai_render_inspection') {
    const p = layoutOf(promptIn(image.image_url.url));
    const h = (p.w * 11) / 15;
    return answer({ subjects: [{ name: 'Earth', bbox_2d: [p.cx - p.w / 2, p.cy - h / 2, p.cx + p.w / 2, p.cy + h / 2].map(v => Math.round(v * 1000)) }], horizon_y: Math.round(p.hz * 1000), ...SCENE });
  }
  const n = schema.properties.drafts.minItems;
  if (!task.text.includes('Earlier attempts')) return answer({ notes: 'first', drafts: Array.from({ length: n }, (_, i) => draftOf({ w: 0.4 + i * 0.01, cx: 0.3, cy: 0.25, hz: 0.5 })) });
  const best = layoutOf(task.text.slice(task.text.indexOf('#1 score')));
  const next = Object.fromEntries(Object.entries(best).map(([k, v]) => [k, v + (TRUE[k] - v) * 0.5]));
  return answer({ notes: 'closer', drafts: Array.from({ length: n }, (_, i) => draftOf({ ...next, w: next.w + (i - 1) * 0.004 })) });
}

/** A fake globalThis.__jai that records what the bundle hands to the host. */
function fakeHost({ saveFails = false } = {}) {
  const rec = { events: [], saved: [], thumbs: [] };
  globalThis.__jai = {
    thumbnail(bytes) { rec.thumbs.push(bytes); return { width: 16, height: 16, data: new Uint8Array(16 * 16 * 4).fill(120) }; },
    emit(json) { assert.equal(typeof json, 'string'); rec.events.push(JSON.parse(json)); },
    saveRender(name, bytes) { if (saveFails) throw new Error('disk full'); rec.saved.push({ name, bytes }); },
  };
  return rec;
}
const encodeOptions = (o = {}) => ({ server: SERVER, target: TARGET, targetThumbnail: { width: 16, height: 16, data: new Uint8Array(16 * 16 * 4).fill(120) }, generations: 8, population: 3, seeds: [1, 2], restarts: 1, goal: 0.95, sourceName: 'earthrise.jpg', ...o });

// ---------------------------------------------------------------- validate

test('host validate: bytes, JSON and text forms; canonical text; validator advisories', async () => {
  assert.equal(host.version, JSON.parse((await import('node:fs')).readFileSync(new URL('../package.json', import.meta.url), 'utf8')).version);
  const doc = J.fromObject({ prompt: 'A lighthouse at dusk', style: 'oil painting', aspect: '16:9' });
  const gz = host.validate({ bytes: await J.encode(doc) });
  assert.deepEqual(gz, { ok: true, fatal: false, form: 'gzip', text: J.serialize(doc), diagnostics: [] });
  const json = host.validate({ text: '  {"jai":1,"prompt":"A castle","vendor":"x"}' });
  assert.equal(json.form, 'json');
  assert.equal(json.text, 'jai:A castle;vendor=x');
  assert.deepEqual(codes(json.diagnostics), ['unknown-key:vendor']);
  const text = host.validate({ text: 'jai: A castle ; style=ink ; aspect=wide' });
  assert.equal(text.form, 'text');
  assert.equal(text.ok, true);
  assert.ok(codes(text.diagnostics).includes('invalid-value:aspect'));
  assert.throws(() => host.validate({}), TypeError);
});

test('host validate: unreadable documents are fatal, with the reason', () => {
  const bad = host.validate({ text: '{"jai":1,' });
  assert.deepEqual({ ok: bad.ok, fatal: bad.fatal, form: bad.form, text: bad.text }, { ok: false, fatal: true, form: 'json', text: null });
  assert.ok(codes(bad.diagnostics).includes('invalid-json'));
  const gz = host.validate({ bytes: new Uint8Array([0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 255, 1, 2, 3, 4, 5, 6, 7, 8]) });
  assert.equal(gz.fatal, true);
  assert.equal(gz.form, null);
  const mc = gz.diagnostics.find(d => d.code === 'malformed-container');
  assert.equal(typeof mc.reason, 'string');
  assert.equal(host.validate({ bytes: new Uint8Array([0xff, 0xfe, 0x41, 0x00]) }).diagnostics[0].code, 'undecodable');
  assert.equal(host.validate({ bytes: [0x41, 0x20, 0x63, 0x61, 0x74] }).text, 'jai:A cat', 'byte arrays from the host are accepted');
});

test('host validate mirrors `jai validate`: the same text, codes and verdict', () => {
  for (const input of ['jai: A castle ; Style=ink;foo=bar', '{"jai":1,"prompt":"x","prompt ":"y"}', 'prompt=a;prompt=b;seed=12x', '{"jai":1,']) {
    const cli = spawnSync(process.execPath, [CLI, 'validate', input], { encoding: 'utf8' });
    const r = host.validate({ text: input });
    const lines = cli.stdout.trimEnd().split('\n');
    assert.equal(cli.status === 0, r.ok, input);
    if (r.text !== null) assert.equal(lines[0], r.text, input);
    assert.deepEqual(lines.slice(1).map(l => l.trim()), r.diagnostics.map(d => `${d.code}${d.key ? ` (${d.key})` : ''}`), input);
  }
});

// ---------------------------------------------------------------- render

test('host render: one-model interpretation, the exact sent prompt, the reported seed and size', () => withStudio({}, async ({ calls }) => {
  const input = { text: 'jai: A lighthouse at dusk ; style=oil painting ; aspect=16:9 ; seed=42 ; model=stability-ai/sdxl' };
  const r = await host.render(input, { server: SERVER });
  assert.equal(calls.length, 1);
  assert.equal(calls[0].url, `${SERVER.url}/v1/images/generations`);
  assert.equal(calls[0].headers.Authorization, `Bearer ${SERVER.token}`);
  assert.deepEqual(calls[0].body, { prompt: 'A lighthouse at dusk\n\nStyle: oil painting.', width: 1344, height: 768, output_format: 'png', seed: 42 });
  assert.deepEqual({ ...r, bytes: undefined }, {
    bytes: undefined, mime: 'image/png', model: 'black-forest-labs/flux-schnell', modelLabel: 'FLUX.1 schnell',
    sentPrompt: 'A lighthouse at dusk\n\nStyle: oil painting.', seed: 42, width: 1344, height: 768,
    description: 'A lighthouse at dusk', documentText: 'jai:A lighthouse at dusk;aspect=16:9;model=stability-ai/sdxl;seed=42;style=oil painting',
    diagnostics: [{ code: 'no-matching-model', key: 'model' }],
  });
  assert.ok(r.bytes instanceof Uint8Array && r.bytes[0] === 0x89);
}));

test('host render: seed and model options, output format and scale', () => withStudio({}, async ({ calls }) => {
  const text = { text: 'jai: A castle ; seed=5 ; model=flux-dev' };
  const r = await host.render(text, { server: SERVER, seed: ' 7 ', outputFormat: 'jpeg', scale: 0.5 });
  assert.deepEqual(calls[0].body, { prompt: 'A castle', width: 512, height: 512, output_format: 'jpeg', seed: 7 });
  assert.equal(r.seed, 7);
  assert.equal(r.mime, 'image/jpeg');
  assert.deepEqual(codes(r.diagnostics), ['substituted-model:model']);
  const picked = await host.render({ text: 'A castle' }, { server: SERVER, seed: null, model: 'flux-schnell' });
  assert.equal('seed' in calls[1].body, false);
  assert.equal(picked.seed, 99, 'the seed the server picked');
  assert.deepEqual(picked.diagnostics, []);
  const big = await host.render({ text: 'A castle;seed=99999999999999999999' }, { server: SERVER });
  // A document without a model value is reported with no-matching-model too (spec: Model matching).
  assert.deepEqual(big.diagnostics, [{ code: 'no-matching-model', key: 'model' }, { code: 'adjusted-value', key: 'seed', requested: '99999999999999999999', actual: String(Number(99999999999999999999n % 4294967296n)) }]);
  await assert.rejects(host.render(text, { server: SERVER, model: 'stability-ai/sdxl' }), RangeError);
  await assert.rejects(host.render(text, { server: SERVER, seed: '-1' }), RangeError);
  await assert.rejects(host.render(text, {}), TypeError);
}));

test('host render: modes, interpreter reports, file bytes and unreadable documents', () => withStudio({}, async ({ calls }) => {
  const transform = await host.render({ text: 'jai: A castle in snow ; mode=transform ; strength=0.4 ; aspect=4:3 ; aspect=3:2' }, { server: SERVER });
  assert.deepEqual(codes(transform.diagnostics), ['repeated-single:aspect', 'no-matching-model:model', 'original-not-readable:mode']);
  assert.equal(calls[0].body.width, 1216, 'the last aspect value counts');
  const keep = await host.render({ text: 'jai: A castle ; mode=keep' }, { server: SERVER });
  assert.deepEqual(codes(keep.diagnostics), ['no-matching-model:model'], 'a requested render of a keep document is a generate render');
  const file = await host.render({ bytes: await J.encode(J.fromObject({ prompt: 'A red kite', seed: 'abc' })) }, { server: SERVER });
  assert.equal(file.sentPrompt, 'A red kite');
  assert.deepEqual(codes(file.diagnostics), ['invalid-value:seed', 'no-matching-model:model']);
  await assert.rejects(host.render({ bytes: new Uint8Array([0xff, 0xfe, 0x00]) }, { server: SERVER }), /cannot be read: undecodable/);
  assert.equal(calls.length, 3);
}));

// ---------------------------------------------------------------- encode

test('host encode: renders streamed to saveRender, events as JSON with render references, report and .jai out', () => withStudio({}, async ({ calls, maxInflight }) => {
  const rec = fakeHost();
  const out = await host.encode(encodeOptions());
  assert.equal(out.stopReason, 'goal');
  assert.equal(out.error, null);
  assert.ok(out.best.fitness >= 0.95, String(out.best.fitness));
  assert.deepEqual(Object.keys(out.best), ['id', 'fitness']);
  assert.ok(typeof out.seconds === 'number');
  const decoded = J.decode(out.jai);
  assert.equal(decoded.form, 'gzip');
  assert.ok(J.documentsEqual(decoded.document, J.parse(out.text).document));
  assert.deepEqual(decoded.document.get('model'), ['black-forest-labs/flux-schnell']);

  // Every render request: JPEG, a fixed seed, all of a generation at once.
  const images = calls.filter(c => c.url.endsWith('/v1/images/generations'));
  assert.ok(images.length >= 12);
  assert.ok(images.every(c => c.body.output_format === 'jpeg' && [1, 2].includes(c.body.seed)));
  assert.equal(maxInflight(), 4, 'at most four renders at once, so no request waits behind a whole generation');

  // Renders: saved once each, in order, with safe names; each thumbnailed once.
  assert.equal(rec.saved.length, images.length);
  rec.saved.forEach((s, i) => {
    assert.match(s.name, /^[A-Za-z0-9._-]{1,120}$/);
    assert.match(s.name, new RegExp(`^${String(i + 1).padStart(3, '0')}-l1g\\d+c\\d+-s[12]\\.jpg$`));
    assert.deepEqual([...s.bytes.subarray(0, 3)], [0xff, 0xd8, 0xff]);
  });
  assert.equal(rec.thumbs.length, rec.saved.length);
  const names = new Set(rec.saved.map(s => s.name));

  // Events: every engine event, renders replaced by references to the saved files.
  const types = new Set(rec.events.map(e => e.type));
  for (const t of ['phase', 'target', 'candidate', 'generation', 'spend']) assert.ok(types.has(t), t);
  for (const e of rec.events.filter(x => x.type === 'candidate')) {
    for (const r of e.candidate.renders) {
      assert.deepEqual(Object.keys(r), ['model', 'seed', 'file', 'error', 'measure', 'score']);
      assert.ok(names.has(r.file), r.file);
      assert.equal(typeof r.measure.feedback, 'string');
    }
  }
  assert.equal(rec.events.filter(e => e.type === 'candidate').length * 2, rec.saved.length);
  assert.deepEqual(rec.events.at(-1), { type: 'phase', phase: 'done' });

  // The report: the Node CLI's report.json shape, renders by file name.
  const report = out.report;
  assert.deepEqual(Object.keys(report), ['source', 'size', 'models', 'analyst', 'optimiser', 'settings', 'target', 'budgetUSD', 'text', 'ranking', 'stopReason', 'error', 'spentEstimateUSD', 'seconds', 'generations', 'best', 'candidates', 'spend']);
  assert.equal(report.source, 'earthrise.jpg');
  assert.deepEqual(report.size, [1200, 800]);
  assert.deepEqual(report.models, ['black-forest-labs/flux-schnell']);
  assert.equal(report.analyst, 'local/qwen3.8-27b');
  assert.equal(report.budgetUSD, 0);
  assert.equal(report.spentEstimateUSD, 0);
  assert.equal(report.text, out.text);
  assert.deepEqual(report.settings, { population: 3, generations: 8, seeds: [1, 2], restarts: 1, goal: 0.95, thinking: 'off', renderScale: 1, timeLimitMs: null });
  assert.equal(report.best.id, out.best.id);
  for (const c of report.candidates) for (const r of c.renders) {
    assert.deepEqual(Object.keys(r), ['model', 'seed', 'file', 'error', 'observation', 'measure', 'score', 'sentPrompt']);
    assert.ok(names.has(r.file));
  }
  assert.equal(globalThis.__jaiAbort, undefined, 'the abort hook is removed afterwards');
}));

test('host encode: globalThis.__jaiAbort cancels, keeping the best so far', () => withStudio({}, async () => {
  const rec = fakeHost();
  let hook = null;
  const emit = globalThis.__jai.emit;
  globalThis.__jai.emit = json => {
    emit(json);
    if (JSON.parse(json).type === 'generation' && !hook) { hook = globalThis.__jaiAbort; hook(); }
  };
  const out = await host.encode(encodeOptions({ goal: 1, generations: 20 }));
  assert.equal(typeof hook, 'function');
  assert.equal(out.stopReason, 'canceled');
  assert.equal(rec.events.filter(e => e.type === 'generation').length, 1);
  assert.ok(out.text.startsWith('jai:Photograph'));
  assert.equal(globalThis.__jaiAbort, undefined);
}));

test('host encode: a run that stops on an error returns its best with the error, also logged', () => withStudio({
  chat: body => (body.response_format.json_schema.name === 'jai_drafts' && body.messages[1].content[0].text.includes('Earlier attempts')
    ? { status: 500, json: { error: { message: 'llama-server exited' } } } : mockQwen(body)),
}, async () => {
  const rec = fakeHost();
  const out = await host.encode(encodeOptions({ goal: 1 }));
  assert.equal(out.stopReason, 'error');
  assert.equal(out.error, 'Chat completion failed (HTTP 500): llama-server exited');
  assert.equal(out.report.error, out.error);
  assert.ok(out.text.startsWith('jai:Photograph'), 'the best of the first generation');
  assert.deepEqual(rec.events.filter(e => e.type === 'log').map(e => e.message), [`Stopped: ${out.error}`]);
}));

test('host encode: __jaiAbort while Qwen proposes stops as canceled, not as an error', () => withStudio({
  chat: body => {
    if (body.response_format.json_schema.name === 'jai_drafts' && body.messages[1].content[0].text.includes('Earlier attempts')) {
      globalThis.__jaiAbort();
      return new Promise(() => {});
    }
    return mockQwen(body);
  },
}, async () => {
  fakeHost();
  // The stubbed fetch ignores the signal, as a slow server would; the client's own abort ends the call.
  const original = globalThis.fetch;
  globalThis.fetch = (url, init) => new Promise((resolve, reject) => {
    init.signal?.addEventListener('abort', () => reject(Object.assign(new Error('This operation was aborted'), { name: 'AbortError' })));
    original(url, init).then(resolve, reject);
  });
  try {
    const out = await host.encode(encodeOptions({ goal: 1 }));
    assert.equal(out.stopReason, 'canceled');
    assert.equal(out.error, null);
  } finally { globalThis.fetch = original; }
}));

test('host encode: options normalised as the engine does; thinking, scale and time limit reach the providers', () => withStudio({}, async ({ calls, maxInflight }) => {
  fakeHost();
  const out = await host.encode(encodeOptions({ population: 2.7, seeds: [5, -1, 1.5], generations: 1, goal: 1, thinking: 'propose', renderScale: 0.5, timeLimitMs: 60_000, targetThumbnail: { width: 16, height: 16, data: Array.from({ length: 1024 }, () => 120) } }));
  assert.equal(out.stopReason, 'generations');
  const images = calls.filter(c => c.url.endsWith('/v1/images/generations'));
  assert.deepEqual(images.map(c => c.body.seed), [5, 5], 'two candidates, one valid seed');
  assert.ok(images.every(c => c.body.width === 640 && c.body.height === 448), 'renders at half size');
  assert.equal(maxInflight(), 2);
  assert.deepEqual(out.report.settings, { population: 2, generations: 1, seeds: [5], restarts: 1, goal: 1, thinking: 'propose', renderScale: 0.5, timeLimitMs: 60000 });
}));

test('host encode: a render that cannot be saved is logged, not fatal; missing host functions are', () => withStudio({}, async () => {
  const rec = fakeHost({ saveFails: true });
  const out = await host.encode(encodeOptions({ generations: 1, goal: 1 }));
  assert.equal(out.stopReason, 'generations');
  assert.ok(rec.events.some(e => e.type === 'log' && /Could not save render 001-l1g0c1-s1\.jpg: disk full/.test(e.message)));
  assert.ok(rec.events.filter(e => e.type === 'candidate').every(e => e.candidate.renders.every(r => r.file === null)));
  delete globalThis.__jai;
  await assert.rejects(host.encode(encodeOptions()), /globalThis\.__jai/);
  fakeHost();
  await assert.rejects(host.encode(encodeOptions({ targetThumbnail: { width: 16, height: 16, data: new Uint8Array(3) } })), TypeError);
  await assert.rejects(host.encode(encodeOptions({ target: { url: TARGET.url, width: 0, height: 800 } })), TypeError);
}));
