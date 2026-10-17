import { test } from 'node:test';
import assert from 'node:assert/strict';
import * as J from '../dist/jai.mjs';

test('file form round trip of a large document', async () => {
  const doc = J.fromObject({ prompt: ['A castle on a hill, '.repeat(3000).trim(), 'Mist below'], style: ['ink', 'loose'], aspect: '16:9' });
  const file = await J.encode(doc);
  assert.equal(file[0], 0x1f); assert.equal(file[1], 0x8b);
  const r = J.decode(file);
  assert.equal(r.form, 'gzip');
  assert.ok(J.documentsEqual(r.document, doc));
});

test('prompt=escapeProse(s) parses to exactly s', () => {
  const samples = ['a;b=c', 'x\;y', 'ends\\', ';starts', 'k=v;k2=v2', 'C:\\Users\\me', 'jai:prefixed', 'a\\\;b', '魔法；style＝x'];
  for (const s of samples) {
    const r = J.parse('prompt=' + J.escapeProse(s));
    assert.equal(JSON.stringify(J.toObject(r.document)), JSON.stringify({ prompt: [s] }), s);
  }
});

test('identity is the same across text, JSON, data: URL and file forms', async () => {
  const doc = J.parse('jai: A castle ; Style=anime;model=flux').document;
  const id = await J.identity(doc);
  assert.equal(await J.identity(J.readJSON(J.canonicalJSON(doc)).document), id);
  assert.equal(await J.identity(J.decodeDataURL(J.toDataURL(doc)).document), id);
  assert.equal(await J.identity(J.decode(await J.encode(doc)).document), id);
});

test('matchModel only ever returns catalog models', () => {
  const catalog = [{ id: 'black-forest-labs/flux-schnell', family: 'flux', default: true }, { id: 'stability-ai/sdxl', family: 'sdxl' }];
  const fam = { flux: ['flux'], sdxl: ['sdxl', 'stable diffusion xl'] };
  assert.equal(J.matchModel(['https://evil.example/model.safetensors'], { catalog, families: fam }).model.id, 'black-forest-labs/flux-schnell');
  assert.equal(J.matchModel(['stable-diffusion-xl-base-1.0'], { catalog, families: fam }).model.id, 'stability-ai/sdxl');
  assert.equal(J.matchModel(['some-user/exfiltrator'], { catalog, families: fam }).diagnostics.at(-1).code, 'no-matching-model');
});

test('trim removes exactly the 25 White_Space code points from both ends', () => {
  const ws = new RegExp(`^[${J.WS_CLASS}]$`);
  for (let c = 0; c < 0x10000; c++) {
    const ch = String.fromCharCode(c);
    assert.equal(J.trim(`${ch}${ch}a${ch}b${ch}`), ws.test(ch) ? `a${ch}b` : `${ch}${ch}a${ch}b${ch}`, c.toString(16));
  }
});

test('validate: newline-assignment is found, and long runs of whitespace cannot make validation slow', () => {
  const codes = prompt => J.validate(J.fromObject({ prompt })).map(d => d.code);
  assert.ok(codes('a castle\n  style = ink').includes('newline-assignment'));
  assert.ok(codes('a castle\n\n \n\tstyle/en=ink').includes('newline-assignment'));
  assert.ok(!codes('a castle\n\n\n').includes('newline-assignment'));
  assert.ok(!codes('a castle\n\n= ink').includes('newline-assignment'));
  // Regexes that retried a run of whitespace from each of its positions (the
  // newline-assignment check, trimming) took seconds here, minutes in QuickJS.
  const started = performance.now();
  assert.ok(!codes(`a${'\n'.repeat(200000)}b`).includes('newline-assignment'));
  J.validate(J.readJSON(JSON.stringify({ prompt: `a${' '.repeat(200000)}b` })).document);
  assert.ok(J.decodeDataURL(`data:image/jai${' '.repeat(100000)}x;base64,${btoa('jai: a castle')}`) !== undefined);
  assert.ok(performance.now() - started < 1000, `${Math.round(performance.now() - started)} ms`);
});
