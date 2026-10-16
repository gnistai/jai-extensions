#!/usr/bin/env node
// jai: command-line app for JAI (Just AI Images).
//   jai encode <image>      image -> .jai by analysis by synthesis (Replicate)
//   jai render <doc>        .jai, JAI text or JSON -> image (Replicate)
//   jai validate <doc>      parse, validate and normalise
// The token comes from REPLICATE_API_TOKEN (or --env <file>); it is never
// printed or written to reports. Run `npm run build` first.
import { readFileSync, writeFileSync, mkdirSync, existsSync } from 'node:fs';
import { basename, dirname, extname, join, resolve } from 'node:path';
import { parseArgs } from 'node:util';

const HELP = `jai encode <image> [options]
  --models <list>       target models, comma-separated ids or aliases (default flux-schnell)
  --budget <usd>        hard spending limit in USD (default 0.50)
  --population <n>      candidates per generation (default 4)
  --generations <n>     evolve generations including the first (default 5)
  --seeds <list>        fixed seeds, comma-separated (default 1)
  --analyst <name>      gemini (default) or claude
  --no-pixel            no colour/layout term
  --judge-weight <w>    weight of the analyst's rating, 0..1 (default 0.5; 0 = off)
  --no-refine           skip per-model refinements
  --scores              record scores in jaitools.* vendor keys
  --out <file.jai>      output file (default <image>.jai)
  --report <dir>        report directory (default <out>.report)
  --force               overwrite an existing output file
jai render <file.jai | text> [--model <id>] [--out <image>]
jai validate <file.jai | text>
Common: --env <file>  read REPLICATE_API_TOKEN from a dotenv file`;

const { values: o, positionals } = parseArgs({
  allowPositionals: true,
  options: {
    models: { type: 'string' }, model: { type: 'string' }, budget: { type: 'string' }, population: { type: 'string' },
    generations: { type: 'string' }, 'judge-weight': { type: 'string' }, seeds: { type: 'string' }, analyst: { type: 'string' }, 'no-pixel': { type: 'boolean' },
    'no-refine': { type: 'boolean' }, scores: { type: 'boolean' }, out: { type: 'string' }, report: { type: 'string' },
    env: { type: 'string' }, force: { type: 'boolean' }, help: { type: 'boolean', short: 'h' },
  },
});
const [cmd, input] = positionals;
if (o.help || !cmd || !input) { console.log(HELP); process.exit(cmd ? 2 : 0); }

const here = new URL('..', import.meta.url);
const J = await import(new URL('dist/jai.mjs', here)).catch(() => { console.error('Run `npm run build` first.'); process.exit(1); });
const E = await import(new URL('dist/encoder.mjs', here));

function token() {
  if (o.env) for (const line of readFileSync(o.env, 'utf8').split(/\r?\n/)) {
    const m = line.match(/^\s*(?:export\s+)?REPLICATE_API_TOKEN\s*=\s*["']?([^"'\s]+)/);
    if (m) process.env.REPLICATE_API_TOKEN = m[1];
  }
  const t = process.env.REPLICATE_API_TOKEN;
  if (!t) { console.error('Set REPLICATE_API_TOKEN or pass --env <file>.'); process.exit(1); }
  return t;
}

function findModel(name) {
  const n = name.trim().toLowerCase();
  const m = E.REPLICATE_MODELS.find(x => x.id === n || x.aliases.includes(n)) ?? E.REPLICATE_MODELS.find(x => x.family === n);
  if (!m) { console.error(`Unknown model "${name}". Known: ${E.REPLICATE_MODELS.map(x => x.aliases[0] ?? x.id).join(', ')}`); process.exit(2); }
  return m;
}

function readDocument(arg) {
  if (existsSync(arg)) {
    const r = J.decode(new Uint8Array(readFileSync(arg)));
    return { document: r.document, diagnostics: r.diagnostics, info: r.info };
  }
  const r = arg.trimStart().startsWith('{') ? J.readJSON(arg) : J.parse(arg);
  return { document: r.document, diagnostics: r.diagnostics, info: r.info };
}

/** A number option: finite, within [lo, hi], an integer when asked; exits with code 2 otherwise. */
function numberOption(name, fallback, lo, hi, integer = false) {
  const raw = o[name];
  if (raw === undefined) return fallback;
  const n = Number(raw);
  if (!Number.isFinite(n) || n < lo || n > hi || (integer && !Number.isInteger(n))) {
    console.error(`--${name} must be ${integer ? 'an integer' : 'a number'} from ${lo} to ${hi}`);
    process.exit(2);
  }
  return n;
}

const esc = s => String(s).replace(/[&<>"']/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[c]);
const usd = n => `$${n.toFixed(4)}`;
const scoreLine = c => Object.entries(c.perModel).map(([m, s]) => `${m.split('/')[1]} ${s.toFixed(3)}`).join(', ');

if (cmd === 'validate') {
  const r = readDocument(input);
  console.log(J.serialize(r.document));
  const all = [...r.diagnostics, ...J.validate(r.document, r.info)];
  for (const d of all) console.log(`  ${d.code}${d.key ? ` (${d.key})` : ''}`);
  process.exit(r.diagnostics.some(d => ['invalid-json', 'malformed-container', 'undecodable'].includes(d.code)) ? 1 : 0);
}

if (cmd === 'render') {
  const r = readDocument(input);
  const catalog = E.REPLICATE_MODELS.map(m => ({ id: m.id, aliases: m.aliases, family: m.family, imageInput: m.imageInput, default: m.id === 'black-forest-labs/flux-schnell' }));
  const selected = o.model ? findModel(o.model).id : null;
  const { interpreted: it, diagnostics } = J.interpret(r.document, { catalog, families: E.FAMILIES, selected });
  if (it.mode !== 'generate') { console.error(`mode=${it.mode} needs an input image; the CLI renders generate only.`); process.exit(2); }
  const model = E.REPLICATE_MODELS.find(m => m.id === it.matched);
  const renderer = new E.ReplicateRenderer(token(), { foldAvoid: true });
  const res = await renderer.render({ intent: { description: it.modelDescription, style: it.modelStyle, avoid: it.modelAvoid, aspect: it.aspect ? { width: it.aspect.width, height: it.aspect.height } : null, mode: 'generate', strength: null, seed: it.seed, animate: false }, modelId: model.id, inputImage: null, governingRatio: null, target: null });
  const out = o.out ?? `render-${Date.now()}.${res.mime.split('/')[1]}`;
  writeFileSync(out, res.bytes);
  console.log(`${model.label} -> ${out} (about ${usd(model.costHint)})`);
  console.log(`sent: ${res.sentPrompt}`);
  for (const d of [...diagnostics, ...res.diagnostics]) console.log(`  ${d.code}${d.key ? ` (${d.key})` : ''}`);
  process.exit(0);
}

if (cmd !== 'encode') { console.log(HELP); process.exit(2); }

// ---------------------------------------------------------------- encode
const sharp = (await import('sharp')).default;
const src = resolve(input);
// Refuse decompression bombs before decoding (spec: Untrusted input and bounds).
const PIXELS = 64e6;
const meta = await sharp(src, { limitInputPixels: PIXELS }).metadata().catch(e => { console.error(`Cannot read ${basename(src)}: ${e.message}`); process.exit(1); });
if (!meta.width || !meta.height || meta.width * meta.height > PIXELS) { console.error(`${basename(src)} is larger than ${PIXELS / 1e6} megapixels.`); process.exit(1); }
const oriented = (meta.orientation ?? 1) >= 5 ? { width: meta.height, height: meta.width } : { width: meta.width, height: meta.height };
// Upload copy: at most 1024 px on the long side, JPEG, metadata stripped.
const upload = await sharp(src, { limitInputPixels: PIXELS }).rotate().resize(1024, 1024, { fit: 'inside', withoutEnlargement: true }).jpeg({ quality: 85 }).toBuffer();
const target = { url: `data:image/jpeg;base64,${upload.toString('base64')}`, width: oriented.width, height: oriented.height };
const THUMB = 16;
const thumb = async buf => { const { data, info } = await sharp(buf).rotate().removeAlpha().resize(THUMB, THUMB, { fit: 'cover' }).ensureAlpha().raw().toBuffer({ resolveWithObject: true }); return { width: info.width, height: info.height, data }; };

const models = (o.models ?? 'flux-schnell').split(',').filter(Boolean).map(findModel);
const budgetUSD = numberOption('budget', 0.5, 0.01, 50);
const population = numberOption('population', 4, 1, 9, true);
const generations = numberOption('generations', 5, 1, 30, true);
const judgeWeight = numberOption('judge-weight', 0.5, 0, 1);
const seeds = (o.seeds ?? '1').split(',').map(x => {
  const n = Number(x);
  if (!Number.isSafeInteger(n) || n < 0 || n > 2 ** 32 - 1) { console.error('--seeds must be integers from 0 to 4294967295'); process.exit(2); }
  return n;
});
const client = new E.HttpReplicateClient(token());
const composite = async images => {
  const bufs = await Promise.all(images.map(async i => i.url.startsWith('data:') ? Buffer.from(i.url.split(',')[1], 'base64') : Buffer.from(await (await fetch(i.url)).arrayBuffer())));
  const tiles = await Promise.all(bufs.map(b => sharp(b, { limitInputPixels: PIXELS }).resize(512, 512, { fit: 'contain', background: '#fff' }).toBuffer()));
  const img = await sharp({ create: { width: 512 * tiles.length, height: 512, channels: 3, background: '#fff' } }).composite(tiles.map((t, k) => ({ input: t, left: 512 * k, top: 0 }))).jpeg({ quality: 85 }).toBuffer();
  return { url: `data:image/jpeg;base64,${img.toString('base64')}` };
};
const analyst = o.analyst === 'claude' ? new E.ClaudeAnalyst(client, { composite }) : new E.GeminiAnalyst(client);
const out = resolve(o.out ?? join(dirname(src), `${basename(src, extname(src))}.jai`));
if (existsSync(out) && !o.force) { console.error(`${out} exists; pass --force to overwrite it.`); process.exit(2); }
const reportDir = o.report ?? `${out}.report`;
mkdirSync(join(reportDir, 'renders'), { recursive: true });
writeFileSync(join(reportDir, 'target.jpg'), upload);

console.log(`Encoding ${basename(src)} (${target.width}x${target.height}) for ${models.map(m => m.label).join(', ')}; budget ${usd(budgetUSD)}; analyst ${analyst.id}`);
const files = new Map();
let n = 0;
const result = await E.encode({
  target, models: models.map(m => m.id), renderer: new E.ReplicateRenderer(client, { foldAvoid: true }), analyst, scorer: new E.ClipScorer(client),
  ...(o['no-pixel'] ? {} : { thumbnail: r => thumb(Buffer.from(r.bytes)), targetThumbnail: await thumb(upload) }),
  budgetUSD, judgeWeight, population, maxGenerations: generations,
  seeds, refine: o['no-refine'] ? false : undefined,
  onEvent: e => {
    if (e.type === 'phase') console.log(`\n== ${e.phase}`);
    if (e.type === 'candidate') {
      for (const r of e.candidate.renders) if (r.result) {
        const f = `renders/${String(++n).padStart(3, '0')}-${e.candidate.id}-${r.model.split('/')[1]}-s${r.seed}.${r.result.mime.split('/')[1]}`;
        writeFileSync(join(reportDir, f), r.result.bytes);
        files.set(r, f);
      }
      const scores = Object.entries(e.candidate.perModel).map(([m, s]) => `${m.split('/')[1]} ${s.toFixed(3)}`).join(', ');
      console.log(`  ${e.candidate.id.padEnd(16)} ${e.candidate.fitness.toFixed(3)}  [${scores}]`);
    }
    if (e.type === 'generation') console.log(`  -> gen ${e.generation.index} best ${e.generation.best} ${e.generation.bestFitness.toFixed(3)}, spent ${usd(e.generation.spent)}${e.generation.notes ? `\n     ${e.generation.notes.slice(0, 300)}` : ''}`);
    if (e.type === 'log') console.log(`  ${e.message}`);
  },
}).catch(e => { console.error(`\nEncoding failed: ${e.message}${e.spent !== undefined ? ` (spent about ${usd(e.spent)})` : ''}`); process.exit(1); });

let doc = result.document;
if (o.scores) {
  doc = J.fromObject({ ...J.toObject(doc), 'jaitools.score': result.ranking.map(r => `${r.model} ${r.score.toFixed(4)}`), 'jaitools.encoder': [`${analyst.id} + ${E.CLIP_MODEL.id}`] });
}
writeFileSync(out, await J.encode(doc));
const text = J.serialize(doc);

// Report: JSON without image bytes, plus an HTML contact sheet with relative image paths.
const strip = c => ({ ...c, renders: c.renders.map(r => ({ model: r.model, seed: r.seed, file: files.get(r) ?? null, error: r.error, semantic: r.semantic, pixel: r.pixel, judge: r.judge, metric: r.metric, score: r.score, sentPrompt: r.result?.sentPrompt ?? null })) });
const report = { source: basename(src), size: [target.width, target.height], models: models.map(m => m.id), analyst: analyst.id, scorer: E.CLIP_MODEL.id, budgetUSD, text, ranking: result.ranking, stopReason: result.stopReason, error: result.error, spentEstimateUSD: result.spent, seconds: result.seconds, generations: result.generations, best: strip(result.best), candidates: result.candidates.map(strip), spend: result.spend };
writeFileSync(join(reportDir, 'report.json'), JSON.stringify(report, null, 2));
const byId = new Map(result.candidates.map(c => [c.id, c]));
const card = c => `<div class="cand${c.id === result.best.id ? ' best' : ''}"><div class="h"><b>${esc(c.id)}</b> ${c.fitness.toFixed(3)}</div><div class="imgs">${c.renders.map(r => `<figure>${files.get(r) ? `<img src="${esc(files.get(r))}" alt="" loading="lazy">` : '<div class="fail">failed</div>'}<figcaption>${esc(r.model.split('/')[1])} ${r.score.toFixed(3)}${r.judge !== null ? ` (judge ${Math.round(r.judge * 100)})` : ''}</figcaption></figure>`).join('')}</div><p>${esc(c.draft.prompt)}${Object.entries(c.refinements).map(([m, v]) => `<br><i>+ ${esc(m.split('/')[1])}: ${esc(v.join(' '))}</i>`).join('')}</p></div>`;
const html = `<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>JAI encoding: ${esc(basename(src))}</title>
<style>body{font:15px/1.5 system-ui,sans-serif;margin:0 auto;max-width:1200px;padding:16px;color:#1c1917;background:#fafaf9}h1{font-size:1.4rem}pre{white-space:pre-wrap;background:#fff;border:1px solid #d6d3d1;padding:12px;border-radius:8px}.gen{margin:24px 0}.row{display:flex;flex-wrap:wrap;gap:12px}.cand{background:#fff;border:1px solid #d6d3d1;border-radius:8px;padding:8px;width:min(100%,360px)}.cand.best{border:2px solid #c2410c}.imgs{display:flex;gap:6px}figure{margin:0;flex:1}figure img{width:100%;border-radius:4px}figcaption{font-size:.8rem;color:#57534e}.cand p{font-size:.85rem;margin:6px 0 0}.fail{background:#fee2e2;padding:20px;text-align:center}table{border-collapse:collapse}td,th{padding:2px 10px;text-align:left}</style></head><body>
<h1>JAI encoding of ${esc(basename(src))}</h1>
<div class="row"><figure style="max-width:360px"><img src="target.jpg" alt="Target image"><figcaption>Target, ${target.width}×${target.height}</figcaption></figure></div>
<h2>Result</h2><pre>${esc(text)}</pre>
<table><tr><th>Model</th><th>Score</th></tr>${result.ranking.map(r => `<tr><td>${esc(r.model)}</td><td>${r.score.toFixed(3)}</td></tr>`).join('')}</table>
<p>Stopped: ${esc(result.stopReason)}${result.error ? ` (${esc(result.error)})` : ''}. Estimated spend ${usd(result.spent)} of ${usd(budgetUSD)} in ${result.seconds.toFixed(0)} s. Analyst ${esc(analyst.id)}, scorer ${esc(E.CLIP_MODEL.id)}.</p>
${result.generations.map(g => `<section class="gen"><h3>${g.phase === 'refine' ? 'Refinement' : 'Generation'} ${g.index}: best ${g.bestFitness.toFixed(3)}</h3>${g.notes ? `<p>${esc(g.notes)}</p>` : ''}<div class="row">${g.candidates.map(id => byId.get(id)).filter(Boolean).map(card).join('')}</div></section>`).join('\n')}
</body></html>`;
writeFileSync(join(reportDir, 'index.html'), html);

console.log(`\n${text}\n`);
for (const r of result.ranking) console.log(`  ${r.model.padEnd(36)} ${r.score.toFixed(3)}`);
console.log(`\nStopped: ${result.stopReason}${result.error ? ` (${result.error})` : ''}. Estimated spend ${usd(result.spent)} of ${usd(budgetUSD)}, ${result.seconds.toFixed(0)} s.`);
console.log(`Wrote ${out} and ${join(reportDir, 'index.html')}`);
