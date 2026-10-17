// Encoder engine with mocked providers: no network, no cost.
// The mock "image models" turn numbers in the description into measurable
// boxes, so the tests check that the loop really converges on the target.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { encode, measure, fitnessOf, describeObservation, Budget, BudgetExceeded, reduceAspect, thumbnailSimilarity, parseAnswer, toDrafts, toObservation, refinementTargets, buildDocument } from '../dist/encoder.mjs';
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
      calls.push(['propose', ctx.scope.kind, ctx.history.length, ctx.history.map(h => h.fitness), ctx.n]);
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

test('measure: subjects pair by exact name, never by shared words; names listed twice pair in order', () => {
  const box = (xmin, xmax, ymin, ymax) => ({ xmin, xmax, ymin, ymax });
  const obs = subjects => ({ ...TARGET_OBS, horizon: null, horizonShape: '', subjects });
  for (const [a, b] of [['tree', 'large tree'], ['street', 'street lamp'], ['boat', 'small boat'], ['person', "person's shadow"]]) {
    for (const t of [obs([{ name: a, box: box(0.1, 0.3, 0.4, 0.8) }, { name: b, box: box(0.6, 0.9, 0.1, 0.9) }]), obs([{ name: b, box: box(0.6, 0.9, 0.1, 0.9) }, { name: a, box: box(0.1, 0.3, 0.4, 0.8) }])]) {
      const m = measure(t, t);
      assert.ok(m.fitness > 0.99, `${a} / ${b}: ${m.fitness}`);
      assert.equal(m.feedback, 'matches the measured layout');
    }
  }
  // The render inspection names each target subject; the shorter name missing leaves the longer one present.
  const t = obs([{ name: 'tree', box: box(0.1, 0.3, 0.4, 0.8) }, { name: 'large tree', box: box(0.6, 0.9, 0.1, 0.9) }]);
  const m = measure(t, obs([{ name: 'Tree', box: null }, { name: 'large  tree', box: box(0.6, 0.9, 0.1, 0.9) }]));
  assert.deepEqual(m.subjects.map(s => [s.name, s.present]), [['tree', false], ['large tree', true]]);
  assert.match(m.feedback, /^Largest first: tree: missing$/);
  // Two subjects with one name: the first box pairs with the first, the second with the second.
  const twins = obs([{ name: 'tree', box: box(0.1, 0.2, 0.5, 0.9) }, { name: 'tree', box: box(0.7, 0.8, 0.5, 0.9) }]);
  assert.ok(measure(twins, twins).fitness > 0.99);
  assert.deepEqual(measure(twins, obs([{ name: 'tree', box: box(0.1, 0.2, 0.5, 0.9) }, { name: 'tree', box: null }])).subjects.map(s => s.present), [true, false]);
  // Medium and light still compare by words.
  const looks = measure(TARGET_OBS, { ...TARGET_OBS, medium: 'digital photograph', lighting: 'sun light' });
  assert.doesNotMatch(looks.feedback, /looks like|light:/);
  assert.match(measure(TARGET_OBS, { ...TARGET_OBS, medium: 'oil painting' }).feedback, /looks like: oil painting \(target: photograph\)/);
});

test('toObservation: exact names first, then a changed name takes an answer no name claimed; none is used twice', () => {
  const ans = subjects => ({ subjects: subjects.map(([name, x]) => ({ name, box_2d: [100, x, 200, x + 100] })) });
  const xs = o => o.subjects.map(s => [s.name, s.box ? Math.round(s.box.xmin * 1000) : null]);
  // The analyst renamed one subject and answered it first.
  assert.deepEqual(xs(toObservation(ans([['bike', 100], ['wall', 500]]), ['wall', 'red bicycle'])), [['wall', 500], ['red bicycle', 100]]);
  // A name listed twice pairs by occurrence.
  assert.deepEqual(xs(toObservation(ans([['tree', 100], ['tree', 700]]), ['tree', 'tree'])), [['tree', 100], ['tree', 700]]);
  // Fewer answers than names: the rest have no box.
  assert.deepEqual(xs(toObservation(ans([['tree', 100]]), ['tree', 'large tree'])), [['tree', 100], ['large tree', null]]);
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

test('encoder: stops on plateau when the optimiser makes no progress, measuring no draft twice', async () => {
  const log = [];
  const r = await encode({ target, models: ['acme/alpha'], renderer: mockRenderer(log), analyst: mockAnalyst([], { stuck: true }), budgetUSD: 2, population: 2, maxGenerations: 10, plateau: 2 });
  assert.equal(r.stopReason, 'plateau');
  assert.ok(r.generations.length <= 3, String(r.generations.length));
  const prompts = r.candidates.map(c => c.draft.prompt);
  assert.equal(new Set(prompts).size, prompts.length, 'each draft is measured once');
  assert.equal(log.length, prompts.length * 2, 'two seeds per unique draft');
});

test('encoder: repeated drafts are skipped and the optimiser is asked once for the rest', async () => {
  const calls = [];
  const base = mockAnalyst(calls, { step: 0.5 });
  let first = true;
  const analyst = { ...base, async propose(t, ctx) {
    const r = await base.propose(t, ctx);
    if (first && ctx.scope.kind === 'core') { first = false; return { ...r, value: [ctx.history[0].draft, r.value[0], r.value[0], r.value[1]] }; }
    // The top-up sees the note about repeats and writes new variations.
    if (/repeated/.test(ctx.notes)) return { ...r, value: r.value.map(d => ({ ...d, prompt: `${d.prompt} (variation)` })) };
    return r;
  } };
  const r = await encode({ target, models: ['acme/alpha'], renderer: mockRenderer(), analyst, budgetUSD: 2, population: 4, maxGenerations: 2, goal: 2 });
  const proposals = calls.filter(c => c[0] === 'propose');
  assert.equal(proposals.length, 2, 'one proposal plus one top-up for the repeats');
  assert.equal(proposals[1][4], 2, 'the top-up asks for the two missing drafts');
  const gen1 = r.candidates.filter(c => c.generation === 1);
  assert.equal(gen1.length, 4);
  const prompts = r.candidates.map(c => c.draft.prompt);
  assert.equal(new Set(prompts).size, prompts.length);
});

test('encoder: no top-up when the optimiser wrote fewer drafts without repeats', async () => {
  const calls = [];
  const base = mockAnalyst(calls, { step: 0.5 });
  const analyst = { ...base, async propose(t, ctx) { const r = await base.propose(t, ctx); return { ...r, value: r.value.slice(0, ctx.n - 1) }; } };
  const r = await encode({ target, models: ['acme/alpha'], renderer: mockRenderer(), analyst, budgetUSD: 2, population: 4, maxGenerations: 3, goal: 2 });
  const proposals = calls.filter(c => c[0] === 'propose');
  assert.equal(proposals.length, 2, 'one proposal per generation after the first');
  assert.ok(proposals.every(c => c[4] === 4));
  assert.ok(!r.spend.some(l => l.label.includes('(more)')), 'no extra call is paid for');
  assert.deepEqual(r.generations.map(g => g.candidates.length), [4, 3, 3]);
});

test('encoder: the top-up asks only for the repeats, and only when the drafts in hand stay affordable', async () => {
  const calls = [];
  const base = mockAnalyst(calls, { step: 0.5 });
  // Three of four: one new draft, one repeat of a measured draft, one repeat within the answer.
  const analyst = { ...base, async propose(t, ctx) {
    const r = await base.propose(t, ctx);
    return /repeated/.test(ctx.notes) ? { ...r, value: r.value.map(d => ({ ...d, prompt: `${d.prompt} (variation)` })) } : { ...r, value: [r.value[0], ctx.history[0].draft, r.value[0]] };
  } };
  await encode({ target, models: ['acme/alpha'], renderer: mockRenderer(), analyst, budgetUSD: 2, population: 4, maxGenerations: 2, goal: 2 });
  assert.deepEqual(calls.filter(c => c[0] === 'propose').map(c => c[4]), [4, 2], 'two repeats, two asked for');
  // Spent before the proposal: the target 0.001, describe 0.002, two candidates 2 × 0.022; the proposal 0.002.
  // What is left then pays for a top-up (0.002) and one candidate (0.024), not for the draft in hand as well: no top-up.
  const tight = [];
  const budgetUSD = 0.001 + 0.002 + 2 * 0.022 + 0.002 + (0.002 + 0.024) + 0.001;
  const r = await encode({ target, models: ['acme/alpha'], renderer: mockRenderer(), analyst: { ...analyst, async propose(t, ctx) { tight.push(ctx.n); return analyst.propose(t, ctx); } }, budgetUSD, population: 2, maxGenerations: 2, goal: 2 });
  assert.deepEqual(tight, [2], String(r.spent));
  assert.equal(r.generations.at(-1).candidates.length, 1, 'the draft in hand is measured');
});

test('encoder: a failed top-up leaves the drafts in hand; the run goes on', async () => {
  const base = mockAnalyst([], { step: 0.2 });
  const events = [];
  const analyst = { ...base, async propose(t, ctx) {
    if (/repeated/.test(ctx.notes)) throw new Error("The analyst's answer was cut off");
    const r = await base.propose(t, ctx);
    return { ...r, value: [ctx.history[0].draft, ...r.value.slice(1)] };
  } };
  const r = await encode({ target, models: ['acme/alpha'], renderer: mockRenderer(), analyst, budgetUSD: 5, population: 4, maxGenerations: 5, plateau: 9, goal: 2, onEvent: e => events.push(e) });
  assert.equal(r.stopReason, 'generations');
  assert.equal(r.error, null);
  assert.equal(r.generations.length, 5);
  assert.ok(r.generations.slice(1).every(g => g.candidates.length === 3), 'the three fresh drafts are measured');
  assert.ok(events.some(e => e.type === 'log' && /asking for more descriptions failed \(The analyst's answer was cut off\); going on with 3/.test(e.message)));
});

test('encoder: an error ends the run with the best so far, its message in the result and the log', async () => {
  const base = mockAnalyst([], { step: 0.2 });
  let n = 0;
  const analyst = { ...base, async propose(t, ctx) { if (++n === 2) throw new Error('Chat completion failed (HTTP 500): engine crashed'); return base.propose(t, ctx); } };
  const events = [];
  const r = await encode({ target, models: ['acme/alpha'], renderer: mockRenderer(), analyst, budgetUSD: 2, population: 2, maxGenerations: 9, goal: 2, onEvent: e => events.push(e) });
  assert.equal(r.stopReason, 'error');
  assert.equal(r.error, 'Chat completion failed (HTTP 500): engine crashed');
  assert.equal(r.generations.length, 2);
  assert.deepEqual(events.filter(e => e.type === 'log').map(e => e.message), ['Stopped: Chat completion failed (HTTP 500): engine crashed']);
});

test('encoder: a cancel during an analyst call stops as canceled, not as an error', async () => {
  for (const at of ['describe', 'propose', 'top-up']) {
    const ac = new AbortController();
    const base = mockAnalyst([], { step: 0.2 });
    const pending = signal => new Promise((_, reject) => { signal.addEventListener('abort', () => reject(new Error('Canceled'))); queueMicrotask(() => ac.abort()); });
    const analyst = { ...base,
      async describe(t, n, ctx) { return at === 'describe' ? pending(ctx.signal) : base.describe(t, n, ctx); },
      async propose(t, ctx) {
        if (at === 'propose' || /repeated/.test(ctx.notes)) return pending(ctx.signal);
        const r = await base.propose(t, ctx);
        return at === 'top-up' ? { ...r, value: [ctx.history[0].draft, ...r.value.slice(1)] } : r;
      } };
    const run = encode({ target, models: ['acme/alpha'], renderer: mockRenderer(), analyst, budgetUSD: 2, population: 2, maxGenerations: 9, goal: 2, signal: ac.signal });
    if (at === 'describe') { await assert.rejects(run, e => e.stopReason === 'canceled', at); continue; }
    const r = await run;
    assert.equal(r.stopReason, 'canceled', at);
    assert.equal(r.error, null, at);
  }
});

test('encoder: renders and inspections have their own slots, so waiting inspections never hold up renders', async () => {
  // Like a local server: an inspection is answered only once the image engine has no renders left.
  // With one shared pool, the inspections would hold every slot and the renders they wait for never start.
  let rendering = 0, renders = 0, inspecting = 0, maxRendering = 0, maxInspecting = 0, stalled = 0;
  const base = mockRenderer();
  const renderer = { ...base, async render(req) {
    maxRendering = Math.max(maxRendering, ++rendering);
    await new Promise(r => setTimeout(r, 2));
    rendering--; renders++;
    return base.render(req);
  } };
  const a = mockAnalyst([], { step: 0.5 });
  const analyst = { ...a, async inspect(image, subjects, ctx) {
    if (!subjects) return a.inspect(image, subjects, ctx);
    maxInspecting = Math.max(maxInspecting, ++inspecting);
    const until = Date.now() + 1000;
    while (renders % 6 && Date.now() < until) await new Promise(r => setTimeout(r, 1));
    if (renders % 6) stalled++;
    await new Promise(r => setTimeout(r, 1));
    inspecting--;
    return a.inspect(image, subjects, ctx);
  } };
  const r = await encode({ target, models: ['acme/alpha'], renderer, analyst, budgetUSD: 2, population: 3, seeds: [1, 2], maxGenerations: 2, goal: 2, concurrency: 2 });
  assert.equal(r.candidates.length, 6);
  assert.equal(stalled, 0, 'no inspection waited for renders that could not start');
  assert.equal(maxRendering, 2, 'at most two renders at once');
  assert.equal(maxInspecting, 2, 'at most two inspections at once');
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
  assert.ok(r.text.startsWith('Photograph'));
});

test('encoder helpers', () => {
  assert.equal(fitnessOf({ a: 0.8 }), 0.8);
  assert.ok(Math.abs(fitnessOf({ a: 0.8, b: 0.4 }) - 0.5) < 1e-12);
  assert.equal(reduceAspect(1920, 1080), '16:9');
  assert.equal(reduceAspect(1024, 1024), '1:1');
  assert.equal(reduceAspect(3000, 2000), '3:2');
  assert.equal(reduceAspect(997, 211), '19:4');
  assert.equal(reduceAspect(30000, 100), '100:1');
  const px = (r, g, b) => ({ width: 1, height: 1, data: new Uint8Array([r, g, b, 255]) });
  assert.equal(thumbnailSimilarity(px(10, 20, 30), px(10, 20, 30)), 1);
  assert.deepEqual(parseAnswer('Sure!\n```json\n{"a": "x}y", "b": {"c": 1}}\n```'), { a: 'x}y', b: { c: 1 } });
  assert.deepEqual(toDrafts([{ prompt: ' a\nb ', style: 'x', avoid: [1, 'y'] }, { prompt: '' }, null]), [{ prompt: 'a b', style: ['x'], avoid: ['y'] }]);
  const o = toObservation({ subjects: [{ name: 'earth', box_2d: [461, 492, 573, 645] }, { name: 'bad', box_2d: [5, 5, 1, 1] }], horizon_y: 805, camera: 'wide' }, ['Earth', 'ground']);
  assert.deepEqual(o.subjects.map(s => s.name), ['Earth', 'ground']);
  assert.ok(Math.abs(o.subjects[0].box.xmin - 0.492) < 1e-9 && o.subjects[1].box === null && Math.abs(o.horizon - 0.805) < 1e-9);
  // Qwen's order: bbox_2d [x1, y1, x2, y2] gives the same box.
  const q = toObservation({ subjects: [{ name: 'earth', bbox_2d: [492, 461, 645, 573] }, { name: 'ground', box_2d: [0, 735, 1000, 1000] }] }, ['Earth', 'ground'], 'xy');
  assert.deepEqual(q.subjects[0].box, o.subjects[0].box);
  assert.equal(q.subjects[1].box, null, 'the other field name is not read');
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
