#!/usr/bin/env node
// Builds dist/jai.mjs (the TypeScript JAI library as an ES module), the
// extension in dist/extension/, and test bundles in dist/test/.
import { build } from 'esbuild';
import { cpSync, mkdirSync, rmSync } from 'node:fs';

rmSync('dist', { recursive: true, force: true });
mkdirSync('dist/extension/icons', { recursive: true });

const common = { bundle: true, target: 'es2022', logLevel: 'warning', legalComments: 'none', external: ['node:*'] };
await build({ ...common, entryPoints: ['src/jai/index.ts'], format: 'esm', platform: 'neutral', outfile: 'dist/jai.mjs' });
await build({ ...common, entryPoints: ['src/renderers/replicate.ts', 'src/renderers/prompt.ts'], format: 'esm', platform: 'neutral', outdir: 'dist/test' });
for (const f of ['src/extension/encode-guard.ts', 'src/providers/replicate-client.ts']) await build({ ...common, entryPoints: [f], format: 'esm', platform: 'neutral', outfile: `dist/test/${f.split('/').pop().replace('.ts', '.js')}` });
await build({ ...common, entryPoints: ['src/encoder/index.ts'], format: 'esm', platform: 'neutral', outfile: 'dist/encoder.mjs' });
await build({ ...common, entryPoints: ['src/extension/background.ts'], format: 'esm', platform: 'browser', outfile: 'dist/extension/background.js' });
await build({ ...common, entryPoints: ['src/extension/content.ts'], format: 'iife', platform: 'browser', outfile: 'dist/extension/content.js' });
await build({ ...common, entryPoints: ['src/extension/popup.ts', 'src/extension/options.ts', 'src/extension/encode.ts'], format: 'esm', platform: 'browser', outdir: 'dist/extension' });
for (const f of ['manifest.json', 'popup.html', 'options.html', 'encode.html', 'ui.css']) cpSync(`src/extension/${f}`, `dist/extension/${f}`);
for (const s of [16, 32, 48, 128]) cpSync(`assets/icon${s}.png`, `dist/extension/icons/icon${s}.png`);
console.log('built dist/jai.mjs, dist/encoder.mjs and dist/extension/');
