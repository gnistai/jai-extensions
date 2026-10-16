// Encoder engine with mocked providers: no network, no cost.
// The mock "image models" turn numbers in the description into measurable
// boxes, so the tests check that the loop really converges on the target.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { encode, measure, fitnessOf, describeObservation, Budget, BudgetExceeded, reduceAspect, thumbnailSimilarity, parseAnswer, toDrafts, toObservation, refinementTargets, buildDocument, cosine } from '../dist/encoder.mjs';
import { parse, interpret } from '../dist/jai.mjs';

const MODELS = [
  { id: 'acme/alpha', label: 'Alpha', family: 'alpha', aliases: [], imageInput: false, negativePrompt: false, shortWindow: false, costHint: 0.01, description: '' },
  { id: 'acme/beta', label: 'Beta', family: 'beta', aliases: [], imageInput: false, negativePrompt: false, shortWindow: false, costHint: 0.02, description: '' },
];
const TARGET_OBS = { subjects: [{ name: 'Earth', box: { ymin: 0.46, xmin: 0.49, ymax: 0.57, xmax: 0.64 } }], horizon: 0.8, horizonShape: 'flat', camera: 'wide', lighting: 'sun', medium: 'photograph' };
const target = { url: 'data:image/png;base64,AAAA', width: 960, height: 960 };

// A draft encodes its layout as "w=0.30 cx=0.40 cy=0.30 hz=0.50".
const params = text => Object.fromEntries([...text.matchAll(/(w|cx|cy|hz)=([0-9.]+)/g)].map(m => [m[1], Number(m[2])]));
const draftOf = p => ({ prompt: `Photograph of Earth over the Moon; w=${p.w.toFixed(3)} cx=${p.cx.toFixed(3)} cy=${p.cy.toFixed(3)} hz=${p.hz.toFixed(3)}`, style: ['photo'], avoid: [] });
const TRUE = { w: 0.15, cx: 0.565, cy: 0.515, hz: 0.8 };

function mockRenderer(log = []) {
  return {
    id: 'mock', label: 'Mock', models: () => MODELS,
    async render(req) {
      log.push({ model: req.modelId, description: req.intent.description, seed: req.intent.seed });
      return { bytes: new Uint8Array([1]), mime: 'image/png', model: req.modelId, sentPrompt: req.intent.description, sentNegative: null, sentInput: {}, jobId: 'j', diagnostics: [], outputURL: `mock:${req.modelId}:${req.intent.seed}:${encodeURIComponent(req.intent.description)}` };
    },
  };
}

/** Mock analyst: inspects by reading the numbers back; beta draws the Earth 1.5× too wide unless told "beta-fix". */
function mockAnalyst(calls = [], { start = { w: 0.4, cx: 0.3, cy: 0.25, hz: 0.5 }, step = 0.5, stuck = false } = {}) {
  return {
    id: 'mock-analyst', costHint: 0.002,
    async describe(t, n) { calls.push(['describe', n]); return { value: Array.from({ length: n }, (_, i) => draftOf({ ...start, w: start.w + i * 0.01 })), notes: 'first', cost: 0.002 }; },
    async inspect(image, subjects) {
      calls.push(['inspect', subjects ? 'render' : 'target']);
      if (!subjects) return { value: TARGET_OBS, notes: '', cost: 0.001 };
      const [, model, seed, enc] = image.url.match(/^mock:([^:]+):(\d+):(.*)$/);
      const text = decodeURIComponent(enc);
      const p = params(text);
      const noise = Number(seed) * 0.002;
      const w = p.w * (model === 'acme/beta' && !text.includes('beta-fix') ? 1.5 : 1) + noise;
      const h = w * (11 / 15);
      return { value: { subjects: [{ name: 'Earth', box: { xmin: p.cx - w / 2, xmax: p.cx + w / 2, ymin: p.cy - h / 2, ymax: p.cy + h / 2 } }], horizon: p.hz, horizonShape: 'flat', camera: 'wide', lighting: 'sun', medium: 'photograph' }, notes: '', cost: 0.001 };
    },
    async propose(t, ctx) {
      calls.push(['propose', ctx.scope.kind, ctx.history.length, ctx.history.map(h => h.fitness)]);
      if (ctx.scope.kind === 'refine') return { value: ['A small beta-fix detail.', 'Something irrelevant.'].slice(0, ctx.n), notes: 'beta draws it wide', cost: 0.002 };
      const bestP = params(ctx.history[0].draft.prompt);
      const next = stuck ? bestP : Object.fromEntries(Object.entries(bestP).map(([k, v]) => [k, v + (TRUE[k] - v) * step]));
      return { value: Array.from({ length: ctx.n }, (_, i) => draftOf({ ...next, w: next.w + (i - 1) * 0.004 })), notes: `step ${ctx.history.length}`, cost: 0.002 };
    },
  };
}

test('measure: numbers in, fitness and words out', () => {
  const same = measure(TARGET_OBS, TARGET_OBS);
  assert.ok(same.fitness > 0.99, String(same.fitness));
  assert.equal(same.feedback, 'matches the measured layout');
  const big = { ...TARGET_OBS, subjects: [{ name: 'earth', box: { ymin: 0.1, xmin: 0.37, ymax: 0.55, xmax: 0.73 } }], horizon: 0.5 };
  const m = measure(TARGET_OBS, big);
  assert.ok(m.fitness < 0.5, String(m.fitness));
  assert.match(m.feedback, /Earth: 2\.4× too wide, 4\.1× too tall/);
  assert.match(m.feedback, /too high/);
  assert.match(m.feedback, /^Largest first: /);
  assert.match(m.feedback, /horizon 30% of the frame too high \(at 50% from the top, target 80%\)/);
  assert.match(measure(TARGET_OBS, { ...TARGET_OBS, horizonShape: 'curved' }).feedback, /horizon is curved, target flat/);
  const missing = measure(TARGET_OBS, { ...TARGET_OBS, subjects: [{ name: 'Earth', box: null }] });
  assert.match(missing.feedback, /Earth: missing/);
  assert.ok(missing.fitness < same.fitness);
  const px = v => ({ width: 1, height: 1, data: new Uint8Array([v, v, v, 255]) });
  const bright = measure(TARGET_OBS, TARGET_OBS, { target: px(40), render: px(200) });
  assert.match(bright.feedback, /too bright/);
  assert.match(describeObservation(TARGET_OBS), /Earth: x 49%–64%, y 46%–57%/);
});

test('encoder: measured feedback drives the loop to the goal', async () => {
  const calls = [];
  const events = [];
  const r = await encode({ target, models: ['acme/alpha'], renderer: mockRenderer(), analyst: mockAnalyst(calls), budgetUSD: 2, population: 3, maxGenerations: 12, goal: 0.95, onEvent: e => events.push(e) });
  assert.equal(r.stopReason, 'goal');
  assert.ok(r.best.fitness >= 0.95, String(r.best.fitness));
  const fits = r.generations.map(g => g.bestFitness);
  for (let i = 1; i < fits.length; i++) assert.ok(fits[i] >= fits[i - 1]);
  assert.ok(fits[0] < 0.6, `started far away: ${fits[0]}`);
  assert.equal(calls.filter(c => c[0] === 'inspect' && c[1] === 'target').length, 1, 'the target is measured once');
  const proposeCalls = calls.filter(c => c[0] === 'propose');
  for (const c of proposeCalls) for (let i = 1; i < c[3].length; i++) assert.ok(c[3][i] <= c[3][i - 1], 'history is best first');
  assert.ok(events.some(e => e.type === 'target'));
  // Output conventions: aspect from pixels, model listed, no seed; two seeds per candidate.
  const doc = parse(r.text).document;
  assert.deepEqual(doc.get('aspect'), ['1:1']);
  assert.deepEqual(doc.get('model'), ['acme/alpha']);
  assert.equal(r.best.renders.length, 2);
});

test('encoder: the optimiser sees at most historySize entries', async () => {
  const calls = [];
  await encode({ target, models: ['acme/alpha'], renderer: mockRenderer(), analyst: mockAnalyst(calls, { step: 0.1 }), budgetUSD: 2, population: 4, maxGenerations: 5, historySize: 5, goal: 1 });
  const sizes = calls.filter(c => c[0] === 'propose').map(c => c[2]);
  assert.ok(sizes.length >= 3 && sizes.every(n => n <= 5), JSON.stringify(sizes));
});

test('encoder: stops on plateau when the optimiser makes no progress', async () => {
  const r = await encode({ target, models: ['acme/alpha'], renderer: mockRenderer(), analyst: mockAnalyst([], { stuck: true }), budgetUSD: 2, population: 2, maxGenerations: 10, plateau: 2 });
  assert.equal(r.stopReason, 'plateau');
  assert.equal(r.generations.length, 3);
});

test('encoder: never exceeds the budget; refuses to start when it cannot cover one round', async () => {
  const log = [];
  const r = await encode({ target, models: ['acme/alpha', 'acme/beta'], renderer: mockRenderer(log), analyst: mockAnalyst([], { step: 0.05 }), budgetUSD: 0.4, population: 4, maxGenerations: 50, goal: 1 });
  assert.equal(r.stopReason, 'budget');
  assert.ok(r.spent <= 0.4 + 1e-9, String(r.spent));
  assert.equal(r.spend.filter(l => l.kind === 'render').length, log.length, 'every render is charged');
  await assert.rejects(encode({ target, models: ['acme/alpha'], renderer: mockRenderer(), analyst: mockAnalyst(), budgetUSD: 0.01 }), e => e.stopReason === 'budget');
});

test('encoder: restarts run independent lineages and the best by fitness wins', async () => {
  let lineage = 0;
  const base = mockAnalyst([], { step: 0 });
  const analyst = { ...base, async describe(t, n) { lineage++; return { value: Array.from({ length: n }, () => draftOf(lineage === 2 ? { w: 0.16, cx: 0.56, cy: 0.52, hz: 0.79 } : { w: 0.4, cx: 0.3, cy: 0.25, hz: 0.5 })), notes: '', cost: 0.002 }; } };
  const r = await encode({ target, models: ['acme/alpha'], renderer: mockRenderer(), analyst, budgetUSD: 2, population: 2, maxGenerations: 1, restarts: 3, goal: 1 });
  assert.equal(r.best.lineage, 2);
  assert.ok(new Set(r.candidates.map(c => c.lineage)).size === 3);
});

test('encoder: a stronger optimiser can be plugged in; the analyst still measures', async () => {
  const calls = [], ocalls = [];
  const optimiser = mockAnalyst(ocalls);
  const r = await encode({ target, models: ['acme/alpha'], renderer: mockRenderer(), analyst: mockAnalyst(calls, { stuck: true }), optimiser, budgetUSD: 2, population: 2, maxGenerations: 8, goal: 0.95 });
  assert.equal(r.stopReason, 'goal');
  assert.ok(ocalls.some(c => c[0] === 'propose') && !calls.some(c => c[0] === 'propose'));
  assert.ok(calls.some(c => c[0] === 'inspect') && !ocalls.some(c => c[0] === 'inspect'));
});

test('encoder: refines the lagging model with prompt/<family> using its own measurements', async () => {
  const calls = [];
  const log = [];
  const r = await encode({ target, models: ['acme/beta', 'acme/alpha'], renderer: mockRenderer(log), analyst: mockAnalyst(calls), budgetUSD: 3, population: 3, maxGenerations: 6, goal: 0.97 });
  assert.ok(calls.some(c => c[0] === 'propose' && c[1] === 'refine'));
  const doc = parse(r.text).document;
  assert.deepEqual(doc.get('prompt/beta'), ['A small beta-fix detail.']);
  assert.ok(!log.some(l => l.model === 'acme/alpha' && l.description.includes('beta-fix')), 'alpha never sees the refinement');
  const it = interpret(doc, { catalog: MODELS.map(m => ({ ...m, default: false })), selected: 'acme/beta' }).interpreted;
  assert.ok(it.modelDescription.endsWith('\nA small beta-fix detail.'));
});

test('encoder: failed renders and failed inspections score 0 and are still charged', async () => {
  const base = mockRenderer();
  const flaky = { ...base, async render(req) { if (req.modelId === 'acme/beta') throw new Error('NSFW'); return base.render(req); } };
  const r = await encode({ target, models: ['acme/alpha', 'acme/beta'], renderer: flaky, analyst: mockAnalyst(), budgetUSD: 1, population: 2, maxGenerations: 2, refine: false, goal: 1 });
  assert.ok(r.candidates.every(c => c.perModel['acme/beta'] === 0));
  assert.ok(r.spend.some(l => l.label.includes('(failed)')));
  const a = mockAnalyst();
  let n = 0;
  const inspectFails = { ...a, async inspect(img, subjects, ctx) { if (subjects && ++n === 2) throw new Error('bad JSON'); return a.inspect(img, subjects, ctx); } };
  const r2 = await encode({ target, models: ['acme/alpha'], renderer: mockRenderer(), analyst: inspectFails, budgetUSD: 1, population: 2, maxGenerations: 1, goal: 1 });
  const failed = r2.candidates.flatMap(c => c.renders).filter(x => x.error?.startsWith('Inspection failed'));
  assert.equal(failed.length, 1);
  assert.equal(failed[0].score, 0);
  assert.ok(r2.spend.some(l => l.kind === 'inspect' && l.label.includes('(failed)')));
});

test('encoder: cancel stops with the best so far', async () => {
  const ac = new AbortController();
  const r = await encode({ target, models: ['acme/alpha'], renderer: mockRenderer(), analyst: mockAnalyst([], { step: 0.1 }), budgetUSD: 2, population: 2, maxGenerations: 20, goal: 1, signal: ac.signal, onEvent: e => { if (e.type === 'generation' && e.generation.index === 1) ac.abort(); } });
  assert.equal(r.stopReason, 'canceled');
  assert.ok(r.text.startsWith('jai:Photograph'));
});

test('encoder helpers', () => {
  assert.equal(fitnessOf({ a: 0.8 }), 0.8);
  assert.ok(Math.abs(fitnessOf({ a: 0.8, b: 0.4 }) - 0.5) < 1e-12);
  assert.equal(reduceAspect(1920, 1080), '16:9');
  assert.equal(reduceAspect(1024, 1024), '1:1');
  assert.equal(reduceAspect(3000, 2000), '3:2');
  assert.equal(reduceAspect(997, 211), '19:4');
  assert.equal(reduceAspect(30000, 100), '100:1');
  assert.equal(cosine([1, 0], [1, 0]), 1);
  const px = (r, g, b) => ({ width: 1, height: 1, data: new Uint8Array([r, g, b, 255]) });
  assert.equal(thumbnailSimilarity(px(10, 20, 30), px(10, 20, 30)), 1);
  assert.deepEqual(parseAnswer('Sure!\n```json\n{"a": "x}y", "b": {"c": 1}}\n```'), { a: 'x}y', b: { c: 1 } });
  assert.deepEqual(toDrafts([{ prompt: ' a\nb ', style: 'x', avoid: [1, 'y'] }, { prompt: '' }, null]), [{ prompt: 'a b', style: ['x'], avoid: ['y'] }]);
  const o = toObservation({ subjects: [{ name: 'earth', box_2d: [461, 492, 573, 645] }, { name: 'bad', box_2d: [5, 5, 1, 1] }], horizon_y: 805, camera: 'wide' }, ['Earth', 'ground']);
  assert.deepEqual(o.subjects.map(s => s.name), ['Earth', 'ground']);
  assert.ok(Math.abs(o.subjects[0].box.xmin - 0.492) < 1e-9 && o.subjects[1].box === null && Math.abs(o.horizon - 0.805) < 1e-9);
  const flux = (id, aliases) => ({ id, family: 'flux', aliases });
  assert.deepEqual(refinementTargets([flux('bfl/flux-schnell', ['flux-schnell']), { id: 'sai/sdxl', family: 'sdxl', aliases: [] }]), { 'bfl/flux-schnell': 'flux', 'sai/sdxl': 'sdxl' });
  const doc = buildDocument({ prompt: 'x', style: [], avoid: [] }, { 'bfl/flux-schnell': ['y'] }, { 'bfl/flux-schnell': 'flux' }, ['bfl/flux-schnell'], '3:2');
  assert.deepEqual([...doc.keys()], ['prompt', 'prompt/flux', 'aspect', 'model']);
  const b = new Budget(1);
  const h = b.hold(0.6);
  assert.throws(() => b.hold(0.5), BudgetExceeded);
  h.charge('render', 'a', 0.2); h.release();
  assert.ok(Math.abs(b.left() - 0.8) < 1e-12);
});
