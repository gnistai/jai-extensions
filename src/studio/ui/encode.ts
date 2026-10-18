// Encode tab (studio/DESIGN.md section 11): choose an image, set the encoder
// options (the CLI's defaults), then watch the run: the target with its
// measured subjects, people and horizon, fitness per generation, the best candidate's
// renders with their measured feedback, and every candidate. At the end: the
// document, Save .jai, Open in viewer and Reveal report folder.
import { encode as encodeFile, parse } from '../../jai/index.ts';
import { personLabel, traitsInWords } from '../../encoder/people.ts';
import {
  api, events, type Box, type CandidateRef, type EncodeDone, type EncodeEvent, type EncodeEventMessage, type EncodeSettings,
  type Generation, type ImagePreview, type Observation, type RenderRef,
} from './api.ts';
import { FitnessChart } from './chart.ts';
import { $, baseName, bytesToBase64, copyText, el, errorText, fmtClock, fmtFitness, say, stem, svg, toast } from './dom.ts';
import type { TabContext } from './main.ts';
import { ENCODE_LIMITS, checkEncodeSettings, stopSentence, type SettingsField } from './rules.ts';

export interface EncodeTab {
  useImage(path: string): void;
  openDialog(): void;
}

const PHASES: Record<string, string> = { inspect: 'Measuring the image', describe: 'Writing first descriptions', evolve: 'Rendering and improving', refine: 'Refining', done: 'Finishing' };
const SUBJECT_SLOTS = 5;

/** A subject (numbered 1…) or a person (P1…), drawn dashed in one colour. */
interface Mark { n: number; box: Box; label?: string; person?: boolean }
interface Job { id: string; dir: string; settings: EncodeSettings; started: number }

const clampBox = (b: Box): Box => ({ ymin: Math.max(0, b.ymin), xmin: Math.max(0, b.xmin), ymax: Math.min(1, b.ymax), xmax: Math.min(1, b.xmax) });
const pct = (f: number): string => `${Math.round(f * 100)}%`;
const slot = (n: number): number => ((n - 1) % SUBJECT_SLOTS) + 1;

/** Boxes and horizon drawn over an image; the SVG spans the image box, labels are HTML so they keep their size. */
function overlay(img: HTMLImageElement, marks: Mark[], horizon: number | null, labelled: boolean): HTMLElement {
  const box = el('div', { className: 'ov' }, img);
  const layer = svg('svg', { class: 'ov-svg', viewBox: '0 0 1000 1000', preserveAspectRatio: 'none', 'aria-hidden': 'true', focusable: 'false' });
  const tags = el('div', { className: 'ov-tags' });
  tags.setAttribute('aria-hidden', 'true');
  const shape = (tag: 'rect' | 'line', a: Record<string, number>, cls: string): void => {
    layer.append(svg(tag, { ...a, class: 'ov-halo', 'vector-effect': 'non-scaling-stroke' }), svg(tag, { ...a, class: cls, 'vector-effect': 'non-scaling-stroke' }));
  };
  if (horizon !== null) {
    shape('line', { x1: 0, x2: 1000, y1: horizon * 1000, y2: horizon * 1000 }, 'ov-horizon');
    if (labelled) {
      const t = el('span', { className: 'ov-tag ov-tag-horizon' }, 'horizon');
      t.style.top = pct(horizon);
      tags.append(t);
    }
  }
  for (const m of marks) {
    const b = m.box, colour = m.person ? 'sp' : `s${slot(m.n)}`, id = m.person ? `P${m.n}` : String(m.n);
    shape('rect', { x: b.xmin * 1000, y: b.ymin * 1000, width: Math.max(1, (b.xmax - b.xmin) * 1000), height: Math.max(1, (b.ymax - b.ymin) * 1000) }, `ov-box ${m.person ? 'ov-person ' : ''}${colour}`);
    if (!labelled) continue;
    const t = el('span', { className: `ov-tag ${colour}` }, m.label ? `${id} ${m.label}` : id);
    if (b.xmin > 0.7) t.style.right = pct(1 - b.xmax); else t.style.left = pct(b.xmin);
    t.style.top = pct(b.ymin);
    if (b.ymin > 0.08) t.classList.add('above');
    tags.append(t);
  }
  box.append(layer, tags);
  return box;
}

export function initEncode(ctx: TabContext): EncodeTab {
  const form = $<HTMLFormElement>('enc-form'), fields = $<HTMLFieldSetElement>('enc-fields'), msg = $('enc-msg');
  const start = $<HTMLButtonElement>('enc-start'), cancel = $<HTMLButtonElement>('enc-cancel'), modelsLink = $<HTMLButtonElement>('enc-models');
  const summary = $('enc-summary'), edit = $<HTMLButtonElement>('enc-edit');
  const drop = $('enc-drop'), figure = $('enc-figure'), stage = $('enc-stage');
  const input: Record<SettingsField, HTMLInputElement> = {
    generations: $<HTMLInputElement>('enc-generations'), population: $<HTMLInputElement>('enc-population'), seeds: $<HTMLInputElement>('enc-seeds'),
    restarts: $<HTMLInputElement>('enc-restarts'), goal: $<HTMLInputElement>('enc-goal'), scale: $<HTMLInputElement>('enc-scale'),
  };
  const thinkingInput = $<HTMLSelectElement>('enc-thinking');
  // The limits the app checks, so the fields offer no value encode_start refuses.
  for (const k of ['generations', 'population', 'restarts', 'scale'] as const) { input[k].min = String(ENCODE_LIMITS[k][0]); input[k].max = String(ENCODE_LIMITS[k][1]); }
  const chart = new FitnessChart($('enc-chart'));
  const cands = new Map<string, CandidateRef>();
  const gens: Generation[] = [];
  let image: { path: string; name: string; preview: ImagePreview } | null = null;
  let job: Job | null = null;
  let starting = false, canceling = false, editing = false;
  let lastSettings: EncodeSettings | null = null;
  /** Events that arrived before encode_start answered with the job id. */
  const early: Array<EncodeEventMessage | EncodeDone> = [];
  let target: Observation | null = null;
  let best: CandidateRef | null = null;
  let result: EncodeDone | null = null;
  let phase = '', lineage = 1, timer = 0;

  // ---------------------------------------------------------------- settings

  function readSettings(): EncodeSettings | null {
    for (const i of Object.values(input)) i.removeAttribute('aria-invalid');
    const values = Object.fromEntries(Object.entries(input).map(([k, i]) => [k, i.value])) as Record<SettingsField, string>;
    const r = checkEncodeSettings({ ...values, thinking: thinkingInput.value });
    if (r.ok) return r.settings;
    const i = input[r.field];
    i.setAttribute('aria-invalid', 'true');
    say(msg, r.message, 'err');
    // A field under "More settings" may be folded away.
    const more = i.closest('details');
    if (more) more.open = true;
    i.focus();
    return null;
  }

  function describe(s: EncodeSettings): string {
    const n = (k: number, one: string, many: string): string => `${k} ${k === 1 ? one : many}`;
    return [n(s.generations, 'generation', 'generations'), `${n(s.population, 'candidate', 'candidates')} each`, `${s.seeds.length === 1 ? 'seed' : 'seeds'} ${s.seeds.join(', ')}`,
      s.restarts > 1 ? `${s.restarts} attempts` : '', `goal ${s.goal.toFixed(2)}`, s.scale !== 1 ? `render scale ${s.scale}` : '', s.thinking !== 'off' ? `thinking: ${s.thinking}` : ''].filter(Boolean).join(' · ');
  }

  /** While a run goes and after it, the settings fold into one line, so the progress stays in view. */
  function refreshControls(): void {
    const running = !!job || starting;
    const folded = !!lastSettings && (running || (result !== null && !editing));
    fields.disabled = running;
    fields.hidden = folded;
    summary.hidden = !folded;
    if (lastSettings) summary.textContent = describe(lastSettings);
    start.hidden = running;
    start.disabled = running || !image;
    start.textContent = result ? 'Start again' : 'Start encoding';
    cancel.disabled = !job || canceling;
    cancel.hidden = !running;
    edit.hidden = !folded || running;
    $<HTMLButtonElement>('enc-open').disabled = running;
    stage.classList.toggle('locked', running);
  }

  // ---------------------------------------------------------------- the image

  async function useImage(path: string): Promise<void> {
    if (job || starting) { toast('An encode is running. Cancel it before you choose another image.', 'err'); return; }
    say(msg, 'Opening the image…');
    try {
      const preview = await api.imagePreview(path);
      image = { path, name: baseName(path), preview };
      resetRun();
      drawTarget();
      drop.hidden = true;
      figure.hidden = false;
      $('enc-caption').textContent = `${image.name} · ${preview.width} × ${preview.height}`;
      say(msg, '');
    } catch (e) {
      say(msg, `Could not open that image: ${errorText(e)}`, 'err');
    }
    refreshControls();
  }

  async function openDialog(): Promise<void> {
    if (job || starting) return;
    try {
      const r = await api.openDialog('image');
      if (r) void useImage(r.path);
    } catch (e) {
      toast(`Could not open the file dialog: ${errorText(e)}`, 'err');
    }
  }

  /** The target preview with the measured subjects, people and horizon once the target event arrived. */
  function drawTarget(): void {
    const holder = $('enc-target');
    if (!image) { holder.replaceChildren(); return; }
    const img = el('img', { src: image.preview.dataUrl, width: image.preview.width, height: image.preview.height, alt: `The image to encode: ${image.name}` });
    const marks: Mark[] = [];
    target?.subjects.forEach((s, i) => { if (s.box) marks.push({ n: i + 1, box: s.box, label: s.name }); });
    target?.people.forEach((p, i) => marks.push({ n: i + 1, box: p.box, label: personLabel(p).replace(/ at \w+$/, ''), person: true }));
    holder.replaceChildren(overlay(img, marks, target?.horizon ?? null, true));
    const list = $('enc-subjects');
    if (!target) { list.replaceChildren(); list.hidden = true; return; }
    const items = target.subjects.map((s, i) => el('li', {}, el('i', { className: `swatch s${slot(i + 1)}` }), el('b', {}, `${i + 1} ${s.name}`),
      el('span', { className: 'muted' }, s.box ? ` x ${pct(s.box.xmin)}–${pct(s.box.xmax)}, y ${pct(s.box.ymin)}–${pct(s.box.ymax)}` : ' (no box)')));
    items.push(...target.people.map((p, i) => el('li', {}, el('i', { className: 'swatch person sp' }), el('b', {}, `P${i + 1} ${personLabel(p)}`),
      el('span', { className: 'muted' }, ` ${pct(p.box.ymax - p.box.ymin)} of the frame tall${p.face ? `, face ${p.face}` : ''}${traitsInWords(p) ? `: ${traitsInWords(p)}` : ''}`))));
    if (target.crowd && target.crowd !== 'none') items.push(el('li', { className: 'muted measured' }, `Other people: ${target.crowd}`));
    if (target.horizon !== null) items.push(el('li', {}, el('i', { className: 'swatch horizon' }), el('b', {}, 'horizon'), el('span', { className: 'muted' }, ` ${pct(target.horizon)} from the top${target.horizonShape ? `, ${target.horizonShape}` : ''}`)));
    const looks = [target.camera, target.lighting, target.medium].filter(Boolean).join(' · ');
    if (looks) items.push(el('li', { className: 'muted measured' }, looks));
    list.replaceChildren(...items);
    list.hidden = false;
  }

  /** A render's measured boxes: subjects rebuilt from the target boxes and the measured offsets, people as found. */
  function renderMarks(r: RenderRef): { marks: Mark[]; horizon: number | null } {
    const m = r.measure;
    if (!target || !m) return { marks: [], horizon: null };
    const marks: Mark[] = [];
    // measure() reports the target's boxed subjects in order, so they pair by position (names may repeat).
    const boxed = target.subjects.flatMap((s, i) => (s.box ? [{ n: i + 1, box: s.box }] : []));
    for (const [k, sm] of m.subjects.entries()) {
      const n = boxed[k]?.n ?? 0, tb = boxed[k]?.box;
      if (!n || !tb || !sm.present) continue;
      const w = Math.max(1e-3, tb.xmax - tb.xmin) * Math.exp(sm.sw), h = Math.max(1e-3, tb.ymax - tb.ymin) * Math.exp(sm.sh);
      const cx = (tb.xmin + tb.xmax) / 2 + sm.dx, cy = (tb.ymin + tb.ymax) / 2 + sm.dy;
      marks.push({ n, box: clampBox({ xmin: cx - w / 2, xmax: cx + w / 2, ymin: cy - h / 2, ymax: cy + h / 2 }) });
    }
    for (const p of m.people) if (p.box) marks.push({ n: p.n, box: clampBox(p.box), person: true });
    return { marks, horizon: target.horizon !== null && m.horizon !== null ? target.horizon + m.horizon : null };
  }

  function renderFigure(c: CandidateRef, r: RenderRef, withBoxes: boolean): HTMLElement {
    const caption = el('figcaption', {}, `Seed ${r.seed} · `, el('b', {}, fmtFitness(r.score)));
    if (!r.thumb) return el('figure', { className: 'shot failed' }, el('div', { className: 'shot-missing' }, r.error ? 'Render failed' : 'No preview'), caption);
    const img = el('img', { src: r.thumb, alt: `Render of candidate ${c.id}, seed ${r.seed}` });
    const { marks, horizon } = withBoxes ? renderMarks(r) : { marks: [], horizon: null };
    return el('figure', { className: 'shot' }, withBoxes ? overlay(img, marks, horizon, false) : img, caption);
  }

  function feedbackList(c: CandidateRef): HTMLElement {
    return el('dl', { className: 'feedback' }, ...c.renders.flatMap(r => [el('dt', {}, `Seed ${r.seed}`), el('dd', {}, r.measure?.feedback ?? r.error ?? 'Not measured')]));
  }

  function draftBlock(c: CandidateRef): HTMLElement[] {
    const extra = [c.draft.style.length ? `Style: ${c.draft.style.join(', ')}` : '', c.draft.avoid.length ? `Avoid: ${c.draft.avoid.join(', ')}` : ''].filter(Boolean).join(' · ');
    return [el('p', { className: 'draft' }, c.draft.prompt), extra ? el('p', { className: 'muted small' }, extra) : el('span')];
  }

  // ---------------------------------------------------------------- run views

  function resetRun(): void {
    cands.clear();
    gens.length = 0;
    target = null;
    best = null;
    result = null;
    phase = '';
    lineage = 1;
    $('enc-cands').replaceChildren();
    $('enc-log').replaceChildren();
    $('enc-count').textContent = '';
    for (const id of ['enc-result', 'enc-best-card', 'enc-cands-card', 'enc-progress-card']) $(id).hidden = true;
    modelsLink.hidden = true;
  }

  function log(message: string): void {
    const box = $('enc-log');
    const atEnd = box.scrollTop + box.clientHeight >= box.scrollHeight - 4;
    box.append(el('li', {}, message));
    if (atEnd) box.scrollTop = box.scrollHeight;
  }

  function updatePhase(): void {
    if (!job) return;
    const g = gens.filter(x => x.lineage === lineage).length;
    const parts = [canceling ? 'Canceling' : PHASES[phase] ?? 'Starting'];
    if (job.settings.restarts > 1) parts.push(`attempt ${lineage} of ${job.settings.restarts}`);
    if (phase === 'evolve') parts.push(`generation ${Math.min(job.settings.generations, g + 1)} of ${job.settings.generations}`);
    $('enc-phase').textContent = parts.join(' · ');
    $('enc-clock').textContent = fmtClock(Date.now() - job.started);
  }

  function showBest(c: CandidateRef): void {
    best = c;
    $('enc-best-card').hidden = false;
    $('enc-best-id').textContent = c.id;
    $('enc-best-fit').textContent = fmtFitness(c.fitness);
    $('enc-best-shots').replaceChildren(...c.renders.map(r => renderFigure(c, r, true)));
    $('enc-best-feedback').replaceChildren(feedbackList(c));
    $('enc-best-draft').replaceChildren(...draftBlock(c));
    for (const item of $('enc-cands').querySelectorAll<HTMLElement>('details.cand')) {
      const badge = item.querySelector<HTMLElement>('.badge');
      if (badge) badge.hidden = item.dataset.id !== c.id;
    }
  }

  function groupFor(c: CandidateRef): HTMLElement {
    const key = `${c.lineage}-${c.phase}-${c.generation}`;
    const list = $('enc-cands');
    const found = list.querySelector<HTMLElement>(`section[data-key="${key}"]`);
    if (found) return found;
    const name = c.phase === 'refine' ? `Refinement ${c.generation + 1}` : `Generation ${c.generation + 1}`;
    const head = el('h3', {}, (job?.settings.restarts ?? 1) > 1 ? `Attempt ${c.lineage} · ${name}` : name, el('span', { className: 'gen-best muted' }));
    const section = el('section', { className: 'gen-group' }, head, el('div', { className: 'gen-items' }));
    section.dataset.key = key;
    list.append(section);
    return section;
  }

  function addCandidate(c: CandidateRef): void {
    cands.set(c.id, c);
    const list = $('enc-cands');
    const atEnd = list.scrollTop + list.clientHeight >= list.scrollHeight - 8;
    const first = c.renders.find(r => r.thumb);
    const item = el('details', { className: 'cand' },
      el('summary', {},
        first?.thumb ? el('img', { className: 'cand-thumb', src: first.thumb, alt: '' }) : el('span', { className: 'cand-thumb none' }),
        el('span', { className: 'cand-head' }, el('b', {}, c.id), el('span', { className: 'cand-fit' }, fmtFitness(c.fitness)), el('span', { className: 'badge', hidden: true }, 'best')),
        el('span', { className: 'cand-prompt' }, c.draft.prompt)),
      el('div', { className: 'cand-body' }, el('div', { className: 'shots' }, ...c.renders.map(r => renderFigure(c, r, false))), feedbackList(c), ...draftBlock(c)));
    item.dataset.id = c.id;
    groupFor(c).querySelector('.gen-items')?.append(item);
    if (atEnd) list.scrollTop = list.scrollHeight;
    $('enc-cands-card').hidden = false;
    $('enc-count').textContent = `${cands.size}`;
    if (!best || c.fitness > best.fitness) showBest(c);
    chart.update(gens, [...cands.values()]);
  }

  function addGeneration(g: Generation): void {
    gens.push(g);
    const section = $('enc-cands').querySelector(`section[data-key="${g.lineage}-${g.phase}-${g.index}"]`);
    const label = section?.querySelector('.gen-best');
    if (label) label.textContent = ` · best so far ${fmtFitness(g.bestFitness)}`;
    chart.update(gens, [...cands.values()]);
    updatePhase();
  }

  function onEvent(e: EncodeEvent): void {
    switch (e.type) {
      case 'phase':
        phase = e.phase;
        lineage = e.lineage ?? lineage;
        if (e.phase !== 'done') log(`${PHASES[e.phase] ?? e.phase}${e.lineage && (job?.settings.restarts ?? 1) > 1 ? ` (attempt ${e.lineage})` : ''}…`);
        updatePhase();
        break;
      case 'target': {
        target = e.observation;
        drawTarget();
        const named = target.subjects.filter(s => s.box).map(s => s.name), n = target.people.length;
        log(`Measured the image: ${named.join(', ') || 'no distinct subjects'}${n ? `, ${n} ${n === 1 ? 'person' : 'people'}` : ''}${target.horizon !== null ? `, horizon at ${pct(target.horizon)}` : ''}.`);
        break;
      }
      case 'candidate': addCandidate(e.candidate); break;
      case 'generation': addGeneration(e.generation); break;
      case 'log': log(e.message); break;
      case 'spend': break; // local renders cost nothing
    }
  }

  function finish(d: EncodeDone): void {
    result = d;
    clearInterval(timer);
    const seconds = job ? Math.round((Date.now() - job.started) / 1000) : 0;
    job = null;
    canceling = false;
    $('enc-phase').textContent = d.stopReason === 'canceled' ? 'Canceled' : d.ok && d.stopReason !== 'error' ? 'Done' : 'Stopped';
    const ok = d.ok && !!d.text;
    $('enc-result-title').textContent = ok ? 'Result' : d.stopReason === 'canceled' ? 'Encoding canceled' : 'Encoding failed';
    // A run with a result can still have stopped on an error: say which.
    const why = stopSentence(d.stopReason, ok ? d.error : null);
    const note = ok ? `${why} Best fitness ${best ? fmtFitness(best.fitness) : '–'} in ${fmtClock(seconds * 1000)}.` : d.error ?? why;
    say($('enc-result-note'), note.trim(), ok && d.stopReason !== 'error' ? '' : ok ? 'warn' : 'err');
    $('enc-result-text').textContent = d.text ?? '';
    $('enc-result-text').hidden = !ok;
    for (const id of ['enc-save', 'enc-copy']) $(id).hidden = !ok;
    $('enc-view').hidden = !ok || !d.jaiPath;
    $('enc-reveal').hidden = !d.reportDir;
    modelsLink.hidden = ok || !/download the models/i.test(d.error ?? '');
    $('enc-result').hidden = false;
    $('enc-result-title').focus();
    refreshControls();
    ctx.refreshStatus();
  }

  // ---------------------------------------------------------------- start, cancel, result actions

  void events.encodeEvent(m => {
    if (!job && starting) early.push(m);
    else if (job && m.jobId === job.id) onEvent(m.event);
  });
  void events.encodeDone(d => {
    if (!job && starting) early.push(d);
    else if (job && d.jobId === job.id) finish(d);
  });

  form.addEventListener('submit', async e => {
    e.preventDefault();
    if (!image || job || starting) return;
    const settings = readSettings();
    if (!settings) return;
    resetRun();
    drawTarget();
    say(msg, '');
    editing = false;
    lastSettings = settings;
    starting = true;
    refreshControls();
    try {
      const started = await api.encodeStart(image.path, settings);
      job = { id: started.jobId, dir: started.dir, settings, started: Date.now() };
      $('enc-progress-card').hidden = false;
      chart.reset({ goal: settings.goal, generations: settings.generations, attempts: settings.restarts });
      timer = window.setInterval(updatePhase, 1000);
      updatePhase();
      for (const m of early.splice(0)) if (m.jobId === started.jobId) { if ('event' in m) onEvent(m.event); else finish(m); }
    } catch (err) {
      const message = errorText(err);
      say(msg, `Could not start: ${message}`, 'err');
      modelsLink.hidden = !/download the models/i.test(message);
    } finally {
      starting = false;
      early.length = 0;
      refreshControls();
    }
  });

  cancel.addEventListener('click', async () => {
    if (!job) return;
    canceling = true;
    refreshControls();
    updatePhase();
    try {
      await api.encodeCancel(job.id);
    } catch (e) {
      canceling = false;
      refreshControls();
      toast(`Could not cancel: ${errorText(e)}`, 'err');
    }
  });

  $('enc-save').addEventListener('click', async () => {
    if (!result?.text) return;
    try {
      // The file form (spec D3.4): one gzip member of canonical JSON, made by the JAI library from the canonical text.
      const bytes = await encodeFile(parse(result.text).document);
      const r = await api.saveFile({ bytesB64: bytesToBase64(bytes), suggestedName: `${stem(image?.name ?? 'image')}.jai` });
      if (r) toast(`Saved ${baseName(r.path)}`);
    } catch (e) {
      toast(`Could not save the document: ${errorText(e)}`, 'err');
    }
  });
  $('enc-copy').addEventListener('click', async () => {
    if (!result?.text) return;
    try { await copyText(result.text); toast('Copied the document text'); } catch (e) { toast(errorText(e), 'err'); }
  });
  $('enc-view').addEventListener('click', () => { if (result?.jaiPath) ctx.openDocument(result.jaiPath); });
  $('enc-reveal').addEventListener('click', async () => {
    if (!result?.reportDir) return;
    try { await api.reveal(result.reportDir); } catch (e) { toast(`Could not open the folder: ${errorText(e)}`, 'err'); }
  });
  $('enc-open').addEventListener('click', () => void openDialog());
  edit.addEventListener('click', () => { editing = true; refreshControls(); input.generations.focus(); });
  modelsLink.addEventListener('click', () => ctx.show('models', true));
  refreshControls();

  return { useImage: path => void useImage(path), openDialog: () => void openDialog() };
}
