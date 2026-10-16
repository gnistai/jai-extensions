// Hard budget accounting from cost estimates. The engine holds an amount before
// starting provider calls and never starts work it cannot afford; charges then
// draw the hold down, and release() returns what was not used.
import type { SpendLine } from './types.ts';

export class BudgetExceeded extends Error {
  constructor(readonly needed: number, readonly left: number) { super(`Budget exhausted: needs $${needed.toFixed(4)}, $${left.toFixed(4)} left`); }
}

export interface Hold {
  charge(kind: SpendLine['kind'], label: string, usd: number): void;
  release(): void;
}

export class Budget {
  readonly lines: SpendLine[] = [];
  private reserved = 0;
  constructor(readonly limit: number) {
    if (!(limit >= 0) || !Number.isFinite(limit)) throw new RangeError('budget must be a finite number of USD');
  }
  get spent(): number { return this.lines.reduce((a, l) => a + l.usd, 0); }
  /** What can still be held under `ceiling` (default: the limit). */
  left(ceiling = this.limit): number { return Math.max(0, ceiling - this.spent - this.reserved); }
  canAfford(usd: number, ceiling = this.limit): boolean { return this.spent + this.reserved + usd <= ceiling + 1e-12; }
  hold(usd: number, ceiling = this.limit): Hold {
    if (!this.canAfford(usd, ceiling)) throw new BudgetExceeded(usd, this.left(ceiling));
    this.reserved += usd;
    let held = usd;
    return {
      charge: (kind, label, amount) => {
        const fromHold = Math.min(held, amount);
        held -= fromHold;
        this.reserved -= fromHold;
        this.lines.push({ kind, label, usd: amount });
      },
      release: () => { this.reserved -= held; held = 0; },
    };
  }
}
