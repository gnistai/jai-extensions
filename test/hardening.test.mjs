// Review round 3 hardening: encode-port guard, analyst text cleaning,
// cancellation by id, and failures that must not orphan provider work.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { sanitizeInput, allowedModel, allowedImage, PortLedger, GEMINI, CLAUDE, CLIP, LUNA, costOf } from '../dist/test/encode-guard.js';
import { HttpReplicateClient } from '../dist/test/replicate-client.js';
import { cleanText, toDrafts, encode } from '../dist/encoder.mjs';

const FLUX = { id: 'black-forest-labs/flux-schnell' };

test('guard: only allow-listed models, versions pinned', () => {
  assert.ok(allowedModel(FLUX));
  assert.equal(allowedModel({ ...FLUX, version: 'x' }), null);
  assert.equal(allowedModel({ id: 'stability-ai/sdxl' }), null);
  assert.ok(allowedModel(CLIP));
  assert.equal(allowedModel({ id: 'evil/model' }), null);
});

test('guard: inputs are rebuilt from an allow-list; fixed fields come from the renderer table', () => {
  const r = sanitizeInput(FLUX, { prompt: 'a cat', aspect_ratio: '16:9', seed: 3, disable_safety_checker: true, num_outputs: 4, megapixels: '4', image: 'https://x' });
  assert.deepEqual(Object.keys(r.input).sort(), ['aspect_ratio', 'num_outputs', 'output_format', 'output_quality', 'prompt', 'seed']);
  assert.equal(r.input.num_outputs, 1);
  assert.ok('error' in sanitizeInput(FLUX, { prompt: 'a', aspect_ratio: '7:3' }));
  assert.ok('error' in sanitizeInput(FLUX, { aspect_ratio: '1:1' }), 'prompt is required');
  assert.ok('error' in sanitizeInput(FLUX, { prompt: 'a', seed: -1 }));
  const g = sanitizeInput(GEMINI, { prompt: 'p', images: ['data:image/jpeg;base64,AAAA'], max_output_tokens: 65535, thinking_budget: 1024, tools: ['x'] });
  assert.equal(g.input.max_output_tokens, 8192, 'token limits are clamped');
  assert.equal('tools' in g.input, false, 'unknown fields are dropped');
  const g2 = sanitizeInput(GEMINI, { prompt: 'p', images: ['data:image/jpeg;base64,AAAA', 'https://replicate.delivery/a/b.webp'], thinking_budget: 1024 });
  assert.deepEqual(Object.keys(g2.input).sort(), ['images', 'max_output_tokens', 'prompt', 'thinking_budget']);
  assert.ok('error' in sanitizeInput(GEMINI, { prompt: 'p', images: ['http://192.168.0.1/secret.png'] }));
  assert.ok('error' in sanitizeInput(CLAUDE, { prompt: 'p', image: 'https://evil.example/a.png' }));
  assert.ok('error' in sanitizeInput(CLIP, { text: 'x' }));
  const l = sanitizeInput(LUNA, { prompt: 'p', system_prompt: 's', image_input: ['data:image/jpeg;base64,AAAA'], reasoning_effort: 'low', max_completion_tokens: 99999, tools: [1] });
  assert.deepEqual(Object.keys(l.input).sort(), ['image_input', 'max_completion_tokens', 'prompt', 'reasoning_effort', 'system_prompt']);
  assert.equal(l.input.max_completion_tokens, 4096);
  assert.ok('error' in sanitizeInput(LUNA, { prompt: 'p', reasoning_effort: 'xhigh' }));
  assert.ok('error' in sanitizeInput(LUNA, { prompt: 'p', image_input: ['https://evil.example/a.png'] }));
});

test('guard: image URLs are re-encoded rasters or Replicate outputs only', () => {
  assert.ok(allowedImage('data:image/png;base64,iVBORw0KGgo='));
  assert.ok(allowedImage('https://abc.r2.cloudflarestorage.com/o.webp?sig=1'));
  for (const bad of ['data:image/svg+xml;base64,PHN2Zz4=', 'https://replicate.delivery.evil.com/a', 'https://u:p@replicate.delivery/a', 'http://replicate.delivery/a', 'https://replicate.delivery:8443/a', 'file:///etc/passwd']) assert.equal(allowedImage(bad), false, bad);
});

test('guard: a fixed per-port ceiling and a concurrency cap; token models settle from usage', () => {
  const l = new PortLedger();
  assert.match(l.admit(FLUX), /did not declare/);
  assert.ok(l.begin(0.01));
  assert.equal(l.begin(5), false, 'the ceiling is fixed once');
  assert.equal(l.admit(FLUX), null);
  assert.equal(l.admit(FLUX), null);
  assert.equal(l.admit(FLUX), null);
  assert.match(l.admit(FLUX), /limit/);
  const big = new PortLedger(); big.begin(100);
  for (let i = 0; i < 8; i++) assert.equal(big.admit(FLUX), null);
  assert.match(big.admit(FLUX), /Too many/);
  const t = new PortLedger(); t.begin(1);
  t.admit(GEMINI);
  assert.ok(Math.abs(t.spent - costOf(GEMINI)) < 1e-12);
  t.done(GEMINI, { input_token_count: 1000, output_token_count: 1000 });
  assert.ok(Math.abs(t.spent - (1000 * 0.30 + 1000 * 2.50) / 1e6) < 1e-12);
});

test('analyst text: control, bidi and zero-width characters removed, whitespace collapsed, items capped', () => {
  assert.equal(cleanText('a‮b​c\u0007d\ne  f', 100), 'abcd e f');
  assert.equal(cleanText('👩‍💻 ok', 100), '👩‍💻 ok');
  const [d] = toDrafts([{ prompt: 'x', style: ['film\nphoto', 'y'.repeat(5000)], avoid: ['a\tb'] }]);
  assert.deepEqual(d.style.map(s => s.length), [10, 200]);
  assert.equal(d.style[0], 'film photo');
  assert.deepEqual(d.avoid, ['a b']);
});

test('client: an abort during the create request cancels the prediction by id', async () => {
  let cancels = 0;
  const fetch = async (url, init) => {
    if (init?.method === 'POST' && url.endsWith('/cancel')) { cancels++; return new Response('{}', { status: 200 }); }
    if (init?.method === 'POST') { await new Promise(r => setTimeout(r, 50)); return new Response(JSON.stringify({ id: 'p1', status: 'processing' }), { status: 201 }); }
    return new Response(JSON.stringify({ id: 'p1', status: 'processing' }), { status: 200 });
  };
  const ac = new AbortController();
  setTimeout(() => ac.abort(), 10);
  await assert.rejects(new HttpReplicateClient('r8_t', { fetch, pollMs: 1 }).predict(FLUX, { prompt: 'x' }, { signal: ac.signal }), e => e.kind === 'canceled' && e.jobId === 'p1');
  await new Promise(r => setTimeout(r, 10));
  assert.equal(cancels, 1);
  // Already aborted: nothing is created at all.
  let posts = 0;
  const f2 = async () => { posts++; return new Response('{}'); };
  const done = new AbortController(); done.abort();
  await assert.rejects(new HttpReplicateClient('r8_t', { fetch: f2 }).predict(FLUX, { prompt: 'x' }, { signal: done.signal }));
  assert.equal(posts, 0);
});

test('engine: an inspection failure is one failed render, not a crash; nothing runs after encode() returns', async () => {
  const models = [{ id: 'acme/a', label: 'A', family: 'a', aliases: [], imageInput: false, negativePrompt: false, shortWindow: false, costHint: 0.01, description: '' }];
  let started = 0, returned = false, after = 0;
  const renderer = { id: 'm', label: 'm', models: () => models, async render(req) { started++; if (returned) after++; await new Promise(r => setTimeout(r, 5)); return { bytes: new Uint8Array([1]), mime: 'image/png', model: req.modelId, sentPrompt: '', sentNegative: null, sentInput: {}, jobId: null, diagnostics: [], outputURL: 'mock:' + started }; } };
  let n = 0;
  const obs = { subjects: [{ name: 'thing', box: { ymin: 0.4, xmin: 0.4, ymax: 0.6, xmax: 0.6 } }], horizon: null, horizonShape: '', camera: '', lighting: '', medium: '' };
  const analyst = { id: 'a', costHint: 0.005,
    async describe(t, k) { return { value: Array.from({ length: k }, (_, i) => ({ prompt: 'p' + i, style: [], avoid: [] })), notes: '', cost: 0.005 }; },
    async inspect(img, subjects) { if (subjects && ++n === 2) throw new Error('bad JSON'); return { value: obs, notes: '', cost: 0.001 }; },
    async propose(t, ctx) { return { value: Array.from({ length: ctx.n }, (_, i) => ({ prompt: 'q' + i, style: [], avoid: [] })), notes: '', cost: 0.005 }; } };
  const r = await encode({ target: { url: 'data:image/png;base64,AA', width: 10, height: 10 }, models: ['acme/a'], renderer, analyst, budgetUSD: 1, population: 4, maxGenerations: 2, concurrency: 2, goal: 2 });
  returned = true;
  await new Promise(r => setTimeout(r, 50));
  assert.equal(after, 0);
  const failed = r.candidates.flatMap(c => c.renders).filter(x => x.error?.startsWith('Inspection failed'));
  assert.equal(failed.length, 1);
  assert.equal(failed[0].score, 0);
  assert.ok(r.spend.some(l => l.label.includes('(failed)') && l.kind === 'inspect'));
});

test('fetch guard: only .jai over http(s), no private targets from public pages, rate limited', async () => {
  const { isPrivateHost, refuseJaiFetch, RateLimit } = await import('../dist/test/fetch-guard.js');
  for (const h of ['localhost', 'a.localhost', '127.0.0.1', '10.1.2.3', '172.20.0.1', '192.168.1.1', '169.254.1.1', '100.64.0.1', '0.0.0.0', '[::1]', 'fd12:3456::1', 'fe80::1', '::ffff:192.168.0.1', 'router', 'nas.local', 'printer.lan']) assert.equal(isPrivateHost(h), true, h);
  for (const h of ['example.org', '8.8.8.8', '172.32.0.1', '2001:db8::1', 'cdn.example.com']) assert.equal(isPrivateHost(h), false, h);
  assert.equal(refuseJaiFetch('https://cdn.example/a.jai', 'https://site.example'), null);
  assert.match(refuseJaiFetch('https://cdn.example/a.png', 'https://site.example'), /\.jai/);
  assert.match(refuseJaiFetch('ftp://cdn.example/a.jai', 'https://site.example'), /http/);
  assert.match(refuseJaiFetch('http://192.168.1.1/x.jai', 'https://site.example'), /private/);
  assert.match(refuseJaiFetch('http://router/x.jai', 'https://site.example'), /private/);
  assert.equal(refuseJaiFetch('http://192.168.1.1/x.jai', 'http://intranet.local'), null, 'a local page may use local resources');
  const rl = new RateLimit(2, 1000);
  assert.ok(rl.allow('t', 0)); assert.ok(rl.allow('t', 1)); assert.equal(rl.allow('t', 2), false); assert.ok(rl.allow('t', 1500));
});
