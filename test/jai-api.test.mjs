import { test } from 'node:test';
import assert from 'node:assert/strict';
import * as J from '../dist/jai.mjs';

test('file form round trip of a large document', async () => {
  const doc = J.fromObject({ prompt: ['A castle on a hill, '.repeat(3000).trim(), 'Mist below'], style: ['ink', 'loose'], aspect: '16:9' });
  const file = await J.encode(doc);
  assert.equal(file[0], 0x1f); assert.equal(file[1], 0x8b);
  const r = J.decode(file);
  assert.deepEqual(r.diagnostics, []);
  assert.ok(J.documentsEqual(r.document, doc));
});

test('one serialisation per place: file form for .jai bytes, text form for data: URLs and text', async () => {
  const doc = J.fromObject({ prompt: 'A castle', style: 'anime' });
  assert.equal(J.serialize(doc), 'A castle;style=anime');
  assert.equal(J.toDataURL(doc), 'data:image/jai,A%20castle;style=anime');
  assert.deepEqual(Object.keys(J.decode(await J.encode(doc))).sort(), ['diagnostics', 'document']);
  const codes = r => r.diagnostics.map(d => d.code);
  assert.deepEqual(codes(J.decode(new TextEncoder().encode(J.canonicalJSON(doc)))), ['malformed-container']);
  assert.deepEqual(codes(J.decode(new TextEncoder().encode(J.serialize(doc)))), ['malformed-container']);
  assert.deepEqual({ ...J.toObject(J.decodeDataURL('data:image/jai,' + encodeURIComponent(J.canonicalJSON(doc))).document) }, { prompt: [J.canonicalJSON(doc)] });
  assert.deepEqual(codes(J.decodeDataURL('data:image/jai;base64,' + Buffer.from(await J.encode(doc)).toString('base64'))), ['undecodable']);
  assert.deepEqual(Object.keys(J.readJSON(J.canonicalJSON(doc))).sort(), ['diagnostics', 'document']);
});

test('withAltPrompt fills only a missing prompt', () => {
  const show = d => JSON.stringify(J.toObject(d));
  assert.equal(show(J.withAltPrompt(J.parse('style=anime;prompt/flux=warm').document, ' A castle ')), JSON.stringify({ prompt: ['A castle'], style: ['anime'], 'prompt/flux': ['warm'] }));
  assert.equal(show(J.withAltPrompt(J.parse('A tower;style=anime').document, 'A castle')), JSON.stringify({ prompt: ['A tower'], style: ['anime'] }));
  assert.equal(show(J.withAltPrompt(J.parse('style=anime').document, '  ')), JSON.stringify({ style: ['anime'] }));
});

test('prompt=escapeProse(s) parses to exactly s', () => {
  const samples = ['a;b=c', 'x\;y', 'ends\\', ';starts', 'k=v;k2=v2', 'C:\\Users\\me', 'jai:x', 'a\\\;b', 'a\\;style=x', '魔法；style＝x'];
  for (const s of samples) {
    const r = J.parse('prompt=' + J.escapeProse(s));
    assert.equal(JSON.stringify(J.toObject(r.document)), JSON.stringify({ prompt: [s] }), s);
  }
});

test('identity is the same across text, JSON, data: URL and file forms', async () => {
  const doc = J.parse(' A castle ; Style=anime;model=flux').document;
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
  assert.ok(J.decodeDataURL(`data:image/jai${' '.repeat(100000)}x;base64,${btoa('a castle')}`) !== undefined);
  assert.ok(performance.now() - started < 1000, `${Math.round(performance.now() - started)} ms`);
});

test('validateDataURL reports how a data:image/jai URL is written, and nothing for other URLs', () => {
  const codes = url => J.validateDataURL(url).map(d => d.code);
  assert.deepEqual(codes('data:image/jai,A%20castle;style=anime'), []);
  assert.deepEqual(codes(' \tDATA:IMAGE/JAI,A%20castle\n'), []);
  assert.deepEqual(codes('data:image/png;base64,iVBORw0KGgo='), []);
  assert.deepEqual(codes('castle.jai'), []);
  assert.deepEqual(codes('data:image/jai;A castle'), ['data-url-missing-comma']);
  assert.deepEqual(codes('data:image/jai;base64,QSBjYXN0bGU='), ['data-url-parameter']);
  assert.deepEqual(codes('data:image/jai;charset=utf-8,A%20castle'), ['data-url-parameter']);
  assert.deepEqual(codes('data:image/jai,A castle'), ['unencoded-url-text']);
  assert.deepEqual(codes('data:image/jai,A%20castle#top'), ['unencoded-url-text']);
  assert.deepEqual(codes('data:image/jai,100%'), ['unencoded-url-text']);
});

test('validateDelivery: missing-size unless width and height or a style aspect-ratio size the src image', () => {
  const img = attrs => ({ localName: 'img', parentElement: null, children: [], getAttribute: n => attrs[n] ?? null, getAttributeNames: () => Object.keys(attrs) });
  const codes = attrs => J.validateDelivery({ carrier: 'src', element: img({ src: 'castle.jai', alt: 'A castle', ...attrs }) }).map(d => d.code);
  assert.deepEqual(codes({}), ['missing-size']);
  assert.deepEqual(codes({ width: '800' }), ['missing-size']);
  assert.deepEqual(codes({ width: '800', height: '600' }), []);
  assert.deepEqual(codes({ style: 'max-width:100%; aspect-ratio: 4 / 3' }), []);
  assert.deepEqual(codes({ style: 'ASPECT-RATIO:auto 16/9' }), []);
  assert.deepEqual(codes({ style: 'aspect-ratio: auto' }), ['missing-size']);
  assert.deepEqual(codes({ style: '--aspect-ratio: 2' }), ['missing-size']);
});
