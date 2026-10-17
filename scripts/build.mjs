#!/usr/bin/env node
// Builds dist/jai.mjs (the TypeScript JAI library as an ES module), the
// extension in dist/extension/, test bundles in dist/test/, and JAI Studio's
// bundles in dist/studio/: host.mjs (run by QuickJS), the webview UI in ui/,
// and conformance.mjs (the spec's conformance runner, for the QuickJS tests).
import { build } from 'esbuild';
import { cpSync, existsSync, mkdirSync, readFileSync, readdirSync, rmSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { pathToFileURL } from 'node:url';

// Clean dist/ but keep a built dist/studio/jai.exe (it may be running, and it is
// rebuilt by studio/scripts/build-windows.sh, not here).
for (const e of existsSync('dist') ? readdirSync('dist') : []) if (e !== 'studio') rmSync(join('dist', e), { recursive: true, force: true });
for (const e of existsSync('dist/studio') ? readdirSync('dist/studio') : []) if (e !== 'jai.exe') rmSync(join('dist/studio', e), { recursive: true, force: true });
mkdirSync('dist/extension/icons', { recursive: true });
mkdirSync('dist/studio/ui', { recursive: true });

const common = { bundle: true, target: 'es2022', logLevel: 'warning', legalComments: 'none', external: ['node:*'] };
await build({ ...common, entryPoints: ['src/jai/index.ts'], format: 'esm', platform: 'neutral', outfile: 'dist/jai.mjs' });
await build({ ...common, entryPoints: ['src/renderers/replicate.ts', 'src/renderers/prompt.ts'], format: 'esm', platform: 'neutral', outdir: 'dist/test' });
for (const f of ['src/extension/encode-guard.ts', 'src/extension/fetch-guard.ts', 'src/providers/replicate-client.ts']) await build({ ...common, entryPoints: [f], format: 'esm', platform: 'neutral', outfile: `dist/test/${f.split('/').pop().replace('.ts', '.js')}` });
await build({ ...common, entryPoints: ['src/encoder/index.ts'], format: 'esm', platform: 'neutral', outfile: 'dist/encoder.mjs' });
await build({ ...common, entryPoints: ['src/extension/background.ts'], format: 'esm', platform: 'browser', outfile: 'dist/extension/background.js' });
await build({ ...common, entryPoints: ['src/extension/content.ts'], format: 'iife', platform: 'browser', outfile: 'dist/extension/content.js' });
await build({ ...common, entryPoints: ['src/extension/popup.ts', 'src/extension/options.ts', 'src/extension/encode.ts'], format: 'esm', platform: 'browser', outdir: 'dist/extension' });
for (const f of ['manifest.json', 'popup.html', 'options.html', 'encode.html', 'ui.css']) cpSync(`src/extension/${f}`, `dist/extension/${f}`);
for (const s of [16, 32, 48, 128]) cpSync(`assets/icon${s}.png`, `dist/extension/icons/icon${s}.png`);

// JAI Studio (studio/DESIGN.md sections 1, 8 and 11). node:* stays external in
// host.mjs, so the library's dynamic import("node:zlib") reaches the host's
// synthetic module.
const { version } = JSON.parse(readFileSync('package.json', 'utf8'));
await build({ ...common, entryPoints: ['src/studio/host-entry.ts'], format: 'esm', platform: 'neutral', outfile: 'dist/studio/host.mjs', define: { JAI_TOOLS_VERSION: JSON.stringify(version) } });
await build({ ...common, entryPoints: ['src/studio/ui/main.ts'], format: 'esm', platform: 'browser', outfile: 'dist/studio/ui/main.js' });
// The UI's DOM-free rules, for test/studio-ui.test.mjs.
await build({ ...common, entryPoints: ['src/studio/ui/rules.ts'], format: 'esm', platform: 'neutral', outfile: 'dist/test/studio-ui-rules.js' });
for (const f of ['index.html', 'style.css']) cpSync(`src/studio/ui/${f}`, `dist/studio/ui/${f}`);
cpSync('assets/icon128.png', 'dist/studio/ui/icon.png');

// The spec's conformance runner for QuickJS: node:fs, node:path and node:url
// become shims over globalThis.__host (src/studio/globals.d.ts), and the banner
// supplies the process and Buffer.from(base64) that run.mjs uses. Its
// import(pathToFileURL(implPath).href) stays a dynamic import for the host's
// module loader.
const runner = resolve(process.env.JAI_SPEC_DIR || resolve('..', 'jai'), 'tests', 'run.mjs');
if (existsSync(runner)) {
  const banner = [
    'const __jaiHost = globalThis.__host;',
    'var process = { argv: __jaiHost.argv, env: {}, cwd: () => __jaiHost.cwd(), exit: code => __jaiHost.exit(code ?? 0) };',
    "var Buffer = { from(s, encoding) { if (encoding !== 'base64') throw new TypeError('Buffer shim: base64 only'); return Uint8Array.from(atob(s), c => c.charCodeAt(0)); } };",
    `const __jaiModuleUrl = __jaiHost.moduleUrl ?? ${JSON.stringify(pathToFileURL(runner).href)};`,
  ].join('\n');
  const shim = name => resolve('src/studio/shims', `${name}.ts`);
  await build({
    ...common,
    external: [],
    entryPoints: [runner],
    format: 'esm',
    platform: 'neutral',
    outfile: 'dist/studio/conformance.mjs',
    alias: { 'node:fs': shim('fs'), 'node:path': shim('path'), 'node:url': shim('url') },
    define: { 'import.meta.url': '__jaiModuleUrl' },
    banner: { js: banner },
  });
} else {
  console.warn(`warning: ${runner} not found; skipped dist/studio/conformance.mjs (set JAI_SPEC_DIR to the jai spec repository)`);
}
console.log('built dist/jai.mjs, dist/encoder.mjs, dist/extension/ and dist/studio/');
