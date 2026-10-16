// Review round 3 hardening: encode-port guard, analyst text cleaning,
// cancellation by id, and failures that must not orphan provider work.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { sanitizeInput, allowedModel, allowedImage, PortLedger, GEMINI, CLAUDE, CLIP, costOf } from '../dist/test/encode-guard.js';
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

test('engine: a scorer failure is one failed render, not a crash; nothing runs after encode() returns', async () => {
  const models = [{ id: 'acme/a', label: 'A', family: 'a', aliases: [], imageInput: false, negativePrompt: false, shortWindow: false, costHint: 0.01, description: '' }];
  let started = 0, returned = false, after = 0;
  const renderer = { id: 'm', label: 'm', models: () => models, async render(req) { started++; if (returned) after++; await new Promise(r => setTimeout(r, 5)); return { bytes: new Uint8Array([1]), mime: 'image/png', model: req.modelId, sentPrompt: '', sentNegative: null, sentInput: {}, jobId: null, diagnostics: [], outputURL: 'mock:' + started }; } };
  let n = 0;
  const scorer = { id: 's', costHint: 0.001, async prepare() {}, async score() { if (++n === 2) throw new Error('no embedding'); return 0.5; } };
  const analyst = { id: 'a', costHint: 0.005, async describe(t, k) { return { value: Array.from({ length: k }, (_, i) => ({ prompt: 'p' + i, style: [], avoid: [] })), notes: '', cost: 0.005 }; }, async revise(t, d, r, k) { return { value: Array.from({ length: k }, (_, i) => ({ prompt: 'q' + i, style: [], avoid: [] })), notes: '', cost: 0.005 }; }, async refine() { return { value: [], notes: '', cost: 0.005 }; } };
  const r = await encode({ target: { url: 'data:image/png;base64,AA', width: 10, height: 10 }, models: ['acme/a'], renderer, analyst, scorer, budgetUSD: 1, population: 4, maxGenerations: 2, concurrency: 2 });
  returned = true;
  await new Promise(r => setTimeout(r, 50));
  assert.equal(after, 0);
  const failed = r.candidates.flatMap(c => c.renders).filter(x => x.error?.startsWith('Scoring failed'));
  assert.equal(failed.length, 1);
  assert.equal(failed[0].score, 0);
  assert.ok(r.spend.some(l => l.label.includes('(failed)') && l.kind === 'scorer'));
});
