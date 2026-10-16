// Encoder engine with mocked providers: no network, no cost.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { encode, fitnessOf, Budget, BudgetExceeded, reduceAspect, thumbnailSimilarity, parseAnswer, toDrafts, refinementTargets, buildDocument, cosine } from '../dist/encoder.mjs';
import { parse, interpret } from '../dist/jai.mjs';

const TRUTH = ['harbour', 'dusk', 'boats', 'lighthouse', 'mist', 'gulls'];
const MODELS = [
  { id: 'acme/alpha', label: 'Alpha', family: 'alpha', aliases: ['alpha-1'], imageInput: false, negativePrompt: false, shortWindow: false, costHint: 0.01, description: '' },
  { id: 'acme/beta', label: 'Beta', family: 'beta', aliases: [], imageInput: false, negativePrompt: false, shortWindow: false, costHint: 0.02, description: '' },
];

function mockRenderer(log = []) {
  return {
    id: 'mock', label: 'Mock', models: () => MODELS,
    async render(req) {
      log.push({ model: req.modelId, description: req.intent.description, seed: req.intent.seed, aspect: req.intent.aspect });
      const text = req.intent.description + ' ' + req.intent.style.join(' ');
      return { bytes: new TextEncoder().encode(text), mime: 'image/png', model: req.modelId, sentPrompt: text, sentNegative: null, sentInput: {}, jobId: 'j', diagnostics: [], outputURL: `mock:${req.modelId}:${encodeURIComponent(text)}` };
    },
  };
}

// Similarity = share of the truth words present; beta also loses 0.2 unless told "beta-fix".
function mockScorer() {
  return {
    id: 'mock-scorer', costHint: 0.001,
    async prepare() {},
    async score({ url }) {
      const [, model, enc] = url.match(/^mock:([^:]+):(.*)$/);
      const text = decodeURIComponent(enc);
      let s = TRUTH.filter(w => text.includes(w)).length / TRUTH.length;
      if (model === 'acme/beta' && !text.includes('beta-fix')) s -= 0.2;
      return s;
    },
  };
}

// Each revision adds the next missing truth word to one variant.
function mockAnalyst(calls = []) {
  const draft = words => ({ prompt: `Photograph of a ${words.join(' and ')}`, style: ['photo'], avoid: [] });
  const wordsOf = d => TRUTH.filter(w => d.prompt.includes(w));
  return {
    id: 'mock-analyst', costHint: 0.005,
    async describe(target, n, ctx) { calls.push(['describe', n, ctx.models]); return { value: Array.from({ length: n }, (_, i) => draft(TRUTH.slice(0, 1 + (i % 2)))), notes: 'first', cost: 0.005 }; },
    async revise(target, current, renders, n) {
      calls.push(['revise', n, renders.map(r => r.model)]);
      const have = wordsOf(current);
      const missing = TRUTH.find(w => !have.includes(w));
      const better = missing ? draft([...have, missing]) : current;
      return { value: [better, ...Array.from({ length: n - 1 }, () => draft(have.slice(0, -1)))].slice(0, n), notes: `missing ${missing}`, cost: 0.005 };
    },
    async refine(target, core, render, n) { calls.push(['refine', render.model]); return { value: ['A small beta-fix detail.', 'Something irrelevant.'].slice(0, n), notes: 'beta lags', cost: 0.005 }; },
  };
}

const target = { url: 'data:image/png;base64,AAAA', width: 1920, height: 1080 };

test('encoder: fitness improves over generations and the best is never lost', async () => {
  const events = [];
  const r = await encode({ target, models: ['acme/alpha'], renderer: mockRenderer(), analyst: mockAnalyst(), scorer: mockScorer(), budgetUSD: 1, population: 3, maxGenerations: 5, onEvent: e => events.push(e) });
  const fits = r.generations.map(g => g.bestFitness);
  for (let i = 1; i < fits.length; i++) assert.ok(fits[i] >= fits[i - 1], `monotonic ${fits}`);
  assert.ok(fits[fits.length - 1] > fits[0], `improved ${fits}`);
  assert.equal(r.generations.length, 5);
  assert.equal(r.stopReason, 'generations');
  assert.ok(events.some(e => e.type === 'phase' && e.phase === 'done'));
  // Output conventions: aspect from pixels, model listed, no seed.
  const doc = parse(r.text).document;
  assert.deepEqual(doc.get('aspect'), ['16:9']);
  assert.deepEqual(doc.get('model'), ['acme/alpha']);
  assert.equal(doc.has('seed'), false);
});

test('encoder: stops on plateau', async () => {
  const flat = { ...mockAnalyst(), async revise(t, current, renders, n) { return { value: Array.from({ length: n }, () => current), notes: '', cost: 0.005 }; } };
  const r = await encode({ target, models: ['acme/alpha'], renderer: mockRenderer(), analyst: flat, scorer: mockScorer(), budgetUSD: 1, population: 2, maxGenerations: 10, plateau: 2 });
  assert.equal(r.stopReason, 'plateau');
  assert.equal(r.generations.length, 3);
});

test('encoder: never exceeds the budget and stops with reason budget', async () => {
  const log = [];
  const r = await encode({ target, models: ['acme/alpha', 'acme/beta'], renderer: mockRenderer(log), analyst: mockAnalyst(), scorer: mockScorer(), budgetUSD: 0.2, population: 4, maxGenerations: 20 });
  assert.equal(r.stopReason, 'budget');
  assert.ok(r.spent <= 0.2 + 1e-9, `spent ${r.spent}`);
  const charged = r.spend.filter(l => l.kind === 'render').length;
  assert.equal(charged, log.length, 'every render is charged');
});

test('encoder: refuses to start when the budget cannot cover one round', async () => {
  await assert.rejects(encode({ target, models: ['acme/alpha'], renderer: mockRenderer(), analyst: mockAnalyst(), scorer: mockScorer(), budgetUSD: 0.001 }), e => e.stopReason === 'budget' && e.spent === 0);
});

test('encoder: refines the lagging model with prompt/<family> and ranks models by score', async () => {
  const calls = [];
  const log = [];
  const r = await encode({ target, models: ['acme/beta', 'acme/alpha'], renderer: mockRenderer(log), analyst: mockAnalyst(calls), scorer: mockScorer(), budgetUSD: 2, population: 3, maxGenerations: 3, seeds: [7] });
  assert.ok(calls.some(c => c[0] === 'refine' && c[1] === 'acme/beta'));
  assert.ok(!calls.some(c => c[0] === 'refine' && c[1] === 'acme/alpha'), 'the leader is not refined');
  const doc = parse(r.text).document;
  assert.deepEqual(doc.get('prompt/beta'), ['A small beta-fix detail.']);
  // Best first; ties keep the author's order.
  assert.deepEqual(doc.get('model'), ['acme/beta', 'acme/alpha'].sort((a, b) => r.best.perModel[b] - r.best.perModel[a]));
  // Refinement renders went to beta only, with the fixed seed; alpha never saw the refinement.
  assert.ok(log.every(l => l.seed === '7'));
  assert.ok(!log.some(l => l.model === 'acme/alpha' && l.description.includes('beta-fix')));
  assert.ok(log.some(l => l.model === 'acme/beta' && l.description.includes('beta-fix')));
  // The document interprets for beta with the refinement appended to the core.
  const it = interpret(doc, { catalog: MODELS.map(m => ({ ...m, default: false })), selected: 'acme/beta' }).interpreted;
  assert.ok(it.modelDescription.endsWith('\nA small beta-fix detail.'));
  assert.ok(!it.description.includes('beta-fix'), 'the description stays the core');
});

test('encoder: a failed render scores 0 and is still charged', async () => {
  const base = mockRenderer();
  const flaky = { ...base, async render(req) { if (req.modelId === 'acme/beta') throw Object.assign(new Error('NSFW'), { kind: 'refused' }); return base.render(req); } };
  const r = await encode({ target, models: ['acme/alpha', 'acme/beta'], renderer: flaky, analyst: mockAnalyst(), scorer: mockScorer(), budgetUSD: 1, population: 2, maxGenerations: 2, refine: false });
  assert.ok(r.candidates.every(c => c.perModel['acme/beta'] === 0));
  assert.ok(r.spend.some(l => l.label.includes('(failed)')));
});

test('encoder: cancel stops with the best so far', async () => {
  const ac = new AbortController();
  const r = await encode({ target, models: ['acme/alpha'], renderer: mockRenderer(), analyst: mockAnalyst(), scorer: mockScorer(), budgetUSD: 1, population: 2, maxGenerations: 10, signal: ac.signal, onEvent: e => { if (e.type === 'generation' && e.generation.index === 1) ac.abort(); } });
  assert.equal(r.stopReason, 'canceled');
  assert.ok(r.text.startsWith('jai:Photograph'));
});

test('encoder: the judge rates each generation with the best as anchor and blends into the score', async () => {
  const judged = [];
  const analyst = { ...mockAnalyst(), judgeLimit: 3,
    async judge(target, renders) {
      judged.push(renders.map(r => decodeURIComponent(r.image.url).length));
      // Rates longer descriptions higher (stand-in for "closer").
      return { value: renders.map(r => Math.min(1, decodeURIComponent(r.image.url).length / 200)), notes: 'longer is closer', cost: 0.005 };
    } };
  const r = await encode({ target, models: ['acme/alpha'], renderer: mockRenderer(), analyst, scorer: mockScorer(), budgetUSD: 1, population: 4, maxGenerations: 3, judgeWeight: 0.5 });
  // Generation 0: 4 candidates, limit 3 -> anchor + 2, anchor + 1: two calls; later generations: best + 3 new -> two calls each.
  assert.equal(judged.length, 6, JSON.stringify(judged));
  for (let i = 0; i < judged.length; i += 2) assert.equal(judged[i][0], judged[i + 1][0], 'the anchor is in every call of a generation');
  for (const c of r.candidates) for (const x of c.renders) {
    assert.ok(x.judge !== null);
    assert.ok(Math.abs(x.score - (0.5 * x.metric + 0.5 * x.judge)) < 1e-12);
  }
  assert.ok(r.spend.filter(l => l.label.startsWith('judge')).length === 6);
  assert.ok(r.spent <= 1);
});

test('encoder: a failing judge falls back to the metric for every render in the call', async () => {
  const analyst = { ...mockAnalyst(), async judge() { throw new Error('bad JSON'); } };
  const logs = [];
  const r = await encode({ target, models: ['acme/alpha'], renderer: mockRenderer(), analyst, scorer: mockScorer(), budgetUSD: 1, population: 2, maxGenerations: 2, onEvent: e => e.type === 'log' && logs.push(e.message) });
  assert.ok(logs.some(m => m.includes('Judging Alpha failed')));
  for (const c of r.candidates) for (const x of c.renders) assert.equal(x.score, x.metric);
});

test('encoder: skips refining a short-window model when the core fills its window', async () => {
  const calls = [];
  const longModels = MODELS.map(m => (m.id === 'acme/beta' ? { ...m, shortWindow: true } : m));
  const renderer = { ...mockRenderer(), models: () => longModels };
  const long = { ...mockAnalyst(calls), async describe(t, n) { calls.push(['describe']); return { value: Array.from({ length: n }, () => ({ prompt: 'Photograph of a harbour ' + 'word '.repeat(60), style: [], avoid: [] })), notes: '', cost: 0.005 }; } };
  const logs = [];
  await encode({ target, models: ['acme/alpha', 'acme/beta'], renderer, analyst: long, scorer: mockScorer(), budgetUSD: 1, population: 2, maxGenerations: 1, onEvent: e => e.type === 'log' && logs.push(e.message) });
  assert.ok(!calls.some(c => c[0] === 'refine'));
  assert.ok(logs.some(m => m.startsWith('Not refining Beta')));
});

test('encoder helpers', () => {
  assert.equal(fitnessOf({ a: 0.8 }), 0.8);
  assert.ok(Math.abs(fitnessOf({ a: 0.8, b: 0.4 }) - 0.5) < 1e-12);
  assert.equal(reduceAspect(1920, 1080), '16:9');
  assert.equal(reduceAspect(1024, 1024), '1:1');
  assert.equal(reduceAspect(3000, 2000), '3:2');
  assert.equal(reduceAspect(1000, 1001), '1:1');
  assert.equal(reduceAspect(997, 211), '19:4');
  assert.equal(reduceAspect(6400, 90), '640:9');
  assert.equal(cosine([1, 0], [1, 0]), 1);
  const px = (r, g, b) => ({ width: 1, height: 1, data: new Uint8Array([r, g, b, 255]) });
  assert.equal(thumbnailSimilarity(px(10, 20, 30), px(10, 20, 30)), 1);
  assert.ok(thumbnailSimilarity(px(0, 0, 0), px(255, 255, 255)) === 0);
  assert.deepEqual(parseAnswer('Sure!\n```json\n{"a": "x}y", "b": {"c": 1}}\n```'), { a: 'x}y', b: { c: 1 } });
  assert.deepEqual(toDrafts([{ prompt: ' a\nb ', style: 'x', avoid: [1, 'y'] }, { prompt: '' }, null]), [{ prompt: 'a b', style: ['x'], avoid: ['y'] }]);
  const flux = (id, aliases) => ({ id, family: 'flux', aliases });
  assert.deepEqual(refinementTargets([flux('bfl/flux-schnell', ['flux-schnell']), { id: 'sai/sdxl', family: 'sdxl', aliases: [] }]), { 'bfl/flux-schnell': 'flux', 'sai/sdxl': 'sdxl' });
  assert.deepEqual(refinementTargets([flux('bfl/flux-schnell', ['flux-schnell']), flux('bfl/flux-dev', ['flux-dev'])]), {}, 'a shared family is not refined');
  assert.equal(reduceAspect(30000, 100), '100:1');
  assert.equal(reduceAspect(100, 30000), '1:100');
  const doc = buildDocument({ prompt: 'x', style: [], avoid: [] }, { 'bfl/flux-schnell': ['y'] }, { 'bfl/flux-schnell': 'flux' }, ['bfl/flux-schnell'], '3:2');
  assert.deepEqual([...doc.keys()], ['prompt', 'prompt/flux', 'aspect', 'model']);
  const b = new Budget(1);
  const h = b.hold(0.6);
  assert.throws(() => b.hold(0.5), BudgetExceeded);
  h.charge('render', 'a', 0.2); h.release();
  assert.ok(Math.abs(b.left() - 0.8) < 1e-12);
});
