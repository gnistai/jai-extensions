#!/usr/bin/env node
// Runs the JAI specification's conformance fixtures against dist/jai.mjs.
// The fixtures are owned by the spec repository (gnistai/jai) and are
// authoritative. Looks for it in $JAI_SPEC_DIR, ../jai, or clones it.
import { existsSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { resolve } from 'node:path';

let dir = process.env.JAI_SPEC_DIR || resolve('..', 'jai');
if (!existsSync(resolve(dir, 'tests', 'run.mjs'))) {
  dir = resolve('.cache', 'jai');
  if (!existsSync(dir)) execFileSync('git', ['clone', '--depth', '1', 'https://github.com/gnistai/jai.git', dir], { stdio: 'inherit' });
}
execFileSync(process.execPath, [resolve(dir, 'tests', 'run.mjs'), '--impl', resolve('dist', 'jai.mjs')], { stdio: 'inherit' });
