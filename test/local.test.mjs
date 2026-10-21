// JAI Studio's local providers (OpenAIChatClient, LocalAnalyst, LocalRenderer)
// against a mocked local server: no network, no GPU. The mocked image model
// writes the prompt into the image bytes, and the mocked analyst reads the
// layout numbers back from them, so a full encode() really converges.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { mkdtempSync, readFileSync, writeFileSync } from 'node:fs';
import { createServer } from 'node:http';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import {
  OpenAIChatClient, LocalAnalyst, LocalRenderer, LOCAL_FLUX, STUDIO_PORTS, proveStudio, findStudio, FLUX_SIZES, REPLICATE_MODELS, base64ToBytes, encode, previewRatio,
  SYSTEM, inspectSystem, describePrompt, inspectTargetPrompt, inspectRenderPrompt, proposePrompt, describeObservation, TRAITS,
} from '../dist/encoder.mjs';
import { buildInput } from '../dist/test/replicate.js';
import { parse } from '../dist/jai.mjs';
import { startFakeStudio } from './fake-studio.mjs';

const BASE = 'http://127.0.0.1:5240';
const TOKEN = 'jai_test-token';
const TARGET = { url: 'data:image/jpeg;base64,/9j/4AAQSkZJRg==', width: 1200, height: 800 };
const PNG = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];
const JPEG = [0xff, 0xd8, 0xff, 0xe0];
const b64 = bytes => Buffer.from(bytes).toString('base64');
/** A fake image: a PNG or JPEG signature followed by the prompt, so the mock analyst can read it back. */
const imageBytes = (format, text) => new Uint8Array([...(format === 'jpeg' ? JPEG : PNG), ...new TextEncoder().encode(text)]);
const promptIn = dataURL => { const bytes = Buffer.from(dataURL.slice(dataURL.indexOf(',') + 1), 'base64'); return bytes.subarray(bytes[0] === 0xff ? 4 : 8).toString('utf8'); };

const abortError = () => Object.assign(new Error('This operation was aborted'), { name: 'AbortError' });
const waitAbort = signal => new Promise((_, reject) => signal.addEventListener('abort', () => reject(abortError())));

/** A mocked JAI Studio: routes by path, records every call with its parsed body. */
function studio({ images, chat } = {}) {
  const calls = [];
  const f = async (url, init = {}) => {
    const body = init.body ? JSON.parse(init.body) : null;
    calls.push({ url, method: init.method ?? 'GET', headers: init.headers ?? {}, body });
    const path = new URL(url).pathname;
    const handler = path === '/v1/images/generations' ? images : path === '/v1/chat/completions' ? chat : null;
    const reply = handler ? await handler(body, init) : { status: 404, json: { error: { message: 'no such route' } } };
    return new Response(reply.text ?? JSON.stringify(reply.json ?? {}), { status: reply.status ?? 200, headers: { 'content-type': 'application/json' } });
  };
  f.calls = calls;
  return f;
}
const chatReply = (content, { finish = 'stop', ...extra } = {}) => ({ json: { object: 'chat.completion', model: 'qwen3.8-27b', choices: [{ index: 0, message: { role: 'assistant', content }, finish_reason: finish }], usage: { prompt_tokens: 10, completion_tokens: 5, total_tokens: 15, prompt_tokens_details: { cached_tokens: 0 } }, timings: { prompt_ms: 1.5, predicted_ms: 2 }, ...extra } });
const answer = obj => chatReply(JSON.stringify(obj));
const imageReply = (body, seed = body.seed ?? 7) => ({ json: { created: 1, model: 'flux-schnell', width: body.width, height: body.height, steps: body.steps ?? 4, output_format: body.output_format, data: [{ b64_json: b64(imageBytes(body.output_format, body.prompt)), seed }], timings: { queueMs: 3, generateMs: 1500 } } });

// ---------------------------------------------------------------- OpenAIChatClient

test('chat client: POST /v1/chat/completions with the token; text, finish reason, usage and timings', async () => {
  const f = studio({ chat: () => chatReply('{"ok":true}') });
  const client = new OpenAIChatClient(`${BASE}/`, TOKEN, { fetch: f });
  const messages = [{ role: 'user', content: [{ type: 'text', text: 'hi' }] }];
  const r = await client.complete({ messages, max_tokens: 5, temperature: 0, top_k: 20, seed: 3, chat_template_kwargs: { enable_thinking: false } });
  assert.deepEqual(r, { text: '{"ok":true}', finishReason: 'stop', usage: { prompt_tokens: 10, completion_tokens: 5, total_tokens: 15 }, timings: { prompt_ms: 1.5, predicted_ms: 2 } });
  const [c] = f.calls;
  assert.equal(c.url, `${BASE}/v1/chat/completions`);
  assert.equal(c.method, 'POST');
  assert.deepEqual(c.headers, { Authorization: `Bearer ${TOKEN}`, 'Content-Type': 'application/json' });
  assert.deepEqual(c.body, { messages, max_tokens: 5, temperature: 0, top_k: 20, seed: 3, chat_template_kwargs: { enable_thinking: false } }, 'unset fields are not sent');
});

test('chat client: HTTP errors keep the server message and get a kind', async () => {
  for (const [status, kind] of [[401, 'auth'], [403, 'auth'], [400, 'refused'], [413, 'refused'], [409, 'provider'], [500, 'provider'], [503, 'provider'], [504, 'timeout']]) {
    const f = studio({ chat: () => ({ status, json: { error: { message: `reason ${status}`, type: 'x', code: `code_${status}` } } }) });
    await assert.rejects(new OpenAIChatClient(BASE, TOKEN, { fetch: f }).complete({ messages: [] }), e => {
      assert.equal(e.name, 'ProviderError');
      assert.equal(e.kind, kind, String(status));
      assert.equal(e.status, status);
      assert.equal(e.code, `code_${status}`);
      assert.match(e.message, new RegExp(`HTTP ${status}\\): reason ${status}`));
      return true;
    });
  }
  const html = studio({ chat: () => ({ status: 502, text: '<html> Bad   gateway </html>' }) });
  await assert.rejects(new OpenAIChatClient(BASE, TOKEN, { fetch: html }).complete({ messages: [] }), e => e.kind === 'provider' && /Bad gateway/.test(e.message));
});

test('chat client: malformed replies and refusals', async () => {
  const run = reply => new OpenAIChatClient(BASE, TOKEN, { fetch: studio({ chat: () => reply }) }).complete({ messages: [] });
  await assert.rejects(run({ json: { choices: [] } }), e => e.kind === 'provider' && /no choices/.test(e.message));
  await assert.rejects(run({ text: 'not json' }), e => e.kind === 'provider');
  await assert.rejects(run({ json: { choices: [{ message: { content: null, refusal: 'No.' } }] } }), e => e.kind === 'refused');
  const parts = await run({ json: { choices: [{ message: { content: [{ type: 'text', text: 'a' }, { type: 'text', text: 'b' }] } }] } });
  assert.deepEqual(parts, { text: 'ab', finishReason: null, usage: null, timings: null });
});

test('chat client: network failure, cancel and timeout', async () => {
  const refused = async () => { throw new TypeError('fetch failed', { cause: { code: 'ECONNREFUSED' } }); };
  await assert.rejects(new OpenAIChatClient(BASE, TOKEN, { fetch: refused }).complete({ messages: [] }), e => {
    assert.equal(e.kind, 'network');
    assert.equal(e.message, `Could not reach the local server at ${BASE} (ECONNREFUSED)`);
    assert.ok(!e.message.includes(TOKEN));
    return true;
  });
  const hanging = async (url, init) => waitAbort(init.signal);
  const ac = new AbortController();
  const p = new OpenAIChatClient(BASE, TOKEN, { fetch: hanging }).complete({ messages: [] }, { signal: ac.signal });
  ac.abort();
  await assert.rejects(p, e => e.kind === 'canceled');
  let called = false;
  await assert.rejects(new OpenAIChatClient(BASE, TOKEN, { fetch: async () => { called = true; } }).complete({ messages: [] }, { signal: ac.signal }), e => e.kind === 'canceled');
  assert.equal(called, false, 'an aborted signal sends nothing');
  await assert.rejects(new OpenAIChatClient(BASE, TOKEN, { fetch: hanging, timeoutMs: 20 }).complete({ messages: [] }), e => e.kind === 'timeout' && /timed out/.test(e.message));
});

test("chat client: the HTTP client's own time limits are timeouts, not network failures", async () => {
  for (const code of ['UND_ERR_HEADERS_TIMEOUT', 'UND_ERR_BODY_TIMEOUT']) {
    const undici = async () => { throw new TypeError('fetch failed', { cause: { code } }); };
    await assert.rejects(new OpenAIChatClient(BASE, TOKEN, { fetch: undici }).complete({ messages: [] }), e => {
      assert.equal(e.kind, 'timeout', code);
      assert.equal(e.message, `Chat completion timed out: the HTTP client stopped waiting for the server (${code})`);
      return true;
    });
  }
  const renderer = new LocalRenderer(BASE, TOKEN, { fetch: async () => { throw new TypeError('fetch failed', { cause: { code: 'UND_ERR_HEADERS_TIMEOUT' } }); } });
  await assert.rejects(renderer.render({ intent: { description: 'A castle', style: [], avoid: [], aspect: null, mode: 'generate', strength: null, seed: null }, modelId: LOCAL_FLUX.id, inputImage: null, governingRatio: null, target: null }), e => e.kind === 'timeout' && /^Image generation timed out/.test(e.message));
});

// ---------------------------------------------------------------- LocalAnalyst

const DRAFTS = [
  { prompt: 'Photograph of a red bicycle\nleaning on a wall', style: ['35mm film photo'], avoid: [] },
  { prompt: 'Photograph of a bicycle, red, by a white wall', style: ['photo', 'film', 'grain', 'extra'], avoid: ['text'] },
];
const analystWith = (handler, opts) => {
  const f = studio({ chat: handler });
  return { f, analyst: new LocalAnalyst(new OpenAIChatClient(BASE, TOKEN, { fetch: f }), opts) };
};
const sentSchema = call => call.body.response_format.json_schema.schema;
/** Writing sampling; max_tokens covers the longest answer the schema allows (see the max_tokens test). */
const WRITING = { temperature: 0.7, top_p: 0.8, top_k: 20, presence_penalty: 0.5 };
const DRAFT_TOKENS = Math.ceil((1200 + 5 * 80) / 3) + 60, ADDITION_TOKENS = Math.ceil(300 / 3) + 60, NOTES_TOKENS = Math.ceil(600 / 3) + 60;
const sampling = body => Object.fromEntries(['temperature', 'top_p', 'top_k', 'presence_penalty', 'max_tokens'].filter(k => k in body).map(k => [k, body[k]]));

test('analyst describe: the system prompt, one data: URL image, exactly n drafts, thinking off, writing sampling', async () => {
  const { f, analyst } = analystWith(() => answer({ notes: 'The haze is hardest.', drafts: DRAFTS }));
  assert.equal(analyst.id, 'local/qwen3.8-27b');
  assert.equal(analyst.costHint, 0);
  const r = await analyst.describe(TARGET, 2, { models: ['FLUX.1 schnell'], maxWords: 120 });
  assert.deepEqual(r, { value: [{ prompt: 'Photograph of a red bicycle leaning on a wall', style: ['35mm film photo'], avoid: [] }, { prompt: 'Photograph of a bicycle, red, by a white wall', style: ['photo', 'film', 'grain'], avoid: ['text'] }], notes: 'The haze is hardest.', cost: 0 });
  const { body } = f.calls[0];
  assert.deepEqual(body.messages, [
    { role: 'system', content: SYSTEM },
    { role: 'user', content: [{ type: 'text', text: describePrompt(2, ['FLUX.1 schnell'], 120) }, { type: 'image_url', image_url: { url: TARGET.url } }] },
  ]);
  assert.equal(body.response_format.type, 'json_schema');
  assert.equal(body.response_format.json_schema.name, 'jai_drafts');
  assert.equal(body.response_format.json_schema.strict, true);
  assert.deepEqual(sentSchema(f.calls[0]), {
    type: 'object',
    properties: {
      notes: { type: 'string', maxLength: 600 },
      drafts: {
        type: 'array', minItems: 2, maxItems: 2,
        items: {
          type: 'object',
          properties: {
            prompt: { type: 'string', minLength: 1, maxLength: 1200 },
            style: { type: 'array', items: { type: 'string', maxLength: 80 }, maxItems: 3 },
            avoid: { type: 'array', items: { type: 'string', maxLength: 80 }, maxItems: 2 },
          },
          required: ['prompt', 'style', 'avoid'], additionalProperties: false,
        },
      },
    },
    required: ['notes', 'drafts'], additionalProperties: false,
  });
  assert.deepEqual(body.chat_template_kwargs, { enable_thinking: false });
  assert.deepEqual(sampling(body), { ...WRITING, max_tokens: 2 * DRAFT_TOKENS + NOTES_TOKENS });
});

const OBS_ANSWER = { horizon_y: 805, horizon_shape: 'flat', camera: 'wide, eye level', lighting: 'low sun', medium: 'photograph' };
/** The people list as the grammar fixes it: a box, the face, then every trait as one of its words or null. */
const PEOPLE = max => ({
  type: 'array', maxItems: max,
  items: {
    type: 'object',
    properties: { bbox_2d: { type: 'array', items: { type: 'integer', minimum: 0, maximum: 1000 }, minItems: 4, maxItems: 4 }, face: { enum: ['clear', 'small', 'hidden', null] }, stands_out: { type: 'string', maxLength: 80 }, ...Object.fromEntries(TRAITS.map(t => [t.key, { enum: [...t.values, null] }])) },
    required: ['bbox_2d', 'face', 'stands_out', ...TRAITS.map(t => t.key)], additionalProperties: false,
  },
});

test('analyst inspect (target): up to 5 subjects and the people, integer boxes and listed words, greedy, never thinking', async () => {
  const { f, analyst } = analystWith(() => answer({ subjects: [{ name: 'red bicycle', bbox_2d: [100, 400, 500, 900] }], ...OBS_ANSWER }), { thinking: 'all' });
  const r = await analyst.inspect(TARGET, null, { models: ['FLUX.1 schnell'], careful: true });
  assert.deepEqual(r.value, { subjects: [{ name: 'red bicycle', box: { ymin: 0.4, xmin: 0.1, ymax: 0.9, xmax: 0.5 } }], people: [], crowd: null, horizon: 0.805, horizonShape: 'flat', camera: 'wide, eye level', lighting: 'low sun', medium: 'photograph' });
  assert.equal(r.cost, 0);
  const { body } = f.calls[0];
  assert.equal(body.messages[0].content, inspectSystem('xy'), 'boxes in Qwen\'s own [x1, y1, x2, y2] order');
  assert.equal(body.messages[1].content[0].text, inspectTargetPrompt('xy'));
  assert.equal(body.response_format.json_schema.name, 'jai_target_inspection');
  const coord = { type: 'integer', minimum: 0, maximum: 1000 };
  const box = { type: 'array', items: coord, minItems: 4, maxItems: 4 };
  const scene = { horizon_y: { anyOf: [coord, { type: 'null' }] }, horizon_shape: { enum: ['flat', 'curved', null] }, camera: { type: 'string', maxLength: 120 }, lighting: { type: 'string', maxLength: 120 }, medium: { type: 'string', maxLength: 120 } };
  assert.deepEqual(sentSchema(f.calls[0]), {
    type: 'object',
    properties: { subjects: { type: 'array', items: { type: 'object', properties: { name: { type: 'string', minLength: 1, maxLength: 60 }, bbox_2d: box }, required: ['name', 'bbox_2d'], additionalProperties: false }, maxItems: 5 }, people: PEOPLE(5), crowd: { enum: ['none', 'few', 'some', 'many', null] }, ...scene },
    required: ['subjects', 'people', 'crowd', 'horizon_y', 'horizon_shape', 'camera', 'lighting', 'medium'], additionalProperties: false,
  });
  assert.deepEqual(body.chat_template_kwargs, { enable_thinking: false });
  assert.deepEqual(sampling(body), { temperature: 0, max_tokens: 4096 });
});

test('analyst inspect (render): one entry per given subject, named from the list in any order, boxes may be null; up to 6 people', async () => {
  const { f, analyst } = analystWith(() => answer({ subjects: [{ name: 'red bicycle', bbox_2d: [120, 410, 520, 880] }, { name: 'white wall', bbox_2d: null }], ...OBS_ANSWER, horizon_y: null, horizon_shape: null }));
  const subjects = ['red bicycle', 'white wall'];
  const r = await analyst.inspect({ url: 'data:image/png;base64,iVBORw0KGgo=' }, subjects, { models: ['FLUX.1 schnell'] });
  assert.deepEqual(r.value.subjects, [{ name: 'red bicycle', box: { ymin: 0.41, xmin: 0.12, ymax: 0.88, xmax: 0.52 } }, { name: 'white wall', box: null }]);
  assert.equal(r.value.horizon, null);
  const { body } = f.calls[0];
  assert.equal(body.messages[1].content[1].image_url.url, 'data:image/png;base64,iVBORw0KGgo=');
  assert.equal(body.messages[1].content[0].text, inspectRenderPrompt(subjects, 'xy'), 'the task text comes before the image');
  assert.equal(body.response_format.json_schema.name, 'jai_render_inspection');
  const list = sentSchema(f.calls[0]).properties.subjects;
  assert.equal('prefixItems' in list, false, 'no fixed order: Qwen wrote boxes in its own order under the wrong names');
  assert.equal(list.minItems, 2);
  assert.equal(list.maxItems, 2);
  assert.deepEqual(list.items.properties.name, { type: 'string', enum: ['red bicycle', 'white wall'] });
  assert.deepEqual(list.items.properties.bbox_2d, { anyOf: [{ type: 'array', items: { type: 'integer', minimum: 0, maximum: 1000 }, minItems: 4, maxItems: 4 }, { type: 'null' }] });
  assert.deepEqual(sentSchema(f.calls[0]).properties.people, PEOPLE(6), 'a render lists one more person than the target, down to a twentieth of the frame');
  // Answered in another order, each box still lands on its subject.
  const swapped = analystWith(() => answer({ subjects: [{ name: 'white wall', bbox_2d: [0, 0, 600, 1000] }, { name: 'red bicycle', bbox_2d: [120, 410, 520, 880] }], ...OBS_ANSWER }));
  const r2 = await swapped.analyst.inspect({ url: 'data:image/png;base64,iVBORw0KGgo=' }, subjects, { models: ['FLUX.1 schnell'] });
  assert.deepEqual(r2.value.subjects.map(s => [s.name, s.box?.ymin]), [['red bicycle', 0.41], ['white wall', 0]]);
  assert.deepEqual(sampling(body), { temperature: 0, max_tokens: 4096 });
  // No subjects with boxes in the target: an empty list.
  const none = analystWith(() => answer({ subjects: [], ...OBS_ANSWER }));
  await none.analyst.inspect(TARGET, [], { models: [] });
  const empty = sentSchema(none.f.calls[0]).properties.subjects;
  assert.equal(empty.maxItems, 0);
  assert.equal('prefixItems' in empty, false);
});

const TARGET_OBS = { subjects: [{ name: 'red bicycle', box: { ymin: 0.4, xmin: 0.1, ymax: 0.9, xmax: 0.5 } }], people: [], crowd: null, horizon: 0.8, horizonShape: 'flat', camera: 'wide', lighting: 'sun', medium: 'photograph' };
const history = [{ draft: { prompt: 'Photograph of a red bicycle', style: ['photo'], avoid: [] }, fitness: 0.61, feedback: { 'FLUX.1 schnell': 'Largest first: red bicycle: 1.4× too wide' } }];
const proposeCtx = (scope, n = 3) => ({ models: ['FLUX.1 schnell'], maxWords: 120, targetObservation: TARGET_OBS, history, notes: 'earlier notes', n, scope });

test('analyst propose: drafts for the core, additions for a refinement, each exactly n', async () => {
  const { f, analyst } = analystWith(body => (body.response_format.json_schema.name === 'jai_additions'
    ? answer({ notes: 'wider', additions: ['The bicycle is small.', 'It stands left of centre.'] })
    : answer({ notes: 'tighter framing', drafts: DRAFTS })));
  const core = await analyst.propose(TARGET, proposeCtx({ kind: 'core' }, 2));
  assert.equal(core.value.length, 2);
  assert.equal(core.notes, 'tighter framing');
  const c0 = f.calls[0].body;
  assert.equal(c0.messages[0].content, SYSTEM);
  assert.equal(c0.messages[1].content[0].text, proposePrompt(proposeCtx({ kind: 'core' }, 2), describeObservation(TARGET_OBS)));
  assert.equal(sentSchema(f.calls[0]).properties.drafts.minItems, 2);
  assert.deepEqual(sampling(c0), { ...WRITING, max_tokens: 2 * DRAFT_TOKENS + NOTES_TOKENS });
  const scope = { kind: 'refine', model: 'FLUX.1 schnell', core: history[0].draft };
  const refine = await analyst.propose(TARGET, proposeCtx(scope, 2));
  assert.deepEqual(refine.value, ['The bicycle is small.', 'It stands left of centre.']);
  assert.deepEqual(sentSchema(f.calls[1]), {
    type: 'object',
    properties: { notes: { type: 'string', maxLength: 600 }, additions: { type: 'array', items: { type: 'string', minLength: 1, maxLength: 300 }, minItems: 2, maxItems: 2 } },
    required: ['notes', 'additions'], additionalProperties: false,
  });
  assert.deepEqual(sampling(f.calls[1].body), { ...WRITING, max_tokens: 2 * ADDITION_TOKENS + NOTES_TOKENS });
});

test('analyst max_tokens: room for the longest answer the schema allows; with thinking, the server limit', async () => {
  const longest = n => ({ notes: 'n'.repeat(600), drafts: Array.from({ length: n }, () => ({ prompt: 'p'.repeat(1200), style: ['s'.repeat(80), 's'.repeat(80), 's'.repeat(80)], avoid: ['a'.repeat(80), 'a'.repeat(80)] })) });
  for (let n = 1; n <= 9; n++) {
    const { f, analyst } = analystWith(() => answer(longest(n)));
    await analyst.describe(TARGET, n, { models: [] });
    await analyst.propose(TARGET, proposeCtx({ kind: 'core' }, n));
    for (const c of f.calls) {
      // Even at 3 characters a token (English text takes more), the longest valid answer fits.
      assert.ok(c.body.max_tokens >= JSON.stringify(longest(n), null, 1).length / 3, `n=${n}: ${c.body.max_tokens}`);
      assert.ok(c.body.max_tokens <= 8192, 'JAI Studio clamps to 8192');
    }
  }
  // Reasoning counts against max_tokens and has no known length.
  const think = async thinking => {
    const { f, analyst } = analystWith(body => (body.response_format.json_schema.name.includes('inspection') ? answer({ subjects: [], ...OBS_ANSWER }) : answer({ notes: '', drafts: DRAFTS })), { thinking });
    await analyst.describe(TARGET, 2, { models: [] });
    await analyst.inspect(TARGET, null, { models: [] });
    await analyst.propose(TARGET, proposeCtx({ kind: 'core' }, 2));
    return f.calls.map(c => c.body.max_tokens);
  };
  assert.deepEqual(await think('propose'), [2 * DRAFT_TOKENS + NOTES_TOKENS, 4096, 8192]);
  assert.deepEqual(await think('all'), [8192, 4096, 8192]);
  // The longest inspection: five subjects, six people with the longest word of every trait, and the scene.
  const longestWord = values => values.reduce((a, v) => (v.length > a.length ? v : a), '');
  const person = { bbox_2d: [1000, 1000, 1000, 1000], face: 'hidden', stands_out: 'x'.repeat(80), ...Object.fromEntries(TRAITS.map(t => [t.key, longestWord(t.values)])) };
  const inspection = { subjects: Array(5).fill({ name: 'x'.repeat(60), bbox_2d: [1000, 1000, 1000, 1000] }), people: Array(6).fill(person), crowd: 'some', ...OBS_ANSWER, camera: 'x'.repeat(120), lighting: 'x'.repeat(120), medium: 'x'.repeat(120) };
  // Pessimistic: every digit its own token, the rest at 3 characters a token, indented.
  const text = JSON.stringify(inspection, null, 1), digits = text.replace(/\D/g, '').length;
  assert.ok(4096 >= digits + (text.length - digits) / 3, String(digits + (text.length - digits) / 3));
});

test('analyst thinking: off by default; propose, or describe and propose; inspection never', async () => {
  const handler = body => (body.response_format.json_schema.name.includes('inspection') ? answer({ subjects: [], ...OBS_ANSWER }) : answer({ notes: '', drafts: DRAFTS }));
  const flags = async thinking => {
    const { f, analyst } = analystWith(handler, thinking ? { thinking } : undefined);
    await analyst.describe(TARGET, 2, { models: [] });
    await analyst.inspect(TARGET, null, { models: [] });
    await analyst.propose(TARGET, proposeCtx({ kind: 'core' }, 2));
    return f.calls.map(c => c.body.chat_template_kwargs.enable_thinking);
  };
  assert.deepEqual(await flags(undefined), [false, false, false]);
  assert.deepEqual(await flags('off'), [false, false, false]);
  assert.deepEqual(await flags('propose'), [false, false, true]);
  assert.deepEqual(await flags('all'), [true, false, true]);
  assert.deepEqual(await flags('sometimes'), [false, false, false], 'unknown values mean off');
});

test('analyst answers: a leading reasoning block is skipped; a cut-off answer says so', async () => {
  const thinking = analystWith(() => chatReply(`<think>Maybe {"drafts": []}? No.</think>\n${JSON.stringify({ notes: 'n', drafts: DRAFTS.slice(0, 1) })}`));
  assert.equal((await thinking.analyst.describe(TARGET, 1, { models: [] })).value.length, 1);
  const cut = analystWith(() => chatReply('{"notes": "n", "drafts": [{"prompt": "Photograph of', { finish: 'length' }));
  await assert.rejects(cut.analyst.describe(TARGET, 1, { models: [] }), new RegExp(`cut off at ${DRAFT_TOKENS + NOTES_TOKENS} tokens`));
  const chatter = analystWith(() => chatReply('I cannot see an image.'));
  await assert.rejects(chatter.analyst.describe(TARGET, 1, { models: [] }), /no JSON/);
});

// ---------------------------------------------------------------- LocalRenderer

const intent = (o = {}) => ({ description: 'A castle', style: [], avoid: [], aspect: null, mode: 'generate', strength: null, seed: null, ...o });
const req = (o = {}) => ({ intent: intent(o.intent), modelId: o.modelId ?? 'black-forest-labs/flux-schnell', inputImage: o.inputImage ?? null, governingRatio: o.governingRatio ?? null, previewRatio: o.previewRatio ?? null, target: null, signal: o.signal });
const codes = r => r.diagnostics.map(d => d.code + (d.key ? ':' + d.key : ''));
const rendererWith = (opts = {}, images = body => imageReply(body)) => {
  const f = studio({ images });
  return { f, renderer: new LocalRenderer(BASE, TOKEN, { fetch: f, ...opts }) };
};

test('LOCAL_FLUX is the Replicate flux-schnell model at no cost; FLUX_SIZES are multiples of 64', () => {
  const { id, label, family, aliases, imageInput, negativePrompt, shortWindow, description } = REPLICATE_MODELS.find(m => m.id === 'black-forest-labs/flux-schnell');
  assert.deepEqual(LOCAL_FLUX, { id, label, family, aliases, imageInput, negativePrompt, shortWindow, costHint: 0, description });
  const { renderer } = rendererWith();
  assert.equal(renderer.id, 'local');
  assert.equal(renderer.label, 'JAI Studio');
  assert.deepEqual(renderer.models(), [LOCAL_FLUX]);
  assert.deepEqual(Object.keys(FLUX_SIZES), ['1:1', '16:9', '21:9', '3:2', '2:3', '4:5', '5:4', '3:4', '4:3', '9:16', '9:21']);
  for (const [k, [w, h]] of Object.entries(FLUX_SIZES)) {
    assert.ok(w % 64 === 0 && h % 64 === 0 && w * h <= 1536 * 1536, k);
    const [a, b] = k.split(':').map(Number);
    assert.ok(Math.abs(Math.log(w / h / (a / b))) < 0.05, k);
  }
});

test('render: POST /v1/images/generations with the token; the result carries the bytes, a data: URL and what was sent', async () => {
  const { f, renderer } = rendererWith({ steps: 6 });
  const r = await renderer.render(req({ intent: { seed: '42', style: ['watercolour'] } }));
  const [c] = f.calls;
  assert.equal(c.url, `${BASE}/v1/images/generations`);
  assert.equal(c.method, 'POST');
  assert.deepEqual(c.headers, { Authorization: `Bearer ${TOKEN}`, 'Content-Type': 'application/json' });
  assert.deepEqual(c.body, { prompt: 'A castle\n\nStyle: watercolour.', width: 1024, height: 1024, output_format: 'png', seed: 42, steps: 6 });
  const sentB64 = b64(imageBytes('png', c.body.prompt));
  assert.deepEqual([...r.bytes], [...Buffer.from(sentB64, 'base64')]);
  assert.equal(r.mime, 'image/png');
  assert.equal(r.outputURL, `data:image/png;base64,${sentB64}`);
  assert.equal(r.model, 'black-forest-labs/flux-schnell');
  assert.equal(r.sentPrompt, 'A castle\n\nStyle: watercolour.');
  assert.equal(r.sentNegative, null);
  assert.equal(r.jobId, null);
  assert.equal(r.predictSeconds, 1.5);
  assert.deepEqual(r.sentInput, { prompt: 'A castle\n\nStyle: watercolour.', width: 1024, height: 1024, seed: 42, steps: 6, output_format: 'png' });
  assert.deepEqual(r.diagnostics, []);
});

test('render: the prompt and its diagnostics are what the Replicate renderer would produce', async () => {
  const schnell = REPLICATE_MODELS.find(m => m.id === 'black-forest-labs/flux-schnell');
  const cases = [
    { style: ['watercolor', 'loose'], avoid: ['text'], aspect: { width: 16, height: 10 } },
    { description: '', style: ['anime'] },
    { avoid: ['people', 'cars'], seed: '12345678901234567890' },
    { description: 'Line one\nLine two', aspect: { width: 21, height: 9 } },
  ];
  for (const it of cases) {
    const { f, renderer } = rendererWith();
    const r = await renderer.render(req({ intent: it }));
    const expected = buildInput(schnell, req({ intent: it }), { foldAvoid: true });
    assert.equal(f.calls[0].body.prompt, expected.prompt);
    assert.deepEqual(codes(r).sort(), codes(expected).sort(), JSON.stringify(it));
  }
});

test("render: aspect snaps to the nearest FLUX size; the governing ratio wins without a report; else the preview's ratio, else 1:1", async () => {
  const size = async (aspect, governingRatio = null, previewRatio = null) => {
    const { f, renderer } = rendererWith();
    const r = await renderer.render(req({ intent: { aspect }, governingRatio, previewRatio }));
    return [f.calls[0].body.width, f.calls[0].body.height, codes(r).includes('adjusted-value:aspect')];
  };
  assert.deepEqual(await size(null), [1024, 1024, false]);
  assert.deepEqual(await size({ width: 16, height: 9 }), [1344, 768, false]);
  assert.deepEqual(await size({ width: 16, height: 10 }), [1216, 832, true]);
  assert.deepEqual(await size({ width: 9, height: 21 }), [640, 1536, false]);
  assert.deepEqual(await size({ width: 4, height: 5 }), [896, 1088, false]);
  assert.deepEqual(await size({ width: 100, height: 1 }), [1536, 640, true]);
  assert.deepEqual(await size({ width: 1, height: 1 }, 1.5), [1216, 832, false]);
  assert.deepEqual(await size(null, 0.75), [896, 1152, false]);
  assert.deepEqual(await size(null, null, 16 / 9), [1344, 768, false]);
  assert.deepEqual(await size(null, null, 1.6), [1216, 832, false], 'a snapped preview ratio is no adjusted aspect');
  assert.deepEqual(await size({ width: 1, height: 1 }, null, 16 / 9), [1024, 1024, false]);
  assert.deepEqual(await size(null, 0.75, 16 / 9), [896, 1152, false]);
  const { renderer } = rendererWith();
  const r = await renderer.render(req({ intent: { aspect: { width: 16, height: 10 } } }));
  assert.deepEqual(r.diagnostics.find(d => d.key === 'aspect'), { code: 'adjusted-value', key: 'aspect', requested: '16:10', actual: '3:2' });
});

test('render: scale multiplies the size, in steps of 64 from 256, within the server limits', async () => {
  const size = async (scale, aspect = null) => {
    const { f, renderer } = rendererWith({ scale });
    await renderer.render(req({ intent: { aspect } }));
    return [f.calls[0].body.width, f.calls[0].body.height];
  };
  assert.deepEqual(await size(0.5), [512, 512]);
  assert.deepEqual(await size(0.1), [256, 256]);
  assert.deepEqual(await size(1.5, { width: 16, height: 9 }), [2048, 1152]);
  assert.deepEqual(await size(2), [1536, 1536], 'at most 2,359,296 pixels');
  assert.deepEqual(await size(3, { width: 21, height: 9 }), [1984, 832], 'at most 2048 a side, at the ratio');
  assert.deepEqual(await size(Number.NaN), [1024, 1024]);
  assert.deepEqual(await size(-1), [1024, 1024]);
  // Small sizes keep their ratio: rounding each side to 64 by itself made 16:9 at 0.25 320x256.
  assert.deepEqual(await size(0.25, { width: 16, height: 9 }), [448, 256]);
  assert.deepEqual(await size(0.25, { width: 21, height: 9 }), [640, 256]);
  assert.deepEqual(await size(0.25, { width: 4, height: 5 }), [256, 320]);
  assert.deepEqual(await size(0.4, { width: 3, height: 4 }), [384, 512]);
});

test('render: at every scale, every FLUX size keeps its ratio within 0.05 and the server limits; scale 1 is the FLUX size', async () => {
  for (const [key, [bw, bh]] of Object.entries(FLUX_SIZES)) {
    const [a, b] = key.split(':').map(Number);
    for (let k = 1; k <= 60; k++) {
      const scale = k * 0.05;
      const { f, renderer } = rendererWith({ scale });
      const r = await renderer.render(req({ intent: { aspect: { width: a, height: b } } }));
      const { width: w, height: h } = f.calls[0].body;
      assert.ok(w % 64 === 0 && h % 64 === 0 && w >= 256 && h >= 256 && w <= 2048 && h <= 2048 && w * h <= 2359296, `${key} at ${scale}: ${w}x${h}`);
      assert.ok(Math.abs(Math.log(w / h / (bw / bh))) <= 0.05, `${key} at ${scale}: ${w}x${h}`);
      if (k === 20) assert.deepEqual([w, h], [bw, bh], key);
      // The scaled size as nearly as the ratio allows: the short side within a 64-pixel step of the scaled one (or of 256).
      if (scale <= 1.25) assert.ok(Math.abs(Math.min(w, h) - Math.max(256, Math.min(bw, bh) * scale)) <= 64, `${key} at ${scale}: ${w}x${h}`);
      assert.deepEqual(codes(r), [], `${key} at ${scale}`);
    }
  }
});

test('render: seeds reduce modulo 2^32 with a report; with no seed the server picks and the result reports it', async () => {
  const seedOf = async seed => {
    const { f, renderer } = rendererWith();
    const r = await renderer.render(req({ intent: { seed } }));
    return { sent: f.calls[0].body.seed, reported: r.sentInput.seed, diag: r.diagnostics.find(d => d.key === 'seed') ?? null };
  };
  assert.deepEqual(await seedOf('4294967295'), { sent: 4294967295, reported: 4294967295, diag: null });
  assert.deepEqual(await seedOf('4294967296'), { sent: 0, reported: 0, diag: { code: 'adjusted-value', key: 'seed', requested: '4294967296', actual: '0' } });
  const big = Number(12345678901234567890n % 4294967296n);
  assert.deepEqual(await seedOf('12345678901234567890'), { sent: big, reported: big, diag: { code: 'adjusted-value', key: 'seed', requested: '12345678901234567890', actual: String(big) } });
  const { f, renderer } = rendererWith({}, body => imageReply(body, 3141592653));
  const r = await renderer.render(req());
  assert.equal('seed' in f.calls[0].body, false);
  assert.equal(r.sentInput.seed, 3141592653);
  await assert.rejects(rendererWith().renderer.render(req({ intent: { seed: '-5' } })), e => e.kind === 'config');
});

test('render: transform and avoid are reported as the Replicate renderer reports them', async () => {
  const run = async o => codes(await rendererWith().renderer.render(req(o)));
  assert.deepEqual(await run({ intent: { mode: 'transform', strength: 0.4 } }), ['original-not-readable:mode']);
  assert.deepEqual(await run({ intent: { mode: 'transform' }, inputImage: { bytes: new Uint8Array([1]), mime: 'image/png' } }), ['mode-degraded:mode']);
  assert.deepEqual(await run({ intent: { avoid: ['text'] } }), ['folded-into-prompt:avoid']);
});

test('render: JPEG output, a long description shortened, nothing to render refused', async () => {
  const jpeg = rendererWith({ outputFormat: 'jpeg' });
  const r = await jpeg.renderer.render(req());
  assert.equal(jpeg.f.calls[0].body.output_format, 'jpeg');
  assert.equal(r.mime, 'image/jpeg');
  assert.ok(r.outputURL.startsWith('data:image/jpeg;base64,/9j/'));
  const long = rendererWith();
  const lr = await long.renderer.render(req({ intent: { description: 'castle '.repeat(1500), style: ['ink'] } }));
  assert.ok(long.f.calls[0].body.prompt.length <= 8000);
  assert.ok(long.f.calls[0].body.prompt.endsWith('Style: ink.'), 'description text goes before any style value');
  assert.ok(codes(lr).includes('adjusted-value:prompt'));
  const empty = rendererWith();
  await assert.rejects(empty.renderer.render(req({ intent: { description: '' } })), e => e.kind === 'refused');
  assert.equal(empty.f.calls.length, 0);
});

test('render: errors become RenderErrors with a kind', async () => {
  const fail = (reply, o) => rendererWith({}, () => reply).renderer.render(req(o));
  await assert.rejects(fail({ status: 409, json: { error: { message: 'Download the models first: open JAI Studio or run `jai models download`.', type: 'invalid_request_error', code: 'models_missing' } } }), e => e.kind === 'provider' && /HTTP 409\): Download the models first/.test(e.message));
  await assert.rejects(fail({ status: 401, json: { error: { message: 'Missing or bad token', code: 'unauthorized' } } }), e => e.kind === 'auth');
  await assert.rejects(fail({ status: 400, json: { error: { message: 'bad size' } } }), e => e.kind === 'refused');
  await assert.rejects(fail({ json: { data: [] } }), e => e.kind === 'provider' && /no image/.test(e.message));
  await assert.rejects(fail({ json: { data: [{ b64_json: b64(new TextEncoder().encode('<html>')) }] } }), e => e.kind === 'provider' && /not an image/.test(e.message));
  await assert.rejects(fail({ json: { data: [{ b64_json: '!!!!' }] } }), e => e.kind === 'provider' && /base64/.test(e.message));
  await assert.rejects(rendererWith().renderer.render(req({ modelId: 'stability-ai/sdxl' })), e => e.kind === 'config');
  assert.equal((await rendererWith().renderer.render(req({ modelId: 'flux-schnell' }))).model, LOCAL_FLUX.id);
  const ac = new AbortController();
  const p = rendererWith({}, (body, init) => waitAbort(init.signal)).renderer.render(req({ signal: ac.signal }));
  ac.abort();
  await assert.rejects(p, e => e.kind === 'canceled');
});

test("previewRatio: a preview's size from its header, for every raster type a preview may have", async () => {
  const sharp = (await import('sharp')).default;
  const picture = (width, height, alpha = false) => sharp({ create: { width, height, channels: alpha ? 4 : 3, background: { r: 40, g: 90, b: 200, alpha: 0.5 } } });
  const url = (mime, bytes) => `data:${mime};base64,${Buffer.from(bytes).toString('base64')}`;
  const cases = [
    ['image/png', await picture(16, 9).png().toBuffer(), 16 / 9],
    ['image/jpeg', await picture(9, 16).jpeg().toBuffer(), 9 / 16],
    ['image/jpeg', await picture(30, 20).jpeg({ progressive: true }).withMetadata().toBuffer(), 3 / 2],
    ['image/webp', await picture(8, 6).webp().toBuffer(), 4 / 3],
    ['image/webp', await picture(5, 4).webp({ lossless: true }).toBuffer(), 5 / 4],
    ['image/webp', await picture(7, 3, true).webp().toBuffer(), 7 / 3],
    ['image/gif', await picture(3, 4).gif().toBuffer(), 3 / 4],
    ['image/avif', await picture(64, 32).avif().toBuffer(), 2],
  ];
  for (const [mime, bytes, ratio] of cases) assert.equal(previewRatio(url(mime, bytes)), ratio, `${mime} ${bytes.length} bytes`);
  assert.equal(new TextDecoder().decode(cases[5][1].subarray(12, 16)), 'VP8X', 'an extended WebP');
  // Percent-encoded as well as base64; the data: URL's own type does not matter, the bytes do.
  assert.equal(previewRatio(`data:image/webp,${[...cases[0][1]].map(b => `%${b.toString(16).padStart(2, '0')}`).join('')}`), 16 / 9);
  // Nothing to read: no preview, a header cut short, not an image, or not a data: URL (never fetched).
  for (const v of [null, '', url('image/png', cases[0][1].subarray(0, 20)), url('image/jpeg', cases[1][1].subarray(0, 40)), url('image/png', new TextEncoder().encode('A castle')), 'https://example.com/p.png'])
    assert.equal(previewRatio(v), null, String(v).slice(0, 60));
});

test('base64ToBytes: standard base64 with optional padding and whitespace; anything else throws', () => {
  for (let n = 0; n < 40; n++) {
    const bytes = Uint8Array.from({ length: n }, (_, i) => (i * 97 + n * 13) & 0xff);
    assert.deepEqual([...base64ToBytes(b64(bytes))], [...bytes], `length ${n}`);
    assert.deepEqual([...base64ToBytes(b64(bytes).replace(/=+$/, ''))], [...bytes], `unpadded ${n}`);
  }
  const big = Uint8Array.from({ length: 3 * 1024 * 1024 + 1 }, (_, i) => (i * 31 + (i >> 8)) & 0xff);
  assert.ok(Buffer.from(base64ToBytes(b64(big))).equals(Buffer.from(big)));
  assert.deepEqual([...base64ToBytes('QUJD\nREVG\r\n')], [...new TextEncoder().encode('ABCDEF')]);
  assert.deepEqual([...base64ToBytes(' Q UJ D ')], [...new TextEncoder().encode('ABC')]);
  for (const bad of ['Q', 'QQ=A', '@@@@', 'QUJD-_', 'QUJDé===', 'QQ===']) assert.throws(() => base64ToBytes(bad), RangeError, bad);
});

// ---------------------------------------------------------------- encode()

const LAYOUT = /w=([0-9.]+) cx=([0-9.]+) cy=([0-9.]+) hz=([0-9.]+)/;
const TRUE = { w: 0.15, cx: 0.565, cy: 0.515, hz: 0.8 };
const layoutOf = s => { const m = LAYOUT.exec(s); return { w: +m[1], cx: +m[2], cy: +m[3], hz: +m[4] }; };
const draftOf = p => ({ prompt: `Photograph of Earth over the Moon; w=${p.w.toFixed(3)} cx=${p.cx.toFixed(3)} cy=${p.cy.toFixed(3)} hz=${p.hz.toFixed(3)}`, style: ['photo'], avoid: [] });
const SCENE_ANSWER = { horizon_shape: 'flat', camera: 'wide', lighting: 'sun', medium: 'photograph' };

/** The mocked analyst: it measures the target as TRUE, reads a render's layout from its bytes, and steps proposals halfway to TRUE. */
function mockQwen(body) {
  const { name, schema } = body.response_format.json_schema;
  const task = body.messages[1].content.find(p => p.type === 'text');
  const image = body.messages[1].content.find(p => p.type === 'image_url');
  if (name === 'jai_target_inspection') return answer({ subjects: [{ name: 'Earth', bbox_2d: [490, 460, 640, 570] }], horizon_y: 800, ...SCENE_ANSWER });
  if (name === 'jai_render_inspection') {
    const p = layoutOf(promptIn(image.image_url.url));
    const h = (p.w * 11) / 15;
    return answer({ subjects: [{ name: 'Earth', bbox_2d: [p.cx - p.w / 2, p.cy - h / 2, p.cx + p.w / 2, p.cy + h / 2].map(v => Math.round(v * 1000)) }], horizon_y: Math.round(p.hz * 1000), ...SCENE_ANSWER });
  }
  const n = schema.properties.drafts.minItems;
  if (task.text.includes('Earlier attempts')) {
    const best = layoutOf(task.text.slice(task.text.indexOf('#1 score')));
    const next = Object.fromEntries(Object.entries(best).map(([k, v]) => [k, v + (TRUE[k] - v) * 0.5]));
    return answer({ notes: 'closer', drafts: Array.from({ length: n }, (_, i) => draftOf({ ...next, w: next.w + (i - 1) * 0.004 })) });
  }
  return answer({ notes: 'first', drafts: Array.from({ length: n }, (_, i) => draftOf({ w: 0.4 + i * 0.01, cx: 0.3, cy: 0.25, hz: 0.5 })) });
}

test('encode() with LocalRenderer and LocalAnalyst against a mocked JAI Studio reaches the goal', async () => {
  const f = studio({ images: body => imageReply(body), chat: mockQwen });
  const renderer = new LocalRenderer(BASE, TOKEN, { fetch: f, outputFormat: 'jpeg' });
  const analyst = new LocalAnalyst(new OpenAIChatClient(BASE, TOKEN, { fetch: f }));
  const events = [];
  const r = await encode({ target: TARGET, models: [LOCAL_FLUX.id], renderer, analyst, budgetUSD: 0, population: 3, maxGenerations: 12, goal: 0.95, seeds: [1, 2], concurrency: 6, refine: false, onEvent: e => events.push(e) });
  assert.equal(r.stopReason, 'goal');
  assert.ok(r.best.fitness >= 0.95, String(r.best.fitness));
  assert.ok(r.generations[0].bestFitness < 0.6, 'it started far away');
  assert.equal(r.spent, 0);
  const doc = parse(r.text).document;
  assert.deepEqual(doc.get('model'), ['black-forest-labs/flux-schnell']);
  assert.deepEqual(doc.get('aspect'), ['3:2']);
  const imageCalls = f.calls.filter(c => c.url.endsWith('/v1/images/generations'));
  assert.ok(imageCalls.every(c => c.body.output_format === 'jpeg' && [1, 2].includes(c.body.seed) && c.body.width === 1216 && c.body.height === 832));
  const chatCalls = f.calls.filter(c => c.url.endsWith('/v1/chat/completions'));
  assert.equal(chatCalls.filter(c => c.body.response_format.json_schema.name === 'jai_target_inspection').length, 1, 'the target is measured once');
  assert.equal(chatCalls.filter(c => c.body.response_format.json_schema.name === 'jai_render_inspection').length, imageCalls.length, 'every render is inspected');
  assert.ok(chatCalls.every(c => c.body.messages[1].content.filter(p => p.type === 'image_url').length === 1 && c.body.messages[1].content[1].image_url.url.startsWith('data:image/')));
  assert.ok(r.best.renders.every(x => x.result.outputURL.startsWith('data:image/jpeg;base64,') && x.error === null));
  assert.ok(events.some(e => e.type === 'candidate'));
});

test('encode() with LocalAnalyst: a cancel while Qwen proposes stops as canceled, keeping the best', async () => {
  const ac = new AbortController();
  const chat = (body, init) => {
    const task = body.messages[1].content.find(p => p.type === 'text');
    if (body.response_format.json_schema.name === 'jai_drafts' && task.text.includes('Earlier attempts')) {
      queueMicrotask(() => ac.abort());
      return waitAbort(init.signal);
    }
    return mockQwen(body);
  };
  const f = studio({ images: body => imageReply(body), chat });
  const renderer = new LocalRenderer(BASE, TOKEN, { fetch: f, outputFormat: 'jpeg' });
  const analyst = new LocalAnalyst(new OpenAIChatClient(BASE, TOKEN, { fetch: f }));
  const r = await encode({ target: TARGET, models: [LOCAL_FLUX.id], renderer, analyst, budgetUSD: 0, population: 2, maxGenerations: 5, goal: 1, seeds: [1], refine: false, signal: ac.signal });
  assert.equal(r.stopReason, 'canceled');
  assert.equal(r.error, null);
  assert.equal(r.candidates.length, 2);
});

// ---------------------------------------------------------------- finding JAI Studio (src/providers/studio.ts)

/** A loopback port with nothing listening on it. */
const closedPort = async () => { const s = createServer(); await new Promise(r => s.listen(0, '127.0.0.1', r)); const { port } = s.address(); await new Promise(r => s.close(r)); return port; };

test('proveStudio: only a server holding the token proves it, for its own port; the token itself is never sent', async () => {
  const fake = await startFakeStudio({ token: TOKEN });
  const other = createServer((req, res) => { res.writeHead(200, { 'content-type': 'application/json' }); res.end('{"name":"something-else"}'); });
  await new Promise(r => other.listen(0, '127.0.0.1', r));
  try {
    assert.deepEqual(await proveStudio(fake.url, fake.port, TOKEN), { url: fake.url, version: '0.1.0' });
    assert.equal(await proveStudio(fake.url, fake.port, 'jai_someone-elses-token'), 'unproven');
    assert.equal(await proveStudio(fake.url, fake.port + 1, TOKEN), 'unproven', 'an answer relayed from another port does not match');
    assert.equal(await proveStudio(`http://127.0.0.1:${other.address().port}`, other.address().port, TOKEN), 'absent');
    const closed = await closedPort();
    assert.equal(await proveStudio(`http://127.0.0.1:${closed}`, closed, TOKEN), 'absent');
    assert.equal(fake.requests.length, 3);
    for (const q of fake.requests) {
      assert.match(q.url, /^\/health\?nonce=[A-Za-z0-9_-]{43}$/);
      assert.equal(q.headers.authorization, undefined);
    }
    assert.equal(new Set(fake.requests.map(q => q.url)).size, 3, 'a fresh nonce for every probe');
  } finally { await fake.close(); await new Promise(r => other.close(r)); }
});

test("findStudio: Studio's ten ports, all asked at once; the lowest one that proves the token wins", async () => {
  assert.deepEqual(STUDIO_PORTS, [5240, 5241, 5242, 5243, 5244, 5245, 5246, 5247, 5248, 5249]);
  const servers = [await startFakeStudio({ token: 'jai_someone-elses-token' }), await startFakeStudio({ token: TOKEN }), await startFakeStudio({ token: TOKEN })];
  try {
    const ports = [await closedPort(), ...servers.map(s => s.port)].sort((a, b) => a - b);
    const lowest = Math.min(servers[1].port, servers[2].port);
    assert.deepEqual(await findStudio(TOKEN, ports), { url: `http://127.0.0.1:${lowest}`, version: '0.1.0' });
    assert.equal(await findStudio('jai_nobody-has-this-token', ports), null);
    for (const s of servers) assert.ok(s.requests.every(q => q.url.startsWith('/health?nonce=') && !q.headers.authorization));
  } finally { for (const s of servers) await s.close(); }
});

// ---------------------------------------------------------------- bin/jai.mjs --local

const CLI = fileURLToPath(new URL('../bin/jai.mjs', import.meta.url));
const runCLI = (args, env) => new Promise((resolve, reject) => {
  const child = spawn(process.execPath, [CLI, ...args], { env: { ...process.env, ...env } });
  let stdout = '', stderr = '';
  child.stdout.on('data', d => { stdout += d; });
  child.stderr.on('data', d => { stderr += d; });
  child.on('error', reject);
  child.on('close', code => resolve({ code, stdout, stderr }));
});

/**
 * A fake JAI Studio on a free loopback port, found through server.json and
 * token in a JAI_HOME. Its /health proves the token for a nonce as Studio does
 * (HMAC-SHA256 of "jai-health:<port>:<nonce>"), with `proofToken` standing in
 * for the token it holds: another one plays a process that took the port.
 */
async function fakeStudio({ proofToken = TOKEN } = {}) {
  const fake = await startFakeStudio({ token: TOKEN, proofToken, images: imageReply });
  const home = mkdtempSync(join(tmpdir(), 'jai-cli-'));
  writeFileSync(join(home, 'server.json'), JSON.stringify({ host: '127.0.0.1', port: fake.port, pid: process.pid }));
  writeFileSync(join(home, 'token'), `${TOKEN}\n`);
  return { home, requests: fake.requests, close: fake.close };
}

test('CLI --local: renders through JAI Studio over node:http, not Node\'s fetch with its 300 s limit', async () => {
  const fake = await fakeStudio();
  try {
    const out = join(fake.home, 'castle.png');
    const r = await runCLI(['render', 'A castle;seed=5', '--local', '--out', out], { JAI_HOME: fake.home });
    assert.equal(r.code, 0, r.stderr);
    assert.match(r.stdout, /FLUX\.1 schnell on JAI Studio -> .*castle\.png \(seed 5, 1024x1024\)/);
    assert.deepEqual([...readFileSync(out)], [...imageBytes('png', 'A castle')]);
    const sent = fake.requests.find(q => q.url === '/v1/images/generations');
    assert.deepEqual(sent.body, { prompt: 'A castle', width: 1024, height: 1024, output_format: 'png', seed: 5 });
    assert.equal(sent.headers['content-type'], 'application/json');
    // Node's fetch (undici) marks its requests; node:http does not.
    assert.equal(sent.headers['sec-fetch-mode'], undefined);
  } finally { await fake.close(); }
});

test("CLI --local render: the preview's ratio without an aspect; transform without an input image is reported by the renderer only", async () => {
  const sharp = (await import('sharp')).default;
  const preview = `data:image/png;base64,${(await sharp({ create: { width: 16, height: 9, channels: 3, background: '#336699' } }).png().toBuffer()).toString('base64')}`;
  const fake = await fakeStudio();
  try {
    const out = join(fake.home, 'castle.png');
    const r = await runCLI(['render', `A castle in snow;mode=transform;source=https://example.com/c.png;preview=${preview}`, '--local', '--out', out], { JAI_HOME: fake.home });
    assert.equal(r.code, 0, r.stderr);
    assert.match(r.stdout, /\(seed 7, 1344x768\)/);
    const sent = fake.requests.find(q => q.url === '/v1/images/generations');
    assert.deepEqual([sent.body.width, sent.body.height], [1344, 768]);
    // The source is never fetched: original-not-readable, never mode-degraded beside it.
    assert.deepEqual(r.stdout.split('\n').filter(l => l.startsWith('  ')), ['  no-matching-model (model)', '  original-not-readable (mode)']);
  } finally { await fake.close(); }
});

test('CLI --local: the token goes only to a server that proves it holds it', async () => {
  for (const proofToken of ['jai_another-users-token', null]) {
    const fake = await fakeStudio({ proofToken });
    try {
      const r = await runCLI(['render', 'A castle;seed=5', '--local', '--out', join(fake.home, 'castle.png')], { JAI_HOME: fake.home });
      assert.equal(r.code, 1);
      assert.match(r.stderr, /did not prove that it holds your JAI Studio token/);
      assert.deepEqual(fake.requests.map(q => new URL(q.url, 'http://x').pathname), ['/health'], 'nothing but the probe');
      assert.match(fake.requests[0].url, /^\/health\?nonce=[A-Za-z0-9_-]{43}$/);
      assert.equal(fake.requests[0].headers.authorization, undefined);
    } finally { await fake.close(); }
  }
});

test('CLI --local: a server.json whose port has no server says so', async () => {
  const home = mkdtempSync(join(tmpdir(), 'jai-cli-'));
  writeFileSync(join(home, 'server.json'), JSON.stringify({ host: '127.0.0.1', port: await closedPort(), pid: process.pid }));
  writeFileSync(join(home, 'token'), `${TOKEN}\n`);
  const r = await runCLI(['render', 'A castle;seed=5', '--local', '--out', join(home, 'castle.png')], { JAI_HOME: home });
  assert.equal(r.code, 1);
  assert.match(r.stderr, /No JAI Studio server answers at http:\/\/127\.0\.0\.1:\d+\. Start JAI Studio or run `jai serve`\./);
});

test('CLI --local: --scale has the app\'s range, 0.25 to 1.25', async () => {
  const sharp = (await import('sharp')).default;
  const fake = await fakeStudio();
  try {
    const image = join(fake.home, 'tiny.png');
    writeFileSync(image, await sharp({ create: { width: 64, height: 48, channels: 3, background: '#336699' } }).png().toBuffer());
    const r = await runCLI(['encode', image, '--local', '--scale', '1.5'], { JAI_HOME: fake.home });
    assert.equal(r.code, 2);
    assert.match(r.stderr, /--scale must be a number from 0\.25 to 1\.25/);
  } finally { await fake.close(); }
});
