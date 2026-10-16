import { test } from 'node:test';
import assert from 'node:assert/strict';
import { buildInput, REPLICATE_MODELS, ReplicateRenderer, sniffImage } from '../dist/test/replicate.js';

const model = id => REPLICATE_MODELS.find(m => m.id === id);
const intent = (o = {}) => ({ description: 'A castle', style: [], avoid: [], aspect: null, mode: 'generate', strength: null, seed: null, animate: false, ...o });
const req = (o = {}) => ({ intent: intent(o.intent), modelId: o.modelId ?? 'black-forest-labs/flux-schnell', inputImage: o.inputImage ?? null, governingRatio: o.governingRatio ?? null, target: o.target ?? null });
const codes = r => r.diagnostics.map(d => d.code + (d.key ? ':' + d.key : ''));

test('flux: style appended, aspect snapped to the enum, avoid folded and reported', () => {
  const r = buildInput(model('black-forest-labs/flux-schnell'), req({ intent: { style: ['watercolor', 'loose'], avoid: ['text'], aspect: { width: 16, height: 10 } } }), { foldAvoid: true });
  assert.equal(r.input.prompt, 'A castle\n\nStyle: watercolor, loose.\n\nThe image does not contain: text.');
  assert.equal(r.input.aspect_ratio, '3:2'); // 16:10 is nearer 3:2 than 16:9 by log ratio
  assert.equal(r.input.output_format, 'webp');
  assert.ok(codes(r).includes('folded-into-prompt:avoid'));
  assert.ok(codes(r).includes('adjusted-value:aspect'));
  assert.equal(r.input.disable_safety_checker, undefined);
});

test('flux: avoid omitted and reported when folding is off', () => {
  const r = buildInput(model('black-forest-labs/flux-schnell'), req({ intent: { avoid: ['people'] } }), { foldAvoid: false });
  assert.equal(r.input.prompt, 'A castle');
  assert.deepEqual(codes(r), ['unsupported-parameter:avoid']);
});

test('sdxl: style first (short window), native negative prompt, width/height', () => {
  const r = buildInput(model('stability-ai/sdxl'), req({ modelId: 'stability-ai/sdxl', intent: { style: ['oil painting'], avoid: ['text', 'frame'], aspect: { width: 4, height: 3 } } }), { foldAvoid: true });
  assert.equal(r.input.prompt, 'oil painting. A castle');
  assert.equal(r.input.negative_prompt, 'text, frame');
  assert.equal(r.input.width % 8, 0); assert.equal(r.input.height % 8, 0);
  assert.ok(Math.abs(r.input.width / r.input.height - 4 / 3) < 0.02);
  assert.equal(r.input.apply_watermark, false);
});

test('the governing ratio (original or sized box) overrides aspect; the library, not the engine, reports it', () => {
  const r = buildInput(model('black-forest-labs/flux-schnell'), req({ governingRatio: 1.5, intent: { aspect: { width: 1, height: 1 } } }), { foldAvoid: true });
  assert.equal(r.input.aspect_ratio, '3:2');
  assert.ok(!codes(r).includes('ignored-value:aspect'));
});

test('without a governing ratio, aspect decides; the displayed size of an unsized box does not', () => {
  const r = buildInput(model('black-forest-labs/flux-schnell'), req({ target: { width: 800, height: 800 }, intent: { aspect: { width: 16, height: 9 } } }), { foldAvoid: true });
  assert.equal(r.input.aspect_ratio, '16:9');
  const none = buildInput(model('black-forest-labs/flux-schnell'), req({ target: { width: 800, height: 400 } }), { foldAvoid: true });
  assert.equal(none.input.aspect_ratio, undefined);
});

test('flux-dev transform: input image and departure strength', () => {
  const img = { bytes: new Uint8Array([1, 2, 3]), mime: 'image/webp' };
  const r = buildInput(model('black-forest-labs/flux-dev'), req({ modelId: 'black-forest-labs/flux-dev', inputImage: img, intent: { mode: 'transform', strength: 0.7 } }), { foldAvoid: true });
  assert.equal(r.input.image, 'data:image/webp;base64,AQID');
  assert.equal(r.input.prompt_strength, 0.7);
  assert.match(String(r.shown.image), /input image, 3 bytes/);
});

test('kontext transform: match_input_image; no strength control is reported', () => {
  const img = { bytes: new Uint8Array([1]), mime: 'image/png' };
  const r = buildInput(model('black-forest-labs/flux-kontext-pro'), req({ modelId: 'black-forest-labs/flux-kontext-pro', inputImage: img, intent: { mode: 'transform', strength: 0.5 } }), { foldAvoid: true });
  assert.equal(r.input.aspect_ratio, 'match_input_image');
  assert.ok(r.input.input_image.startsWith('data:image/png;base64,'));
  assert.ok(codes(r).includes('unsupported-parameter:strength'));
});

test('transform without readable pixels is reported; a still-only model degrades', () => {
  assert.ok(codes(buildInput(model('black-forest-labs/flux-dev'), req({ intent: { mode: 'transform' } }), { foldAvoid: true })).includes('original-not-readable:mode'));
  const img = { bytes: new Uint8Array([1]), mime: 'image/png' };
  assert.ok(codes(buildInput(model('black-forest-labs/flux-schnell'), req({ inputImage: img, intent: { mode: 'transform' } }), { foldAvoid: true })).includes('mode-degraded:mode'));
});

test('seed: passed through, or reduced deterministically above the model range', () => {
  assert.equal(buildInput(model('black-forest-labs/flux-schnell'), req({ intent: { seed: '42' } }), { foldAvoid: true }).input.seed, 42);
  const r = buildInput(model('black-forest-labs/flux-schnell'), req({ intent: { seed: '12345678901234567890' } }), { foldAvoid: true });
  assert.equal(r.input.seed, Number(12345678901234567890n % 4294967296n));
  assert.ok(codes(r).includes('adjusted-value:seed'));
});

test('animate on a still model is reported', () => {
  assert.ok(codes(buildInput(model('black-forest-labs/flux-schnell'), req({ intent: { animate: true } }), { foldAvoid: true })).includes('unsupported-parameter:animate'));
});

test('an empty description is still sent explicitly (never the provider default)', () => {
  const r = buildInput(model('stability-ai/sdxl'), req({ modelId: 'stability-ai/sdxl', intent: { description: '', style: ['anime'] } }), { foldAvoid: true });
  assert.equal(r.input.prompt, 'anime.');
  const r2 = buildInput(model('stability-ai/sdxl'), req({ modelId: 'stability-ai/sdxl', intent: { description: '' } }), { foldAvoid: true });
  assert.equal(r2.input.prompt, '');
});

const WEBP = new Uint8Array([0x52, 0x49, 0x46, 0x46, 0, 0, 0, 0, 0x57, 0x45, 0x42, 0x50, 1, 2, 3]);
function mockFetch(script) {
  const calls = [];
  const f = async (url, init = {}) => {
    calls.push({ url, method: init.method || 'GET', body: init.body ? JSON.parse(init.body) : null, headers: init.headers || {} });
    const step = script(url, init, calls.length);
    return new Response(step.bytes ?? JSON.stringify(step.json ?? {}), { status: step.status ?? 200 });
  };
  f.calls = calls;
  return f;
}

test('render: create, poll, download; token only goes to api.replicate.com', async () => {
  const f = mockFetch((url, init, n) => {
    if (url.endsWith('/models/black-forest-labs/flux-schnell/predictions')) return { json: { id: 'p1', status: 'starting' } };
    if (url.endsWith('/predictions/p1')) return { json: n < 4 ? { id: 'p1', status: 'processing' } : { id: 'p1', status: 'succeeded', output: ['https://replicate.delivery/x/out.webp'] } };
    if (url === 'https://replicate.delivery/x/out.webp') return { bytes: WEBP };
    return { status: 404 };
  });
  const r = await new ReplicateRenderer('r8_test', { foldAvoid: true, fetch: f, pollMs: 1 }).render({ ...req(), onStatus() {} });
  assert.equal(r.mime, 'image/webp');
  assert.equal(r.jobId, 'p1');
  assert.equal(r.sentPrompt, 'A castle');
  for (const c of f.calls) {
    const auth = c.headers.Authorization;
    if (c.url.startsWith('https://api.replicate.com/')) assert.equal(auth, 'Bearer r8_test');
    else assert.equal(auth, undefined, 'token must not be sent to ' + c.url);
  }
});

test('render: community model uses /predictions with a pinned version', async () => {
  const f = mockFetch(url => url.endsWith('/v1/predictions') ? { json: { id: 'p2', status: 'succeeded', output: 'https://replicate.delivery/y.png' } } : { bytes: new Uint8Array([0x89, 0x50, 0x4e, 0x47, 0]) });
  const r = await new ReplicateRenderer('r8_t', { foldAvoid: true, fetch: f, pollMs: 1 }).render(req({ modelId: 'stability-ai/sdxl' }));
  assert.equal(f.calls[0].body.version, REPLICATE_MODELS.find(m => m.id === 'stability-ai/sdxl').version);
  assert.equal(r.mime, 'image/png');
});

test('render: 401 is an auth error; a safety failure is a refusal; a non-image is rejected', async () => {
  await assert.rejects(new ReplicateRenderer('r8_bad', { foldAvoid: true, fetch: mockFetch(() => ({ status: 401, json: { detail: 'nope' } })), pollMs: 1 }).render(req()), e => e.kind === 'auth');
  await assert.rejects(new ReplicateRenderer('r8_t', { foldAvoid: true, fetch: mockFetch(() => ({ json: { id: 'p', status: 'failed', error: 'NSFW content detected' } })), pollMs: 1 }).render(req()), e => e.kind === 'refused');
  const html = mockFetch(url => url.includes('api.') ? { json: { id: 'p', status: 'succeeded', output: ['https://replicate.delivery/a'] } } : { bytes: new TextEncoder().encode('<html>') });
  await assert.rejects(new ReplicateRenderer('r8_t', { foldAvoid: true, fetch: html, pollMs: 1 }).render(req()), e => e.kind === 'provider');
});

test('render: aborting cancels the provider job', async () => {
  const ac = new AbortController();
  const f = mockFetch((url, init, n) => {
    if (url.endsWith('/predictions') || url.endsWith('flux-schnell/predictions')) return { json: { id: 'p9', status: 'starting' } };
    if (url.endsWith('/cancel')) return { json: { id: 'p9', status: 'canceled' } };
    if (n === 2) ac.abort();
    return { json: { id: 'p9', status: 'processing' } };
  });
  await assert.rejects(new ReplicateRenderer('r8_t', { foldAvoid: true, fetch: f, pollMs: 1 }).render({ ...req(), signal: ac.signal }), e => e.kind === 'canceled');
  await new Promise(r => setTimeout(r, 5));
  assert.ok(f.calls.some(c => c.url.endsWith('/predictions/p9/cancel') && c.method === 'POST'));
});

test('sniffImage recognises raster signatures only', () => {
  assert.equal(sniffImage(WEBP), 'image/webp');
  assert.equal(sniffImage(new Uint8Array([0xff, 0xd8, 0xff, 0xe0])), 'image/jpeg');
  assert.equal(sniffImage(new TextEncoder().encode('<svg')), null);
});
