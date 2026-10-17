#!/usr/bin/env node
// jai: command-line app for JAI (Just AI Images).
//   jai encode <image>      image -> .jai by analysis by synthesis (Replicate, or JAI Studio with --local)
//   jai render <doc>        .jai, JAI text or JSON -> image (Replicate, or JAI Studio with --local)
//   jai validate <doc>      parse, validate and normalise
// The Replicate token comes from REPLICATE_API_TOKEN (or --env <file>). With
// --local, the address and token of JAI Studio's local server come from its
// data folder (studio/DESIGN.md section 2). Tokens are never printed or
// written to reports. Run `npm run build` first.
import { createHmac, randomBytes, timingSafeEqual } from 'node:crypto';
import { readFileSync, writeFileSync, mkdirSync, existsSync } from 'node:fs';
import { request } from 'node:http';
import { isIP } from 'node:net';
import { homedir } from 'node:os';
import { basename, dirname, extname, isAbsolute, join, resolve } from 'node:path';
import { parseArgs } from 'node:util';

const HELP = `jai encode <image> [options]
  --models <list>       target models, comma-separated ids or aliases (default flux-schnell)
  --budget <usd>        hard spending limit in USD (default 0.50)
  --population <n>      candidates per generation (default 4)
  --generations <n>     evolve generations including the first (default 5)
  --seeds <list>        fixed seeds, comma-separated (default 1,2)
  --analyst <name>      luna (default: openai/gpt-5.6-luna) or gemini; describes and measures
  --optimiser <name>    who writes improved descriptions: the analyst (default), luna, gemini or sonnet
  --restarts <n>        independent attempts within the budget; the best by fitness wins (default 1)
  --goal <f>            stop when the measured fitness reaches this, 0..1 (default 0.9)
  --no-pixel            no colour/layout term
  --no-refine           skip per-model refinements
  --scores              record scores in jaitools.* vendor keys
  --out <file.jai>      output file (default <image>.jai)
  --report <dir>        report directory (default <out>.report)
  --force               overwrite an existing output file
  --local               encode with JAI Studio's local models (FLUX.1 schnell, Qwen3.8 27B) at no
                        cost; --budget, --analyst and --optimiser do not apply
  --think <mode>        with --local: when Qwen thinks: off (default), propose or all
  --scale <f>           with --local: render size factor from 0.25 to 1.25, 1 is about a
                        megapixel (default 1)
jai render <file.jai | text> [--model <id>] [--out <image>] [--local]
jai validate <file.jai | text>
Common: --env <file>  read REPLICATE_API_TOKEN from a dotenv file
        --local       use the JAI Studio server running on this computer instead of Replicate;
                      no token needed. It is found through server.json and token in Studio's
                      data folder: JAI_HOME, else %LOCALAPPDATA%\\JAI on Windows, else
                      $XDG_DATA_HOME/jai or ~/.local/share/jai`;

const { values: o, positionals } = parseArgs({
  allowPositionals: true,
  options: {
    models: { type: 'string' }, model: { type: 'string' }, budget: { type: 'string' }, population: { type: 'string' },
    generations: { type: 'string' }, optimiser: { type: 'string' }, restarts: { type: 'string' }, goal: { type: 'string' }, seeds: { type: 'string' }, analyst: { type: 'string' }, 'no-pixel': { type: 'boolean' },
    'no-refine': { type: 'boolean' }, scores: { type: 'boolean' }, out: { type: 'string' }, report: { type: 'string' },
    env: { type: 'string' }, force: { type: 'boolean' }, local: { type: 'boolean' }, think: { type: 'string' }, scale: { type: 'string' },
    help: { type: 'boolean', short: 'h' },
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

function findModel(name, models = E.REPLICATE_MODELS) {
  const n = name.trim().toLowerCase();
  const m = models.find(x => x.id === n || x.aliases.includes(n)) ?? models.find(x => x.family === n);
  if (!m) { console.error(`Unknown model "${name}". Known: ${models.map(x => x.aliases[0] ?? x.id).join(', ')}`); process.exit(2); }
  return m;
}

/** JAI Studio's data folder: JAI_HOME, else the platform default (studio/DESIGN.md section 2). */
function studioRoot() {
  if (process.env.JAI_HOME) return process.env.JAI_HOME;
  if (process.platform === 'win32') return join(process.env.LOCALAPPDATA || join(homedir(), 'AppData', 'Local'), 'JAI');
  const xdg = process.env.XDG_DATA_HOME;
  return join(xdg && isAbsolute(xdg) ? xdg : join(homedir(), '.local', 'share'), 'jai');
}

/**
 * The running JAI Studio server: <root>/server.json and <root>/token. Before
 * the token goes anywhere, the server there must prove that it holds it:
 * GET /health?nonce=<random> answers with base64url(HMAC-SHA256(token,
 * "jai-health:<port>:<nonce>")) (studio/DESIGN.md section 7). server.json
 * outlives a server that crashed, and another program or user may then listen
 * on its port; only the nonce is sent to it.
 */
async function localServer() {
  const root = studioRoot();
  const notRunning = `JAI Studio's server is not running (no server.json in ${root}). Start JAI Studio or run \`jai serve\`.`;
  let info, key;
  try { info = JSON.parse(readFileSync(join(root, 'server.json'), 'utf8')); } catch { console.error(notRunning); process.exit(1); }
  try { key = readFileSync(join(root, 'token'), 'utf8').trim(); } catch { key = ''; }
  if (!key) { console.error(`Cannot read JAI Studio's token in ${root}.`); process.exit(1); }
  const port = Number(info?.port);
  if (!Number.isInteger(port) || port < 1 || port > 65535) { console.error(notRunning); process.exit(1); }
  // A wildcard bind is reached through loopback. Only an IP address, never a host name.
  const host = typeof info.host !== 'string' || !info.host || info.host === '0.0.0.0' ? '127.0.0.1' : info.host === '::' ? '::1' : info.host;
  if (!isIP(host)) { console.error(notRunning); process.exit(1); }
  const url = `http://${host.includes(':') ? `[${host}]` : host}:${port}`;
  const nonce = randomBytes(32).toString('base64url');
  const health = await fetch(`${url}/health?nonce=${nonce}`, { redirect: 'error', signal: AbortSignal.timeout(5000) })
    .then(r => (r.ok ? r.json() : null)).catch(() => null);
  if (health?.name !== 'jai') { console.error(`No JAI Studio server answers at ${url}. Start JAI Studio or run \`jai serve\`.`); process.exit(1); }
  const expected = Buffer.from(createHmac('sha256', key).update(`jai-health:${port}:${nonce}`).digest('base64url'));
  const shown = Buffer.from(typeof health.proof === 'string' ? health.proof : '');
  if (shown.length !== expected.length || !timingSafeEqual(shown, expected)) {
    console.error(`The server at ${url} did not prove that it holds your JAI Studio token, so the token was not sent. Restart JAI Studio or \`jai serve\`.`);
    process.exit(1);
  }
  return { url, token: key };
}

/**
 * fetch for JAI Studio's local server over node:http, without the time limits
 * of Node's own fetch (undici gives up after 300 s without reply headers).
 * Studio replies only once the request's engine is free and the job is done;
 * the providers keep their own limits (DESIGN.md section 9).
 */
function localFetch(url, init = {}) {
  return new Promise((resolve, reject) => {
    const { signal } = init;
    const aborted = () => new DOMException('This operation was aborted', 'AbortError');
    if (signal?.aborted) { reject(aborted()); return; }
    const fail = e => { signal?.removeEventListener('abort', onAbort); reject(signal?.aborted ? aborted() : new TypeError('fetch failed', { cause: e })); };
    const req = request(url, { method: init.method ?? 'GET', headers: init.headers }, res => {
      const chunks = [];
      res.on('data', c => chunks.push(c));
      res.on('error', fail);
      res.on('end', () => {
        signal?.removeEventListener('abort', onAbort);
        if (!res.complete) { fail(Object.assign(new Error('the connection closed before the reply was complete'), { code: 'ECONNRESET' })); return; }
        const status = res.statusCode ?? 0;
        const headers = Object.entries(res.headers).flatMap(([k, v]) => (v === undefined ? [] : (Array.isArray(v) ? v : [v]).map(x => [k, x])));
        resolve(new Response([101, 204, 205, 304].includes(status) ? null : Buffer.concat(chunks), { status, statusText: res.statusMessage, headers }));
      });
    });
    const onAbort = () => req.destroy(aborted());
    signal?.addEventListener('abort', onAbort, { once: true });
    req.on('error', fail);
    req.end(init.body);
  });
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
  const local = o.local ? await localServer() : null;
  const models = local ? [E.LOCAL_FLUX] : E.REPLICATE_MODELS;
  const catalog = models.map(m => ({ id: m.id, aliases: m.aliases, family: m.family, imageInput: m.imageInput, default: m.id === 'black-forest-labs/flux-schnell' }));
  const selected = o.model ? findModel(o.model, models).id : null;
  const { interpreted: it, diagnostics } = J.interpret(r.document, { catalog, families: E.FAMILIES, selected });
  if (it.mode !== 'generate') { console.error(`mode=${it.mode} needs an input image; the CLI renders generate only.`); process.exit(2); }
  const model = models.find(m => m.id === it.matched);
  const renderer = local ? new E.LocalRenderer(local.url, local.token, { fetch: localFetch, outputFormat: /\.jpe?g$/i.test(o.out ?? '') ? 'jpeg' : 'png' }) : new E.ReplicateRenderer(token(), { foldAvoid: true });
  const res = await renderer.render({ intent: { description: it.modelDescription, style: it.modelStyle, avoid: it.modelAvoid, aspect: it.aspect ? { width: it.aspect.width, height: it.aspect.height } : null, mode: 'generate', strength: null, seed: it.seed, animate: false }, modelId: model.id, inputImage: null, governingRatio: null, target: null })
    .catch(e => { console.error(`Render failed: ${e.message}`); process.exit(1); });
  const out = o.out ?? `render-${Date.now()}.${res.mime.split('/')[1]}`;
  writeFileSync(out, res.bytes);
  console.log(local ? `${model.label} on JAI Studio -> ${out} (seed ${res.sentInput.seed}, ${res.sentInput.width}x${res.sentInput.height})` : `${model.label} -> ${out} (about ${usd(model.costHint)})`);
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

const local = o.local ? await localServer() : null;
if (local) for (const k of ['budget', 'analyst', 'optimiser']) if (o[k] !== undefined) { console.error(`--${k} does not apply with --local`); process.exit(2); }
if (!local) for (const k of ['think', 'scale']) if (o[k] !== undefined) { console.error(`--${k} needs --local`); process.exit(2); }
const models = [...new Map((o.models ?? 'flux-schnell').split(',').filter(Boolean).map(name => findModel(name, local ? [E.LOCAL_FLUX] : E.REPLICATE_MODELS)).map(m => [m.id, m])).values()];
const budgetUSD = local ? 0 : numberOption('budget', 0.5, 0.01, 50);
const population = numberOption('population', 4, 1, 9, true);
const generations = numberOption('generations', 5, 1, 30, true);
const restarts = numberOption('restarts', 1, 1, 5, true);
const goal = numberOption('goal', 0.9, 0, 1);
const seeds = (o.seeds ?? '1,2').split(',').map(x => {
  const n = Number(x);
  if (!Number.isSafeInteger(n) || n < 0 || n > 2 ** 32 - 1) { console.error('--seeds must be integers from 0 to 4294967295'); process.exit(2); }
  return n;
});
const thinking = o.think ?? 'off';
if (!['off', 'propose', 'all'].includes(thinking)) { console.error('--think must be off, propose or all'); process.exit(2); }
// As JAI Studio's own encoder allows: up to 1.25, every FLUX size fits the server's limits.
const scale = numberOption('scale', 1, 0.25, 1.25);
let analyst, optimiser, renderer;
if (local) {
  analyst = optimiser = new E.LocalAnalyst(new E.OpenAIChatClient(local.url, local.token, { fetch: localFetch }), { thinking });
  renderer = new E.LocalRenderer(local.url, local.token, { fetch: localFetch, outputFormat: 'jpeg', scale });
} else {
  const client = new E.HttpReplicateClient(token());
  const makeAnalyst = name => {
    if (name === 'gemini') return new E.GeminiAnalyst(client);
    if (name === 'sonnet') return new E.ClaudeAnalyst(client);
    if (name === undefined || name === 'luna') return new E.OpenAIAnalyst(client);
    console.error(`Unknown model "${name}": use luna, gemini or sonnet`); process.exit(2);
  };
  analyst = makeAnalyst(o.analyst);
  if (o.analyst === 'sonnet') { console.error('--analyst sonnet takes one image per call; use it as --optimiser'); process.exit(2); }
  optimiser = o.optimiser ? makeAnalyst(o.optimiser) : analyst;
  renderer = new E.ReplicateRenderer(client, { foldAvoid: true });
}
const out = resolve(o.out ?? join(dirname(src), `${basename(src, extname(src))}.jai`));
if (existsSync(out) && !o.force) { console.error(`${out} exists; pass --force to overwrite it.`); process.exit(2); }
const reportDir = o.report ?? `${out}.report`;
mkdirSync(join(reportDir, 'renders'), { recursive: true });
writeFileSync(join(reportDir, 'target.jpg'), upload);

console.log(local
  ? `Encoding ${basename(src)} (${target.width}x${target.height}) for ${models.map(m => m.label).join(', ')} with JAI Studio at ${local.url}; analyst ${analyst.id}${thinking !== 'off' ? `, thinking: ${thinking}` : ''}`
  : `Encoding ${basename(src)} (${target.width}x${target.height}) for ${models.map(m => m.label).join(', ')}; budget ${usd(budgetUSD)}; analyst ${analyst.id}${optimiser !== analyst ? `, optimiser ${optimiser.id}` : ''}`);
const files = new Map();
let n = 0;
const result = await E.encode({
  target, models: models.map(m => m.id), renderer, analyst, optimiser,
  ...(o['no-pixel'] ? {} : { thumbnail: r => thumb(Buffer.from(r.bytes)), targetThumbnail: await thumb(upload) }),
  budgetUSD, population, maxGenerations: generations, restarts, goal,
  seeds, refine: o['no-refine'] ? false : undefined,
  // The engine's default concurrency (four renders and four inspections at once) also suits
  // JAI Studio: its scheduler still batches each engine's work, and no request waits behind
  // a whole generation.
  onEvent: e => {
    if (e.type === 'phase') console.log(`\n== ${e.phase}${e.lineage ? ` (lineage ${e.lineage})` : ''}`);
    if (e.type === 'target') console.log(`  target: ${E.describeObservation(e.observation).replace(/\n/g, '\n          ')}`);
    if (e.type === 'candidate') {
      for (const r of e.candidate.renders) if (r.result) {
        const f = `renders/${String(++n).padStart(3, '0')}-${e.candidate.id}-${r.model.split('/')[1]}-s${r.seed}.${r.result.mime.split('/')[1]}`;
        writeFileSync(join(reportDir, f), r.result.bytes);
        files.set(r, f);
      }
      const scores = Object.entries(e.candidate.perModel).map(([m, s]) => `${m.split('/')[1]} ${s.toFixed(3)}`).join(', ');
      console.log(`  ${e.candidate.id.padEnd(16)} ${e.candidate.fitness.toFixed(3)}  [${scores}]`);
      const fb = e.candidate.renders.find(r => r.measure)?.measure?.feedback;
      if (fb) console.log(`     ${fb.slice(0, 240)}`);
    }
    if (e.type === 'generation') console.log(`  -> l${e.generation.lineage} gen ${e.generation.index} best ${e.generation.best} ${e.generation.bestFitness.toFixed(3)}${local ? '' : `, spent ${usd(e.generation.spent)}`}${e.generation.notes ? `\n     ${e.generation.notes.slice(0, 300)}` : ''}`);
    if (e.type === 'log') console.log(`  ${e.message}`);
  },
}).catch(e => { console.error(`\nEncoding failed: ${e.message}${e.spent !== undefined ? ` (spent about ${usd(e.spent)})` : ''}`); process.exit(1); });

let doc = result.document;
if (o.scores) {
  doc = J.fromObject({ ...J.toObject(doc), 'jaitools.score': result.ranking.map(r => `${r.model} ${r.score.toFixed(4)}`), 'jaitools.encoder': [optimiser === analyst ? analyst.id : `${analyst.id} + ${optimiser.id}`] });
}
writeFileSync(out, await J.encode(doc));
const text = J.serialize(doc);

// Report: JSON without image bytes, plus an HTML contact sheet with relative image paths.
const strip = c => ({ ...c, renders: c.renders.map(r => ({ model: r.model, seed: r.seed, file: files.get(r) ?? null, error: r.error, observation: r.observation, measure: r.measure, score: r.score, sentPrompt: r.result?.sentPrompt ?? null })) });
const report = { source: basename(src), size: [target.width, target.height], models: models.map(m => m.id), analyst: analyst.id, optimiser: optimiser.id, settings: { population, generations, seeds, restarts, goal }, target: result.target, budgetUSD, text, ranking: result.ranking, stopReason: result.stopReason, error: result.error, spentEstimateUSD: result.spent, seconds: result.seconds, generations: result.generations, best: strip(result.best), candidates: result.candidates.map(strip), spend: result.spend };
writeFileSync(join(reportDir, 'report.json'), JSON.stringify(report, null, 2));
const byId = new Map(result.candidates.map(c => [c.id, c]));
const card = c => `<div class="cand${c.id === result.best.id ? ' best' : ''}"><div class="h"><b>${esc(c.id)}</b> ${c.fitness.toFixed(3)}</div><div class="imgs">${c.renders.map(r => `<figure>${files.get(r) ? `<img src="${esc(files.get(r))}" alt="" loading="lazy">` : '<div class="fail">failed</div>'}<figcaption>${esc(r.model.split('/')[1])} ${r.score.toFixed(3)}</figcaption></figure>`).join('')}</div><p class="fb">${esc(c.renders.find(r => r.measure)?.measure?.feedback ?? '')}</p><p>${esc(c.draft.prompt)}${Object.entries(c.refinements).map(([m, v]) => `<br><i>+ ${esc(m.split('/')[1])}: ${esc(v.join(' '))}</i>`).join('')}</p></div>`;
const html = `<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>JAI encoding: ${esc(basename(src))}</title>
<style>body{font:15px/1.5 system-ui,sans-serif;margin:0 auto;max-width:1200px;padding:16px;color:#1c1917;background:#fafaf9}h1{font-size:1.4rem}pre{white-space:pre-wrap;background:#fff;border:1px solid #d6d3d1;padding:12px;border-radius:8px}.gen{margin:24px 0}.row{display:flex;flex-wrap:wrap;gap:12px}.cand{background:#fff;border:1px solid #d6d3d1;border-radius:8px;padding:8px;width:min(100%,360px)}.cand.best{border:2px solid #c2410c}.imgs{display:flex;gap:6px}figure{margin:0;flex:1}figure img{width:100%;border-radius:4px}figcaption{font-size:.8rem;color:#57534e}.cand p{font-size:.85rem;margin:6px 0 0}.fail{background:#fee2e2;padding:20px;text-align:center}.fb{color:#57534e;font-size:.78rem!important}table{border-collapse:collapse}td,th{padding:2px 10px;text-align:left}</style></head><body>
<h1>JAI encoding of ${esc(basename(src))}</h1>
<div class="row"><figure style="max-width:360px"><img src="target.jpg" alt="Target image"><figcaption>Target, ${target.width}×${target.height}</figcaption></figure><pre style="flex:1;min-width:260px">${esc(E.describeObservation(result.target))}</pre></div>
<h2>Result</h2><pre>${esc(text)}</pre>
<table><tr><th>Model</th><th>Score</th></tr>${result.ranking.map(r => `<tr><td>${esc(r.model)}</td><td>${r.score.toFixed(3)}</td></tr>`).join('')}</table>
<p>Stopped: ${esc(result.stopReason)}${result.error ? ` (${esc(result.error)})` : ''}. ${local ? `Rendered and measured with JAI Studio's local models in ${result.seconds.toFixed(0)} s` : `Estimated spend ${usd(result.spent)} of ${usd(budgetUSD)} in ${result.seconds.toFixed(0)} s`}. Analyst ${esc(analyst.id)}${optimiser !== analyst ? `, optimiser ${esc(optimiser.id)}` : ''}; fitness is measured against the target's boxes, horizon and colours.</p>
${result.generations.map(g => `<section class="gen"><h3>${g.phase === 'refine' ? 'Refinement' : `Lineage ${g.lineage}, generation`} ${g.index}: best ${g.bestFitness.toFixed(3)}</h3>${g.notes ? `<p>${esc(g.notes)}</p>` : ''}<div class="row">${g.candidates.map(id => byId.get(id)).filter(Boolean).map(card).join('')}</div></section>`).join('\n')}
</body></html>`;
writeFileSync(join(reportDir, 'index.html'), html);

console.log(`\n${text}\n`);
for (const r of result.ranking) console.log(`  ${r.model.padEnd(36)} ${r.score.toFixed(3)}`);
console.log(`\nStopped: ${result.stopReason}${result.error ? ` (${result.error})` : ''}. ${local ? `JAI Studio, ${result.seconds.toFixed(0)} s.` : `Estimated spend ${usd(result.spent)} of ${usd(budgetUSD)}, ${result.seconds.toFixed(0)} s.`}`);
console.log(`Wrote ${out} and ${join(reportDir, 'index.html')}`);
