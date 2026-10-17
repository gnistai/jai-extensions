// Fitness per generation for the Encode tab: an inline SVG line chart of the
// best fitness so far in each attempt, every candidate as a faint dot, and the
// goal as a dashed threshold. A crosshair readout follows the pointer or the
// arrow keys (it sits low in the plot, where fitness rarely goes), and a
// visually hidden table carries the same numbers.
import type { CandidateRef, Generation } from './api.ts';
import { el, fmtFitness, svg } from './dom.ts';

const HEIGHT = 176;
const PAD = { left: 34, right: 48, top: 14, bottom: 26 };

export interface ChartOptions { goal: number; generations: number; attempts: number }

export class FitnessChart {
  private opts: ChartOptions = { goal: 0.9, generations: 8, attempts: 1 };
  private gens: Generation[] = [];
  private cands: CandidateRef[] = [];
  /** Generation index under the crosshair. */
  private hover: number | null = null;
  private readonly frame: HTMLElement;
  private readonly plot: SVGSVGElement;
  private readonly tip: HTMLElement;
  private readonly legend: HTMLElement;
  private readonly table: HTMLTableElement;

  constructor(host: HTMLElement) {
    this.legend = el('div', { className: 'chart-legend' });
    this.plot = svg('svg', { class: 'chart-svg', height: HEIGHT, 'aria-hidden': 'true', focusable: 'false' });
    this.tip = el('div', { className: 'chart-tip', hidden: true });
    this.tip.setAttribute('aria-live', 'polite');
    this.frame = el('div', { className: 'chart-frame', tabIndex: 0 }, this.plot, this.tip);
    this.frame.setAttribute('role', 'group');
    this.frame.setAttribute('aria-label', 'Fitness per generation. Use the left and right arrow keys to read each generation.');
    this.table = el('table');
    // A table ignores width and overflow, so the visually hidden box is a wrapper around it.
    host.replaceChildren(this.legend, this.frame, el('div', { className: 'sr-only' }, this.table));
    new ResizeObserver(() => this.draw()).observe(this.frame);
    this.frame.addEventListener('pointermove', e => {
      const r = this.frame.getBoundingClientRect();
      this.setHover(this.indexAt(e.clientX - r.left, r.width));
    });
    this.frame.addEventListener('pointerleave', () => { if (document.activeElement !== this.frame) this.setHover(null); });
    this.frame.addEventListener('blur', () => this.setHover(null));
    this.frame.addEventListener('focus', () => { if (this.hover === null) this.setHover(this.lastIndex()); });
    this.frame.addEventListener('keydown', e => {
      const last = this.opts.generations - 1, at = this.hover ?? this.lastIndex();
      const next: Record<string, number | null> = { ArrowLeft: Math.max(0, at - 1), ArrowRight: Math.min(last, at + 1), Home: 0, End: last, Escape: null };
      if (!(e.key in next)) return;
      e.preventDefault();
      this.setHover(next[e.key] ?? null);
    });
  }

  reset(opts: ChartOptions): void {
    this.opts = { ...opts, generations: Math.max(1, opts.generations) };
    this.gens = [];
    this.cands = [];
    this.hover = null;
    this.draw();
  }

  update(gens: Generation[], cands: CandidateRef[]): void {
    this.gens = gens.filter(g => g.phase === 'evolve');
    this.cands = cands.filter(c => c.phase === 'evolve');
    this.draw();
  }

  private lastIndex(): number {
    return this.gens.reduce((a, g) => Math.max(a, g.index), 0);
  }

  private setHover(i: number | null): void {
    if (i === this.hover) return;
    this.hover = i;
    this.draw();
  }

  private indexAt(x: number, width: number): number {
    const n = this.opts.generations;
    if (n <= 1) return 0;
    const t = (x - PAD.left) / Math.max(1, width - PAD.left - PAD.right);
    return Math.min(n - 1, Math.max(0, Math.round(t * (n - 1))));
  }

  private draw(): void {
    const width = this.frame.clientWidth;
    this.drawTable();
    this.drawLegend();
    if (width <= 0) return;
    const { goal, generations: n } = this.opts;
    const iw = width - PAD.left - PAD.right, ih = HEIGHT - PAD.top - PAD.bottom;
    const x = (i: number): number => PAD.left + (n <= 1 ? iw / 2 : (i / (n - 1)) * iw);
    const y = (f: number): number => PAD.top + (1 - Math.min(1, Math.max(0, f))) * ih;
    const p = this.plot;
    p.setAttribute('width', String(width));
    p.setAttribute('viewBox', `0 0 ${width} ${HEIGHT}`);
    p.replaceChildren();
    for (const f of [0, 0.25, 0.5, 0.75, 1]) {
      p.append(svg('line', { class: f === 0 ? 'chart-base' : 'chart-grid', x1: PAD.left, x2: width - PAD.right, y1: y(f), y2: y(f) }));
      if (f === 0 || f === 0.5 || f === 1) p.append(svg('text', { class: 'chart-tick', x: PAD.left - 6, y: y(f) + 4, 'text-anchor': 'end' }, f === 0.5 ? '0.5' : String(f)));
    }
    const step = n <= 12 ? 1 : Math.ceil(n / 10);
    for (let i = 0; i < n; i += step) p.append(svg('text', { class: 'chart-tick', x: x(i), y: HEIGHT - 8, 'text-anchor': 'middle' }, String(i + 1)));
    p.append(svg('line', { class: 'chart-goal', x1: PAD.left, x2: width - PAD.right, y1: y(goal), y2: y(goal) }));
    p.append(svg('text', { class: 'chart-goal-text', x: PAD.left + 4, y: goal > 0.92 ? y(goal) + 13 : y(goal) - 5 }, `goal ${goal.toFixed(2)}`));
    // Candidates: faint dots, spread a little around their generation.
    for (const c of this.cands) {
      const peers = this.cands.filter(o => o.lineage === c.lineage && o.generation === c.generation);
      const k = peers.indexOf(c), spread = Math.min(10, iw / Math.max(1, n) / 3);
      const dx = peers.length > 1 ? (k / (peers.length - 1) - 0.5) * 2 * spread : 0;
      p.append(svg('circle', { class: `chart-dot l${this.slot(c.lineage)}`, cx: x(c.generation) + dx, cy: y(c.fitness), r: 3.5 }));
    }
    if (this.hover !== null) p.append(svg('line', { class: 'chart-cross', x1: x(this.hover), x2: x(this.hover), y1: PAD.top, y2: PAD.top + ih }));
    // Best so far per attempt: a 2 px line, an end dot with a surface ring, and the value at the end.
    const placed: number[] = [];
    for (const lineage of this.lineages()) {
      const pts = this.gens.filter(g => g.lineage === lineage).sort((a, b) => a.index - b.index);
      const last = pts[pts.length - 1];
      if (!last) continue;
      const cls = `l${this.slot(lineage)}`;
      p.append(svg('path', { class: `chart-line ${cls}`, d: pts.map((g, i) => `${i ? 'L' : 'M'}${x(g.index).toFixed(1)},${y(g.bestFitness).toFixed(1)}`).join('') }));
      for (const g of pts) if (g.index === this.hover) p.append(svg('circle', { class: `chart-end ${cls}`, cx: x(g.index), cy: y(g.bestFitness), r: 4 }));
      p.append(svg('circle', { class: `chart-end ${cls}`, cx: x(last.index), cy: y(last.bestFitness), r: 4.5 }));
      const ly = y(last.bestFitness) + 4;
      if (placed.every(v => Math.abs(v - ly) >= 13)) {
        placed.push(ly);
        p.append(svg('text', { class: 'chart-value', x: x(last.index) + 9, y: ly }, fmtFitness(last.bestFitness)));
      }
    }
    this.drawTip(x);
  }

  private lineages(): number[] {
    return [...new Set(this.gens.map(g => g.lineage))].sort((a, b) => a - b);
  }

  /** Colour slot 1 to 3 (validated palette); attempts beyond three reuse the last slot. */
  private slot(lineage: number): number {
    return Math.min(3, Math.max(1, lineage));
  }

  private drawTip(x: (i: number) => number): void {
    const i = this.hover;
    if (i === null) { this.tip.hidden = true; return; }
    const rows: HTMLElement[] = [];
    for (const lineage of this.lineages()) {
      const g = this.gens.find(o => o.lineage === lineage && o.index === i);
      if (!g) continue;
      const cands = this.cands.filter(c => c.lineage === lineage && c.generation === i);
      rows.push(el('div', { className: 'tip-row' },
        el('i', { className: `tip-key l${this.slot(lineage)}` }), el('b', {}, fmtFitness(g.bestFitness)),
        el('span', {}, ` best so far${this.opts.attempts > 1 ? `, attempt ${lineage}` : ''} · ${cands.length} candidate${cands.length === 1 ? '' : 's'}`)));
    }
    if (!rows.length) rows.push(el('div', { className: 'tip-row muted' }, 'Not reached yet'));
    this.tip.replaceChildren(el('div', { className: 'tip-head' }, `Generation ${i + 1} of ${this.opts.generations}`), ...rows);
    this.tip.hidden = false;
    const w = this.frame.clientWidth, tw = this.tip.offsetWidth || 180;
    this.tip.style.left = `${Math.min(w - tw - 4, Math.max(4, x(i) - tw / 2))}px`;
  }

  private drawLegend(): void {
    const items: HTMLElement[] = [];
    const lineages = this.opts.attempts > 1 ? Array.from({ length: this.opts.attempts }, (_, k) => k + 1) : [1];
    for (const l of lineages) items.push(el('span', { className: 'legend-item' }, el('i', { className: `legend-line l${this.slot(l)}` }), this.opts.attempts > 1 ? `Attempt ${l}` : 'Best so far'));
    items.push(el('span', { className: 'legend-item' }, el('i', { className: 'legend-dot' }), 'Candidates'));
    items.push(el('span', { className: 'legend-item' }, el('i', { className: 'legend-goal' }), 'Goal'));
    this.legend.replaceChildren(...items);
  }

  private drawTable(): void {
    const head = el('tr', {}, ...['Attempt', 'Generation', 'Best so far', 'Best candidate', 'Candidates'].map(h => el('th', { scope: 'col' }, h)));
    const rows = this.gens.map(g => el('tr', {}, el('td', {}, String(g.lineage)), el('td', {}, String(g.index + 1)), el('td', {}, fmtFitness(g.bestFitness)), el('td', {}, g.best), el('td', {}, String(g.candidates.length))));
    this.table.replaceChildren(el('caption', {}, 'Best fitness per generation'), el('thead', {}, head), el('tbody', {}, ...rows));
  }
}
