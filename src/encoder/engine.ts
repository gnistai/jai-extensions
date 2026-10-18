// Analysis-by-synthesis image encoder (spec: Encoding images, informative).
// Measure, feed back numbers, optimise from history: the target is inspected
// once (subjects, people and their visible traits, horizon); every candidate is
// rendered with every target model and several seeds, unless it names a group;
// each render is inspected the same way and its fitness is computed from the
// measured differences; an optimiser sees the best past candidates with their
// measured errors and writes the next ones (OPRO-style). Lagging models then get
// short prompt/<family> refinements by the same mechanism. Independent
// lineages share the budget, and the best by fitness wins: fitness is computed
// against one target measurement, so it compares across lineages.
// Documents are built and read only through the JAI library.
import { fromObject, interpret, serialize, type CatalogModel, type JaiDocument } from '../jai/index.ts';
import type { ModelInfo, RenderIntent, RenderResult } from '../renderers/types.ts';
import { Budget, BudgetExceeded } from './budget.ts';
import { measure, reduceAspect } from './metrics.ts';
import { refusal } from './prompts.ts';
import type { Analyst, AnalystReply, Candidate, Draft, EncodeOptions, EncodeResult, Generation, HistoryEntry, Observation, ScoredRender, SpendLine, StopReason, Thumbnail } from './types.ts';

const SEGMENT = /^[a-z0-9][a-z0-9_-]*$/;

/**
 * The qualifier target for each model (spec output conventions): its family
 * name, which is the only name other readers share. Models whose family is
 * shared with another target model, or that have no family, are not refined.
 */
export function refinementTargets(models: ModelInfo[]): Record<string, string> {
  const out: Record<string, string> = {};
  const lower = (s: string): string => s.replace(/[A-Z]/g, c => c.toLowerCase());
  for (const m of models) {
    const fam = m.family ? lower(m.family) : '';
    if (!SEGMENT.test(fam)) continue;
    if (models.some(o => o !== m && o.family && lower(o.family) === fam)) continue;
    out[m.id] = fam;
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

/** A finite number, else the default (options may come from untrusted forms or flags). */
const fin = (v: number | undefined, d: number): number => (typeof v === 'number' && Number.isFinite(v) ? v : d);

/** An error's message; providers may throw errors from another realm, or non-errors. */
const messageOf = (e: unknown): string => {
  const m = (e as { message?: unknown } | null)?.message;
  return typeof m === 'string' ? m : String(e);
};

class Stop extends Error { constructor(readonly reason: StopReason, message: string = reason) { super(message); } }

export async function encode(opts: EncodeOptions): Promise<EncodeResult> {
  const t0 = Date.now();
  const emit = opts.onEvent ?? ((): void => {});
  const population = Math.max(1, Math.floor(fin(opts.population, 4)));
  const maxGenerations = Math.max(1, Math.floor(fin(opts.maxGenerations, 8)));
  const plateau = Math.max(1, Math.floor(fin(opts.plateau, 4)));
  const minGain = fin(opts.minGain, 0.01);
  const goal = Math.min(1, Math.max(0, fin(opts.goal, 0.9)));
  const validSeeds = (opts.seeds ?? [1, 2]).filter(x => Number.isSafeInteger(x) && x >= 0);
  const seeds = validSeeds.length ? validSeeds : [1, 2];
  const restarts = Math.max(1, Math.floor(fin(opts.restarts, 1)));
  const historySize = Math.max(1, Math.floor(fin(opts.historySize, 8)));
  const refine = opts.refine ?? opts.models.length > 1;
  const refineShare = refine ? Math.min(0.9, Math.max(0, fin(opts.refineShare, 0.2))) : 0;
  const refineTries = Math.max(1, Math.floor(fin(opts.refineTries, 3)));
  // Renders and inspections each get their own slots: a render that finished
  // never waits for an inspection slot to free a render slot, so both kinds keep
  // flowing, and a local server never holds more than a few requests of a kind.
  const slots = Math.max(1, Math.floor(fin(opts.concurrency, 4)));
  const renderSlot = pool(slots), inspectSlot = pool(slots);
  const budget = new Budget(opts.budgetUSD);
  const ceilingA = opts.budgetUSD * (1 - refineShare);
  const analyst = opts.analyst;
  const optimiser = opts.optimiser ?? opts.analyst;

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
  // A model with a 77-token text encoder reads only the first ~55 words: keep every description inside that.
  const maxWords = chosen.some(m => m.shortWindow) ? SHORT_WINDOW_WORDS - 5 : 120;
  const ctx = { models: chosen.map(m => m.label), maxWords, signal: opts.signal };
  const wordsIn = (d: Draft): number => (d.style.join(' ') + ' ' + d.prompt).split(/\s+/).filter(Boolean).length;
  /** One candidate on `models`: a render and an inspection per model and seed. */
  const evalCost = (models: string[]): number => models.reduce((a, id) => a + seeds.length * ((all.find(m => m.id === id)?.costHint ?? 0) + analyst.costHint), 0);

  const candidates: Candidate[] = [];
  const generations: Generation[] = [];
  let best: Candidate | null = null;
  let targetObs: Observation | null = null;
  let stopReason: StopReason = 'complete';
  let error: string | null = null;

  const checkpoint = (): void => {
    if (opts.signal?.aborted) throw new Stop('canceled');
    if (opts.timeLimitMs && Date.now() - t0 > opts.timeLimitMs) throw new Stop('time');
  };
  const spend = (): void => emit({ type: 'spend', spent: budget.spent, limit: budget.limit });

  async function call<T>(who: Analyst, f: () => Promise<AnalystReply<T>>, kind: SpendLine['kind'], label: string, ceiling: number): Promise<AnalystReply<T>> {
    checkpoint();
    const hold = budget.hold(who.costHint, ceiling);
    try {
      const r = await f();
      hold.charge(kind, label, r.cost);
      return r;
    } catch (e) {
      hold.charge(kind, label, who.costHint);
      // A cancel makes the provider call fail: report it as the cancel it is.
      if (opts.signal?.aborted) throw new Stop('canceled');
      throw e;
    } finally { hold.release(); spend(); }
  }

  async function evaluate(c: Omit<Candidate, 'renders' | 'perModel' | 'fitness'>, models: string[], ceiling: number): Promise<Candidate> {
    checkpoint();
    const hold = budget.hold(evalCost(models), ceiling);
    const doc = buildDocument(c.draft, c.refinements, targets, opts.models, aspect);
    const names = targetObs!.subjects.filter(x => x.box).map(x => x.name);
    let broken = false;
    /** Before each request: stop on a cancel, or when another task of this batch failed unexpectedly. */
    const proceed = (): void => {
      if (opts.signal?.aborted) throw new Stop('canceled');
      if (broken) throw new Stop('error', 'An earlier render in this batch failed unexpectedly.');
    };
    /** One render and its inspection, each in its own slot. */
    const task = async (model: string, seed: number): Promise<ScoredRender> => {
      const m = all.find(x => x.id === model)!;
      const rendered = await renderSlot(async (): Promise<{ result: RenderResult } | { error: string }> => {
        proceed();
        const it = interpret(doc, { catalog, selected: model }).interpreted;
        try {
          const result = await opts.renderer.render({
            intent: { description: it.modelDescription, style: it.modelStyle, avoid: it.modelAvoid, aspect: it.aspect ? { width: it.aspect.width, height: it.aspect.height } : null, mode: 'generate', strength: null, seed: String(seed) },
            modelId: model, inputImage: null, governingRatio: null, previewRatio: null, target: null, signal: opts.signal,
          });
          hold.charge('render', `${c.id} ${model}`, m.costHint);
          return { result };
        } catch (e) {
          // The job may have run even if it failed or was canceled: charge it either way.
          hold.charge('render', `${c.id} ${model} (${opts.signal?.aborted ? 'canceled' : 'failed'})`, m.costHint);
          if (opts.signal?.aborted) throw new Stop('canceled');
          return { error: messageOf(e) };
        }
      });
      if ('error' in rendered) return { model, seed, result: null, error: rendered.error, observation: null, measure: null, score: 0 };
      const { result } = rendered;
      const inspected = await inspectSlot(async (): Promise<{ observation: Observation } | { error: string }> => {
        proceed();
        try {
          const r = await analyst.inspect(imageOf(result), names, { ...ctx });
          hold.charge('inspect', `${c.id} ${model}`, r.cost);
          return { observation: r.value };
        } catch (e) {
          hold.charge('inspect', `${c.id} ${model} (failed)`, analyst.costHint);
          if (opts.signal?.aborted) throw new Stop('canceled');
          return { error: `Inspection failed: ${messageOf(e)}` };
        }
      });
      if ('error' in inspected) return { model, seed, result, error: inspected.error, observation: null, measure: null, score: 0 };
      const { observation } = inspected;
      let thumbs: { target: Thumbnail; render: Thumbnail } | null = null;
      if (opts.thumbnail && opts.targetThumbnail) {
        try { thumbs = { target: opts.targetThumbnail, render: await opts.thumbnail(result) }; } catch { thumbs = null; }
      }
      const ms = measure(targetObs!, observation, thumbs);
      return { model, seed, result, error: null, observation, measure: ms, score: ms.fitness };
    };
    try {
      const tasks = models.flatMap(model => seeds.map(seed => task(model, seed).catch(e => { broken = true; throw e; })));
      // Wait for every task, so that nothing keeps running or goes uncharged after a failure.
      const settled = await Promise.allSettled(tasks);
      const failed = settled.find((r): r is PromiseRejectedResult => r.status === 'rejected');
      if (failed) throw failed.reason;
      const renders = settled.map(r => (r as PromiseFulfilledResult<ScoredRender>).value);
      const perModel: Record<string, number> = {};
      for (const model of models) {
        const rs = renders.filter(r => r.model === model);
        perModel[model] = rs.reduce((a, r) => a + r.score, 0) / rs.length;
      }
      const cand: Candidate = { ...c, renders, perModel, fitness: fitnessOf(perModel) };
      candidates.push(cand);
      emit({ type: 'candidate', candidate: cand });
      return cand;
    } finally { hold.release(); spend(); }
  }

  /** Evaluate as many drafts as the budget allows, in parallel. */
  async function evaluateAll(drafts: Draft[], lineage: number, gen: number, ceiling: number): Promise<Candidate[]> {
    const cost = evalCost(opts.models);
    const affordable = drafts.filter((_, i) => budget.canAfford(cost * (i + 1), ceiling));
    if (!affordable.length) throw new Stop('budget');
    return Promise.all(affordable.map((draft, k) => evaluate({ id: `l${lineage}g${gen}c${k + 1}`, lineage, generation: gen, phase: 'evolve', draft, refinements: {} }, opts.models, ceiling)));
  }

  /** The measured feedback of a candidate per model label (first seed with a measurement). */
  const feedbackOf = (c: Candidate, models = opts.models): Record<string, string> => Object.fromEntries(models.map(model => {
    const r = c.renders.find(x => x.model === model && x.measure) ?? c.renders.find(x => x.model === model);
    const info = all.find(m => m.id === model);
    const words = wordsIn(c.draft);
    const cut = info?.shortWindow && words > SHORT_WINDOW_WORDS ? `the description has ${words} words and ${labels[model]} reads only about the first ${SHORT_WINDOW_WORDS}, so the rest was cut; ` : '';
    return [labels[model] ?? model, cut + (r?.measure ? r.measure.feedback : `render failed${r?.error ? ` (${r.error})` : ''}`)];
  }));
  const top = (h: HistoryEntry[]): HistoryEntry[] => [...h].sort((a, b) => b.fitness - a.fitness).slice(0, historySize);
  /** An identical draft renders identically with the fixed seeds, so it is measured once per lineage. */
  const draftKey = (d: Draft): string => [d.prompt, d.style.join(','), d.avoid.join(',')].map(x => x.toLowerCase().replace(/\s+/g, ' ').trim()).join('\u0000');
  /** The drafts not measured yet (keys not in `seen`), without repeats. */
  const fresh = (drafts: Draft[], seen: Set<string>): Draft[] => {
    const keys = new Set(seen);
    return drafts.filter(d => { const k = draftKey(d); if (keys.has(k)) return false; keys.add(k); return true; });
  };
  const usable = (v: Draft[] | string[]): Draft[] => (v as Draft[]).filter(d => d && typeof d.prompt === 'string');

  try {
    // Measure the target once (careful), the yardstick for every render in every lineage.
    emit({ type: 'phase', phase: 'inspect' });
    if (!budget.canAfford(analyst.costHint * 2 + evalCost(opts.models), ceilingA)) throw new Stop('budget', 'The budget does not cover measuring the target and one round of renders.');
    targetObs = (await call(analyst, () => analyst.inspect(opts.target, null, { ...ctx, careful: true }), 'inspect', 'inspect target', ceilingA)).value;
    emit({ type: 'target', observation: targetObs });
    if (!targetObs.subjects.some(x => x.box) && !targetObs.people.length) emit({ type: 'log', message: 'The target inspection found no subjects with boxes and no people; fitness uses the horizon and colours only.' });

    let reachedGoal = false;
    for (let lineage = 1; lineage <= restarts && !reachedGoal; lineage++) {
      const ceiling = ceilingA * lineage / restarts;
      if (!budget.canAfford(optimiser.costHint + evalCost(opts.models), ceiling)) { stopReason = 'budget'; break; }
      emit({ type: 'phase', phase: 'describe', lineage });
      const first = await call(analyst, () => analyst.describe(opts.target, population, ctx), 'analyst', `describe l${lineage}`, ceiling);
      if (!first.value.length) throw new Stop('error', 'The analyst wrote no usable description.');
      emit({ type: 'phase', phase: 'evolve', lineage });
      const history: HistoryEntry[] = [];
      const seen = new Set<string>();
      let notes = first.notes;
      let lineBest: Candidate | null = null;
      let stale = 0;
      /** The drafts that may be rendered; the others join the history unrendered, with the reason, for the optimiser to learn from. */
      const screen = (ds: Draft[]): Draft[] => ds.filter(d => {
        const why = refusal([d.prompt, ...d.style, ...d.avoid].join('; '));
        if (!why) return true;
        history.push({ draft: d, fitness: 0, feedback: Object.fromEntries(opts.models.map(m => [labels[m] ?? m, `not rendered: ${why}`])) });
        seen.add(draftKey(d));
        emit({ type: 'log', message: `Lineage ${lineage}: a description was not rendered: ${why}.` });
        return false;
      });
      let drafts: Draft[] = screen(fresh(first.value, seen));
      if (!drafts.length) {
        const again = await call(optimiser, () => optimiser.propose(opts.target, { ...ctx, targetObservation: targetObs!, history: top(history), notes, n: population, scope: { kind: 'core' } }), 'analyst', `propose l${lineage}g0`, ceiling);
        drafts = screen(fresh(usable(again.value), seen));
        if (!drafts.length) throw new Stop('error', 'The analyst wrote no description that could be rendered.');
      }
      let reason: StopReason = 'generations';
      for (let gen = 0; gen < maxGenerations; gen++) {
        let next: Candidate[];
        try { next = await evaluateAll(drafts, lineage, gen, ceiling); } catch (e) { if (e instanceof Stop && e.reason === 'budget') { reason = 'budget'; break; } throw e; }
        for (const c of next) { history.push({ draft: c.draft, fitness: c.fitness, feedback: feedbackOf(c) }); seen.add(draftKey(c.draft)); }
        const genTop = next.reduce((a, b) => (b.fitness > a.fitness ? b : a));
        const gain = lineBest ? genTop.fitness - lineBest.fitness : Infinity;
        if (!lineBest || genTop.fitness > lineBest.fitness) lineBest = genTop;
        if (!best || lineBest.fitness > best.fitness) best = lineBest;
        stale = gain >= minGain ? 0 : stale + 1;
        generations.push({ lineage, index: gen, phase: 'evolve', candidates: next.map(c => c.id), best: lineBest.id, bestFitness: lineBest.fitness, notes, spent: budget.spent });
        emit({ type: 'generation', generation: generations[generations.length - 1]! });
        if (lineBest.fitness >= goal) { reason = 'goal'; reachedGoal = true; break; }
        if (stale >= plateau) { reason = 'plateau'; break; }
        if (gen + 1 >= maxGenerations) break;
        if (!budget.canAfford(optimiser.costHint + evalCost(opts.models), ceiling)) { reason = 'budget'; break; }
        const reply = await call(optimiser, () => optimiser.propose(opts.target, { ...ctx, targetObservation: targetObs!, history: top(history), notes, n: population, scope: { kind: 'core' } }), 'analyst', `propose l${lineage}g${gen + 1}`, ceiling);
        if (reply.notes) notes = reply.notes;
        const proposed = usable(reply.value);
        if (!proposed.length) { emit({ type: 'log', message: `Lineage ${lineage}: the optimiser wrote no usable description.` }); reason = 'error'; break; }
        drafts = screen(fresh(proposed, seen));
        // Repeats of descriptions already measured, and descriptions that may not be
        // rendered, were dropped: ask once more for as many, when the budget still
        // covers the drafts in hand and one more.
        const repeats = Math.min(population - drafts.length, proposed.length - drafts.length);
        if (repeats > 0 && budget.canAfford(optimiser.costHint + evalCost(opts.models) * (drafts.length + 1), ceiling)) {
          try {
            const more = await call(optimiser, () => optimiser.propose(opts.target, { ...ctx, targetObservation: targetObs!, history: top(history), notes: `${notes}\nSome of your last descriptions repeated ones already measured or were not rendered (see the history); write different ones.`.trim(), n: repeats, scope: { kind: 'core' } }), 'analyst', `propose l${lineage}g${gen + 1} (more)`, ceiling);
            drafts = [...drafts, ...screen(fresh(usable(more.value), new Set([...seen, ...drafts.map(draftKey)])))].slice(0, population);
          } catch (e) {
            // The top-up is optional: a cancel, the time limit or the budget still stop the run; any other failure only leaves the drafts in hand.
            if (e instanceof Stop || e instanceof BudgetExceeded) throw e;
            emit({ type: 'log', message: `Lineage ${lineage}: asking for more descriptions failed (${messageOf(e)}); going on with ${drafts.length}.` });
          }
        }
        if (!drafts.length) { emit({ type: 'log', message: `Lineage ${lineage}: the optimiser wrote nothing new that could be rendered.` }); reason = 'plateau'; break; }
      }
      stopReason = reason;
    }

    // Per-model refinements for the models that lag, by the same measured-feedback mechanism.
    if (best && refine && chosen.length > 1) {
      emit({ type: 'phase', phase: 'refine' });
      const order = [...opts.models].sort((a, b) => best!.perModel[a]! - best!.perModel[b]!);
      const leader = order[order.length - 1];
      for (const model of order) {
        if (model === leader) continue;
        if (!targets[model]) { emit({ type: 'log', message: `Not refining ${labels[model]}: another target model shares its family, so a refinement would reach both.` }); continue; }
        const info = all.find(m => m.id === model)!;
        const words = (best.draft.style.join(' ') + ' ' + best.draft.prompt).split(/\s+/).filter(Boolean).length;
        if (info.shortWindow && words >= SHORT_WINDOW_WORDS) { emit({ type: 'log', message: `Not refining ${labels[model]}: it reads only about ${SHORT_WINDOW_WORDS} words and the core already has ${words}, so an appended refinement would be cut off.` }); continue; }
        const tries: HistoryEntry[] = [{ draft: best.draft, addition: '', fitness: best.perModel[model]!, feedback: feedbackOf(best, [model]) }];
        let notes = '';
        for (let round = 0; round < 2; round++) {
          if (!budget.canAfford(optimiser.costHint + evalCost([model]))) break;
          const reply = await call(optimiser, () => optimiser.propose(opts.target, { ...ctx, targetObservation: targetObs!, history: top(tries), notes, n: refineTries, scope: { kind: 'refine', model: labels[model]!, core: best!.draft } }), 'analyst', `refine ${model}`, opts.budgetUSD);
          if (reply.notes) notes = reply.notes;
          const tried: Candidate[] = [];
          for (const [k, addition] of (reply.value as string[]).entries()) {
            const why = refusal(addition);
            if (why) {
              tries.push({ draft: best.draft, addition, fitness: 0, feedback: { [labels[model]!]: `not rendered: ${why}` } });
              emit({ type: 'log', message: `Refining ${labels[model]}: an addition was not rendered: ${why}.` });
              continue;
            }
            if (!budget.canAfford(evalCost([model]))) break;
            const c = await evaluate({ id: `r${round + 1}-${targets[model]}-${k + 1}`, lineage: best.lineage, generation: round, phase: 'refine', draft: best.draft, refinements: { ...best.refinements, [model]: [addition] } }, [model], opts.budgetUSD);
            tried.push(c);
            tries.push({ draft: best.draft, addition, fitness: c.perModel[model]!, feedback: feedbackOf(c, [model]) });
          }
          const winner = tried.reduce<Candidate | null>((a, b) => (!a || b.perModel[model]! > a.perModel[model]! ? b : a), null);
          const accepted = !!winner && winner.perModel[model]! - best.perModel[model]! >= minGain;
          if (winner && accepted) {
            const perModel: Record<string, number> = { ...best.perModel, [model]: winner.perModel[model]! };
            best = { ...best, id: winner.id, phase: 'refine', refinements: winner.refinements, perModel, fitness: fitnessOf(perModel), renders: [...best.renders.filter(r => r.model !== model), ...winner.renders] };
          }
          generations.push({ lineage: best.lineage, index: round, phase: 'refine', candidates: tried.map(c => c.id), best: best.id, bestFitness: best.fitness, notes: `${labels[model]}: ${notes}${accepted ? '' : ' (no addition kept)'}`, spent: budget.spent });
          emit({ type: 'generation', generation: generations[generations.length - 1]! });
          if (accepted && best.perModel[model]! >= goal) break;
        }
      }
    }
  } catch (e) {
    // Whatever failed once the run was canceled failed because of the cancel.
    if (opts.signal?.aborted) stopReason = 'canceled';
    else if (e instanceof Stop) { stopReason = e.reason; if (e.reason === 'error' || e.message !== e.reason) error = e.message; }
    else if (e instanceof BudgetExceeded) stopReason = 'budget';
    else { stopReason = 'error'; error = messageOf(e); }
    if (!best) {
      const err = new Error(error ?? `Stopped before any candidate was scored (${stopReason})`) as Error & { stopReason?: StopReason; spent?: number };
      err.stopReason = stopReason; err.spent = budget.spent;
      throw err;
    }
    // The run ends with its best so far; say why in the log too, which every app shows as it happens.
    if (error) emit({ type: 'log', message: `Stopped: ${error}` });
  }

  emit({ type: 'phase', phase: 'done' });
  const ranking = [...opts.models].map(model => ({ model, score: best!.perModel[model] ?? 0 })).sort((a, b) => b.score - a.score);
  const document = buildDocument(best!.draft, best!.refinements, targets, ranking.map(r => r.model), aspect);
  return {
    document, text: serialize(document), best: best!, target: targetObs!, candidates, generations, ranking,
    spent: budget.spent, spend: budget.lines, stopReason, error, seconds: (Date.now() - t0) / 1000,
  };
}
