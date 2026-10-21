// A fake JAI Studio server for tests (test/local.test.mjs and the e2e driver):
// GET /health proves the token for a nonce as Studio does, base64url of
// HMAC-SHA256(token, "jai-health:<port>:<nonce>"); everything under /v1/ needs
// the Bearer token. Images are real PNGs (one colour per prompt and seed, from
// node:zlib, so it needs no sharp), and chat answers follow the analyst's
// response_format, so a whole encode runs against it. No GPU, no network.
import { createHmac } from 'node:crypto';
import { createServer } from 'node:http';
import { deflateSync } from 'node:zlib';

const CRC = Array.from({ length: 256 }, (_, n) => { let c = n; for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1; return c >>> 0; });
const crc32 = buf => { let c = 0xffffffff; for (const b of buf) c = CRC[(c ^ b) & 0xff] ^ (c >>> 8); return (c ^ 0xffffffff) >>> 0; };
const chunk = (type, data) => {
  const out = Buffer.alloc(12 + data.length);
  out.writeUInt32BE(data.length, 0);
  out.write(type, 4, 'latin1');
  data.copy(out, 8);
  out.writeUInt32BE(crc32(out.subarray(4, 8 + data.length)), 8 + data.length);
  return out;
};

/** A solid-colour RGB PNG. */
export function solidPNG(width, height, [r, g, b]) {
  const row = Buffer.alloc(1 + width * 3);
  for (let x = 0; x < width; x++) row.set([r, g, b], 1 + x * 3);
  const head = Buffer.alloc(13);
  head.writeUInt32BE(width, 0);
  head.writeUInt32BE(height, 4);
  head.set([8, 2, 0, 0, 0], 8);
  return Buffer.concat([Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]), chunk('IHDR', head), chunk('IDAT', deflateSync(Buffer.concat(Array(height).fill(row)))), chunk('IEND', Buffer.alloc(0))]);
}

const colourOf = text => { let h = 2166136261; for (const ch of text) h = Math.imul(h ^ ch.charCodeAt(0), 16777619); return [h & 255, (h >>> 8) & 255, (h >>> 16) & 255]; };

/** POST /v1/images/generations: a real PNG at the asked size. */
export function realImage(body) {
  const seed = body.seed ?? 7;
  const png = solidPNG(body.width, body.height, colourOf(`${body.prompt}|${seed}`));
  return { json: { created: 1, model: 'flux-schnell', width: body.width, height: body.height, steps: body.steps ?? 4, output_format: 'png', data: [{ b64_json: png.toString('base64'), seed }], timings: { queueMs: 3, generateMs: 1500 } } };
}

const draft = i => ({ prompt: `Photograph of a white lighthouse on a green cliff above the sea, wide view, variant ${i + 1}`, style: ['photo'], avoid: [] });

/** POST /v1/chat/completions: an answer of the shape the analyst asked for. */
export function analystAnswer(body) {
  const format = body.response_format?.json_schema ?? {};
  const props = format.schema?.properties ?? {};
  let answer;
  if (format.name === 'jai_drafts') answer = { notes: '', drafts: Array.from({ length: props.drafts?.minItems ?? 1 }, (_, i) => draft(i)) };
  else if (format.name === 'jai_additions') answer = { notes: '', additions: Array.from({ length: props.additions?.minItems ?? 1 }, () => 'Seen from the sea.') };
  else if (format.name === 'jai_target_inspection' || format.name === 'jai_render_inspection') {
    const names = props.subjects?.items?.properties?.name?.enum ?? ['lighthouse'];
    const count = props.subjects?.minItems ?? names.length;
    answer = {
      subjects: Array.from({ length: count }, (_, i) => ({ name: names[i % names.length], bbox_2d: [400, 200, 600, 800] })),
      people: [], crowd: null, horizon_y: 600, horizon_shape: 'flat', camera: 'eye level', lighting: 'daylight', medium: 'photograph',
    };
  } else answer = { ok: true };
  return { json: { object: 'chat.completion', model: 'qwen3.8-27b', choices: [{ index: 0, message: { role: 'assistant', content: JSON.stringify(answer) }, finish_reason: 'stop' }], usage: { prompt_tokens: 10, completion_tokens: 5, total_tokens: 15 } } };
}

/**
 * Starts the fake on 127.0.0.1: on `port`, else on the first free one of
 * `ports`, else on any free port. `proofToken` (default `token`; null for none)
 * is the token its /health proves, so another value plays a process that took
 * the port. `delayMs` holds every /v1/ reply back, and a client that leaves
 * cancels it. `setToken` changes both tokens at once, as Studio's Regenerate
 * does. Every request is recorded with its headers and parsed body.
 */
export async function startFakeStudio({ token, proofToken = token, port, ports = [0], delayMs = 0, images = realImage, chat = analystAnswer } = {}) {
  const state = { token, proofToken, delayMs };
  const requests = [];
  const server = createServer((req, res) => {
    let body = '';
    req.on('data', c => { body += c; });
    req.on('end', () => {
      const parsed = body ? JSON.parse(body) : null;
      requests.push({ method: req.method, url: req.url, headers: req.headers, body: parsed });
      const reply = (status, json) => { if (!res.writableEnded && !res.destroyed) { res.writeHead(status, { 'content-type': 'application/json' }); res.end(JSON.stringify(json)); } };
      const url = new URL(req.url, 'http://x');
      if (url.pathname === '/health') {
        const nonce = url.searchParams.get('nonce');
        const proof = nonce && state.proofToken ? createHmac('sha256', state.proofToken).update(`jai-health:${server.address().port}:${nonce}`).digest('base64url') : undefined;
        return reply(200, { status: 'ok', name: 'jai', version: '0.1.0', ...(proof ? { proof } : {}) });
      }
      if (req.headers.authorization !== `Bearer ${state.token}`) return reply(401, { error: { message: 'Send the JAI Studio token as `Authorization: Bearer <token>` (see `jai token`).', type: 'auth_error', code: 'unauthorized' } });
      const handler = url.pathname === '/v1/images/generations' ? images : url.pathname === '/v1/chat/completions' ? chat : null;
      if (!handler) return reply(404, { error: { message: 'no such route', code: 'not_found' } });
      const send = async () => { const r = await handler(parsed); reply(r.status ?? 200, r.json); };
      if (!state.delayMs) return void send();
      const timer = setTimeout(send, state.delayMs);
      res.on('close', () => clearTimeout(timer));
    });
  });
  const tries = port !== undefined ? [port] : ports;
  for (const p of tries) {
    const ok = await new Promise(resolve => {
      server.once('error', () => resolve(false));
      server.listen(p, '127.0.0.1', () => resolve(true));
    });
    if (ok) break;
  }
  if (!server.listening) throw new Error(`The fake JAI Studio could not listen on any of ${tries.join(', ')}`);
  const bound = server.address().port;
  return {
    port: bound,
    url: `http://127.0.0.1:${bound}`,
    requests,
    setToken: t => { state.token = t; state.proofToken = t; },
    setDelay: ms => { state.delayMs = ms; },
    close: () => new Promise(r => { server.closeAllConnections?.(); server.close(r); }),
  };
}
