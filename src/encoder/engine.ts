// Analysis-by-synthesis image encoder (spec: Encoding images, informative).
// Phase A evolves the core description on the mean score across the target
// models; phase B tries short prompt/<target> refinements for models that lag.
// Documents are built and read only through the JAI library.
import { fromObject, interpret, serialize, type CatalogModel, type JaiDocument } from '../jai/index.ts';
import type { ModelInfo, RenderIntent, RenderResult } from '../renderers/types.ts';
import { Budget, BudgetExceeded } from './budget.ts';
import { reduceAspect, thumbnailSimilarity } from './metrics.ts';
import type { Candidate, Draft, EncodeOptions, EncodeResult, Generation, RenderView, ScoredRender, StopReason } from './types.ts';

const SEGMENT = /^[a-z0-9][a-z0-9_-]*$/;

/**
 * The qualifier target for each model (spec output convention): its family
 * when no other target model shares it, else an alias that names it alone.
 * Models with neither get no entry and are not refined.
 */
export function refinementTargets(models: ModelInfo[]): Record<string, string> {
  const out: Record<string, string> = {};
  const lower = (s: string): string => s.replace(/[A-Z]/g, c => c.toLowerCase());
  const namesOf = (m: ModelInfo): string[] => [m.family, m.id, ...m.aliases].filter(Boolean).map(lower);
  for (const m of models) {
    const options = [m.family, ...m.aliases].filter(Boolean).map(lower).filter(t => SEGMENT.test(t));
    const unique = options.find(t => models.every(o => o === m || !namesOf(o).includes(t)));
    if (unique) out[m.id] = unique;
  }
  return out;
}

/** Build the encoder's output document (spec: Encoding images, output conventions). */
export function buildDocument(draft: Draft, refinements: Record<string, string[]>, targets: Record<string, string>, modelOrder: string[], aspect: string | null): JaiDocument {
  const obj: Record<string, string[]> = { prompt: [draft.prompt] };
  if (draft.style.length) obj.style = draft.style;
  if (draft.avoid.length) obj.avoid = draft.avoid;
  for (const [model, values] of Object.entries(refinements)) {
    const t = targets[model];
    if (!t || !values.length) continue;
    obj[`prompt/${t}`] = [...(obj[`prompt/${t}`] ?? []), ...values];
  }
  if (aspect) obj.aspect = [aspect];
  if (modelOrder.length) obj.model = modelOrder;
  return fromObject(obj);
}

const toB64 = (bytes: Uint8Array): string => {
  let s = '';
  for (let i = 0; i < bytes.length; i += 0x8000) s += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
  return btoa(s);
};
const imageOf = (r: RenderResult): { url: string } => ({ url: r.outputURL ?? `data:${r.mime};base64,${toB64(r.bytes)}` });

function pool(limit: number): <T>(f: () => Promise<T>) => Promise<T> {
  let active = 0;
  const queue: Array<() => void> = [];
  return f => new Promise((resolve, reject) => {
    const run = (): void => {
      active++;
      f().then(resolve, reject).finally(() => { active--; queue.shift()?.(); });
    };
    if (active < limit) run(); else queue.push(run);
  });
}

/** One model: its score. Several: (mean + worst) / 2. */
export function fitnessOf(perModel: Record<string, number>): number {
  const v = Object.values(perModel);
  if (!v.length) return 0;
  const mean = v.reduce((a, x) => a + x, 0) / v.length;
  return v.length === 1 ? mean : (mean + Math.min(...v)) / 2;
}

/** Words a short-window (CLIP-class, 77-token) model reliably reads. */
const SHORT_WINDOW_WORDS = 55;

class Stop extends Error { constructor(readonly reason: StopReason, message: string = reason) { super(message); } }

export async function encode(opts: EncodeOptions): Promise<EncodeResult> {
  const t0 = Date.now();
  const emit = opts.onEvent ?? ((): void => {});
  const population = Math.max(1, Math.floor(opts.population ?? 4));
  const maxGenerations = Math.max(1, Math.floor(opts.maxGenerations ?? 5));
  const plateau = Math.max(1, Math.floor(opts.plateau ?? 2));
  const minGain = opts.minGain ?? 0.005;
  const seeds = opts.seeds?.length ? opts.seeds : [1];
  const refine = opts.refine ?? opts.models.length > 1;
  const refineShare = refine ? Math.min(0.9, Math.max(0, opts.refineShare ?? 0.25)) : 0;
  const refineTries = Math.max(1, Math.floor(opts.refineTries ?? 2));
  const pixelWeight = opts.thumbnail && opts.targetThumbnail ? Math.min(1, Math.max(0, opts.pixelWeight ?? 0.25)) : 0;
  const judgeWeight = opts.analyst.judge ? Math.min(1, Math.max(0, opts.judgeWeight ?? 0.5)) : 0;
  const judgeLimit = Math.max(2, opts.analyst.judgeLimit ?? 9);
  const limit = pool(Math.max(1, opts.concurrency ?? 4));
  const budget = new Budget(opts.budgetUSD);
  const ceilingA = opts.budgetUSD * (1 - refineShare);

  const all = opts.renderer.models();
  const chosen = opts.models.map(id => {
    const m = all.find(x => x.id === id);
    if (!m) throw new RangeError(`Unknown model ${id}`);
    return m;
  });
  if (!chosen.length) throw new RangeError('Choose at least one target model');
  const labels = Object.fromEntries(chosen.map(m => [m.id, m.label]));
  const catalog: CatalogModel[] = all.map(m => ({ id: m.id, aliases: m.aliases, family: m.family, imageInput: m.imageInput, default: false }));
  const targets = refinementTargets(chosen);
  const aspect = reduceAspect(opts.target.width, opts.target.height);
  const ctx = { models: chosen.map(m => m.label), signal: opts.signal };
  const evalCost = (models: string[]): number => models.reduce((a, id) => a + seeds.length * ((all.find(m => m.id === id)?.costHint ?? 0) + opts.scorer.costHint), 0);
  /** Judge calls for `count` candidates (one anchor repeated in every call), per model. */
  const judgeCost = (count: number, models: number): number => (judgeWeight > 0 && count > 0 ? models * Math.ceil(Math.max(1, count - 1) / (judgeLimit - 1)) * opts.analyst.costHint : 0);

  const candidates: Candidate[] = [];
  const generations: Generation[] = [];
  let best: Candidate | null = null;
  let stopReason: StopReason = 'complete';
  let error: string | null = null;

  const checkpoint = (): void => {
    if (opts.signal?.aborted) throw new Stop('canceled');
    if (opts.timeLimitMs && Date.now() - t0 > opts.timeLimitMs) throw new Stop('time');
  };
  const spend = (): void => emit({ type: 'spend', spent: budget.spent, limit: budget.limit });

  async function callAnalyst<T>(f: () => Promise<{ value: T; notes: string; cost: number }>, label: string, ceiling: number): Promise<{ value: T; notes: string }> {
    checkpoint();
    const hold = budget.hold(opts.analyst.costHint, ceiling);
    try {
      const r = await f();
      hold.charge('analyst', label, r.cost);
      return r;
    } catch (e) {
      hold.charge('analyst', label, opts.analyst.costHint);
      throw e;
    } finally { hold.release(); spend(); }
  }

  async function evaluate(c: Omit<Candidate, 'renders' | 'perModel' | 'fitness'>, models: string[], ceiling: number): Promise<Candidate> {
    checkpoint();
    const hold = budget.hold(evalCost(models), ceiling);
    const doc = buildDocument(c.draft, c.refinements, targets, opts.models, aspect);
    try {
      const renders = await Promise.all(models.flatMap(model => seeds.map(seed => limit(async (): Promise<ScoredRender> => {
        const it = interpret(doc, { catalog, selected: model }).interpreted;
        const intent: RenderIntent = {
          description: it.description, style: it.style, avoid: it.avoid,
          aspect: it.aspect ? { width: it.aspect.width, height: it.aspect.height } : null,
          mode: 'generate', strength: null, seed: String(seed), animate: false,
        };
        const m = all.find(x => x.id === model)!;
        let result: RenderResult | null = null;
        try {
          result = await opts.renderer.render({ intent, modelId: model, inputImage: null, governingRatio: null, target: null, signal: opts.signal });
        } catch (e) {
          if (opts.signal?.aborted) throw new Stop('canceled');
          hold.charge('render', `${c.id} ${model} (failed)`, m.costHint);
          return { model, seed, result: null, error: (e as Error).message, semantic: 0, pixel: null, judge: null, metric: 0, score: 0 };
        }
        hold.charge('render', `${c.id} ${model}`, m.costHint);
        const semantic = await opts.scorer.score(imageOf(result), opts.signal);
        hold.charge('scorer', `${c.id} ${model}`, opts.scorer.costHint);
        let pixel: number | null = null;
        if (pixelWeight > 0 && opts.thumbnail && opts.targetThumbnail) {
          try { pixel = thumbnailSimilarity(opts.targetThumbnail, await opts.thumbnail(result)); } catch { pixel = null; }
        }
        const metric = pixel === null ? semantic : (1 - pixelWeight) * semantic + pixelWeight * pixel;
        return { model, seed, result, error: null, semantic, pixel, judge: null, metric, score: metric };
      }))));
      const cand: Candidate = { ...c, renders, perModel: {}, fitness: 0 };
      tally(cand);
      candidates.push(cand);
      return cand;
    } finally { hold.release(); spend(); }
  }

  /** Recompute per-model means and fitness from the render scores. */
  function tally(c: Candidate): void {
    const models = [...new Set(c.renders.map(r => r.model))];
    for (const model of models) {
      const rs = c.renders.filter(r => r.model === model);
      c.perModel[model] = rs.reduce((a, r) => a + r.score, 0) / rs.length;
    }
    c.fitness = fitnessOf(c.perModel);
  }

  /**
   * Ask the analyst to rate the renders of `set` for each model, the first
   * candidate (the current best, or the first new one) included in every call
   * as the anchor, and blend the rating into the scores. If a call fails, the
   * renders in that call fall back to the metric alone, the anchor included,
   * so that comparisons stay within one scale.
   */
  async function judgeSet(set: Candidate[], models: string[], ceiling: number): Promise<string> {
    if (judgeWeight === 0 || !opts.analyst.judge) return '';
    const notes: string[] = [];
    for (const model of models) {
      const entries = set.map(c => ({ c, r: c.renders.find(r => r.model === model && r.seed === seeds[0] && r.result) })).filter(e => e.r);
      if (entries.length < 2) continue;
      const [anchor, ...rest] = entries;
      for (let i = 0; i < rest.length; i += judgeLimit - 1) {
        const chunk = [anchor!, ...rest.slice(i, i + judgeLimit - 1)];
        const view = chunk.map(e => ({ model, label: labels[model] ?? model, image: imageOf(e.r!.result!), score: e.r!.metric }));
        let ratings: number[] | null = null;
        try {
          const reply = await callAnalyst(() => opts.analyst.judge!(opts.target, view, ctx), `judge ${model}`, ceiling);
          ratings = reply.value;
          if (reply.notes) notes.push(`${labels[model]}: ${reply.notes}`);
        } catch (e) {
          if (e instanceof Stop || e instanceof BudgetExceeded) throw e;
          emit({ type: 'log', message: `Judging ${labels[model]} failed (${(e as Error).message}); using the metric alone for these renders.` });
        }
        chunk.forEach((e, k) => {
          for (const r of e.c.renders.filter(x => x.model === model && x.result)) {
            r.judge = ratings ? ratings[k]! : null;
            r.score = r.judge === null ? r.metric : (1 - judgeWeight) * r.metric + judgeWeight * r.judge;
          }
        });
      }
    }
    for (const c of set) tally(c);
    return notes.join(' ');
  }

  /** Evaluate as many drafts as the budget allows, in parallel, then judge them against the current best. */
  async function evaluateAll(drafts: Draft[], gen: number, ceiling: number): Promise<{ next: Candidate[]; notes: string }> {
    const cost = evalCost(opts.models);
    const anchor = best ? 1 : 0;
    const affordable = drafts.filter((_, i) => budget.canAfford(cost * (i + 1) + judgeCost(i + 1 + anchor, opts.models.length), ceiling));
    if (!affordable.length) throw new Stop('budget');
    const next = await Promise.all(affordable.map((draft, k) => evaluate({ id: `g${gen}c${k + 1}`, generation: gen, phase: 'evolve', draft, refinements: {} }, opts.models, ceiling)));
    const notes = await judgeSet(best ? [best, ...next] : next, opts.models, ceiling);
    for (const c of next) emit({ type: 'candidate', candidate: c });
    return { next, notes };
  }

  const views = (c: Candidate, models = opts.models): RenderView[] => c.renders
    .filter(r => r.result && r.seed === seeds[0] && models.includes(r.model))
    .map(r => ({ model: r.model, label: labels[r.model] ?? r.model, image: imageOf(r.result!), score: r.score }));

  try {
    // Phase A: describe, then evolve the core.
    emit({ type: 'phase', phase: 'describe' });
    if (!budget.canAfford(opts.analyst.costHint + opts.scorer.costHint + evalCost(opts.models), ceilingA)) throw new Stop('budget', 'The budget does not cover one description and one round of renders.');
    if (judgeWeight > 0) emit({ type: 'log', message: `Scores blend CLIP${pixelWeight ? ', colour/layout' : ''} and the analyst's rating (weight ${judgeWeight}).` });
    checkpoint();
    const hold = budget.hold(opts.scorer.costHint, ceilingA);
    try { await opts.scorer.prepare(opts.target, opts.signal); hold.charge('scorer', 'target', opts.scorer.costHint); }
    finally { hold.release(); }
    const first = await callAnalyst(() => opts.analyst.describe(opts.target, population, ctx), 'describe', ceilingA);
    if (!first.value.length) throw new Stop('error', 'The analyst wrote no usable description.');
    emit({ type: 'phase', phase: 'evolve' });
    let gen = 0;
    const g0 = await evaluateAll(first.value, gen, ceilingA);
    best = g0.next.reduce((a, b) => (b.fitness > a.fitness ? b : a));
    generations.push({ index: gen, phase: 'evolve', candidates: g0.next.map(c => c.id), best: best.id, bestFitness: best.fitness, notes: [first.notes, g0.notes].filter(Boolean).join(' '), spent: budget.spent });
    emit({ type: 'generation', generation: generations[generations.length - 1]! });
    let stale = 0;
    stopReason = 'generations';
    while (++gen < maxGenerations) {
      if (!budget.canAfford(opts.analyst.costHint + evalCost(opts.models) + judgeCost(2, opts.models.length), ceilingA)) { stopReason = 'budget'; break; }
      const n = Math.max(1, population - 1);
      const rev = await callAnalyst(() => opts.analyst.revise(opts.target, best!.draft, views(best!), n, ctx), `revise g${gen}`, ceilingA);
      if (!rev.value.length) { emit({ type: 'log', message: `Generation ${gen}: the analyst wrote no usable revision.` }); stale++; if (stale >= plateau) { stopReason = 'plateau'; break; } continue; }
      let next: Candidate[];
      let judged = '';
      const before = best.fitness;
      try { ({ next, notes: judged } = await evaluateAll(rev.value, gen, ceilingA)); } catch (e) { if (e instanceof Stop && e.reason === 'budget') { stopReason = 'budget'; break; } throw e; }
      if (best.fitness !== before) emit({ type: 'log', message: `Re-rated the best (${best.id}): ${before.toFixed(3)} -> ${best.fitness.toFixed(3)}` });
      const top = next.reduce((a, b) => (b.fitness > a.fitness ? b : a));
      const gain = top.fitness - best.fitness;
      if (gain > 0) best = top;
      stale = gain >= minGain ? 0 : stale + 1;
      generations.push({ index: gen, phase: 'evolve', candidates: next.map(c => c.id), best: best.id, bestFitness: best.fitness, notes: [rev.notes, judged].filter(Boolean).join(' '), spent: budget.spent });
      emit({ type: 'generation', generation: generations[generations.length - 1]! });
      if (stale >= plateau) { stopReason = 'plateau'; break; }
    }

    // Phase B: per-model refinements for the models that lag.
    if (refine && chosen.length > 1) {
      emit({ type: 'phase', phase: 'refine' });
      const order = [...opts.models].sort((a, b) => best!.perModel[a]! - best!.perModel[b]!);
      const leader = order[order.length - 1];
      for (const model of order) {
        if (model === leader || !targets[model]) continue;
        const info = all.find(m => m.id === model)!;
        const words = (best.draft.style.join(' ') + ' ' + best.draft.prompt).split(/\s+/).filter(Boolean).length;
        if (info.shortWindow && words >= SHORT_WINDOW_WORDS) {
          emit({ type: 'log', message: `Not refining ${labels[model]}: it reads only about ${SHORT_WINDOW_WORDS} words and the core already has ${words}, so an appended refinement would be cut off.` });
          continue;
        }
        if (!budget.canAfford(opts.analyst.costHint + evalCost([model]) + judgeCost(2, 1))) break;
        const view = views(best, [model])[0];
        if (!view) continue;
        const ref = await callAnalyst(() => opts.analyst.refine(opts.target, best!.draft, view, refineTries, ctx), `refine ${model}`, opts.budgetUSD);
        const tried: Candidate[] = [];
        for (const [k, addition] of ref.value.entries()) {
          if (!budget.canAfford(evalCost([model]) + judgeCost(tried.length + 2, 1))) break;
          tried.push(await evaluate({ id: `r${gen}-${targets[model]}-${k + 1}`, generation: gen, phase: 'refine', draft: best.draft, refinements: { ...best.refinements, [model]: [addition] } }, [model], opts.budgetUSD));
        }
        const judged = tried.length ? await judgeSet([best, ...tried], [model], opts.budgetUSD) : '';
        for (const c of tried) emit({ type: 'candidate', candidate: c });
        const top = tried.reduce<Candidate | null>((a, b) => (!a || b.perModel[model]! > a.perModel[model]! ? b : a), null);
        const accepted = !!top && top.perModel[model]! - best.perModel[model]! >= minGain;
        if (top && accepted) {
          const perModel: Record<string, number> = { ...best.perModel, [model]: top.perModel[model]! };
          best = { ...best, id: top.id, phase: 'refine', refinements: top.refinements, perModel, fitness: fitnessOf(perModel),
            renders: [...best.renders.filter(r => r.model !== model), ...top.renders] };
        }
        generations.push({ index: gen, phase: 'refine', candidates: tried.map(c => c.id), best: best.id, bestFitness: best.fitness, notes: `${labels[model]}: ${ref.notes}${judged ? ` ${judged}` : ''}${accepted ? '' : ' (no addition kept)'}`, spent: budget.spent });
        emit({ type: 'generation', generation: generations[generations.length - 1]! });
        gen++;
      }
    }
  } catch (e) {
    if (e instanceof Stop) { stopReason = e.reason; if (e.reason === 'error' || e.message !== e.reason) error = e.message; }
    else if (e instanceof BudgetExceeded) stopReason = 'budget';
    else { stopReason = 'error'; error = (e as Error).message; }
    if (!best) {
      const err = new Error(error ?? `Stopped before any candidate was scored (${stopReason})`) as Error & { stopReason?: StopReason; spent?: number };
      err.stopReason = stopReason; err.spent = budget.spent;
      throw err;
    }
  }

  emit({ type: 'phase', phase: 'done' });
  const ranking = [...opts.models].map(model => ({ model, score: best!.perModel[model] ?? 0 })).sort((a, b) => b.score - a.score);
  const document = buildDocument(best!.draft, best!.refinements, targets, ranking.map(r => r.model), aspect);
  return {
    document, text: serialize(document), best: best!, candidates, generations, ranking,
    spent: budget.spent, spend: budget.lines, stopReason, error, seconds: (Date.now() - t0) / 1000,
  };
}
