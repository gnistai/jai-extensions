//! The web globals inside QuickJS: encoding, URL, crypto, timers, events,
//! console, Headers/Response, and the synthetic modules.

use serde_json::json;

use super::{module_job, run_job, run_js, tools_dir};
use crate::web::LOG_LINES;

#[test]
fn globals_are_installed_like_webidl_globals() {
    let v = run_js(
        r#"export default () => {
            const names = ['console', 'setTimeout', 'clearTimeout', 'setInterval', 'clearInterval', 'fetch', 'Headers', 'Response',
              'AbortController', 'AbortSignal', 'Event', 'EventTarget', 'TextEncoder', 'TextDecoder', 'URL', 'URLSearchParams',
              'crypto', 'DOMException', 'atob', 'btoa', 'queueMicrotask', 'performance', '__jai'];
            const missing = names.filter(n => globalThis[n] === undefined);
            const enumerable = ['fetch', 'URL', 'crypto', 'TextDecoder'].filter(n => Object.getOwnPropertyDescriptor(globalThis, n).enumerable);
            return { missing, enumerable, tag: Object.prototype.toString.call(new URL('https://a.test/')),
                     native: typeof globalThis.native, internal: typeof globalThis.internal };
        }"#,
    );
    assert_eq!(
        v,
        json!({ "missing": [], "enumerable": [], "tag": "[object URL]", "native": "undefined", "internal": "undefined" })
    );
}

#[test]
fn text_encoder_follows_the_encoding_standard() {
    let v = run_js(
        r#"export default () => {
            const e = new TextEncoder();
            const into = new Uint8Array(5);
            const r = e.encodeInto('a€🏰', into);
            return {
                encoding: e.encoding,
                euro: Array.from(e.encode('€')),
                lone: Array.from(e.encode('a\ud800b')),
                none: e.encode().length,
                coerced: Array.from(e.encode(12)),
                into: [r.read, r.written, Array.from(into)],
            };
        }"#,
    );
    assert_eq!(
        v,
        json!({
            "encoding": "utf-8",
            "euro": [0xe2, 0x82, 0xac],
            "lone": [0x61, 0xef, 0xbf, 0xbd, 0x62],
            "none": 0,
            "coerced": [0x31, 0x32],
            "into": [2, 4, [0x61, 0xe2, 0x82, 0xac, 0]],
        })
    );
}

#[test]
fn text_decoder_fatal_bom_and_streaming() {
    let v = run_js(
        r#"export default () => {
            const bytes = a => new Uint8Array(a);
            const fatal = new TextDecoder('utf-8', { fatal: true });
            let threw = null;
            try { fatal.decode(bytes([0x61, 0xff])); } catch (e) { threw = [e.name, e instanceof TypeError]; }
            const lenient = new TextDecoder();
            const bom = bytes([0xef, 0xbb, 0xbf, 0x68, 0x69]);
            const stream = new TextDecoder();
            const parts = [stream.decode(bytes([0xe2]), { stream: true }), stream.decode(bytes([0x82]), { stream: true }), stream.decode(bytes([0xac, 0x21]))];
            const splitBom = new TextDecoder();
            const b1 = splitBom.decode(bytes([0xef, 0xbb]), { stream: true });
            const b2 = splitBom.decode(bytes([0xbf, 0x41]));
            let label = null;
            try { new TextDecoder('latin1'); } catch (e) { label = e.name; }
            return {
                threw,
                okFatal: fatal.decode(bytes([0xc3, 0xa9])),
                replaced: lenient.decode(bytes([0x61, 0xff, 0xe2, 0x82, 0x62, 0xf0, 0x80, 0x80])),
                truncated: lenient.decode(bytes([0xe2, 0x82])),
                bomStripped: lenient.decode(bom),
                bomKept: new TextDecoder('utf-8', { ignoreBOM: true }).decode(bom).length,
                parts,
                splitBom: [b1, b2],
                labels: [new TextDecoder(' UTF8 ').encoding, new TextDecoder('unicode-1-1-utf-8').fatal],
                label,
                views: [lenient.decode(new Uint16Array([0x6968]).buffer), lenient.decode(new DataView(bom.buffer, 3)), lenient.decode()],
            };
        }"#,
    );
    assert_eq!(
        v,
        json!({
            "threw": ["TypeError", true],
            "okFatal": "é",
            "replaced": "a\u{fffd}\u{fffd}b\u{fffd}\u{fffd}\u{fffd}",
            "truncated": "\u{fffd}",
            "bomStripped": "hi",
            "bomKept": 3,
            "parts": ["", "", "€!"],
            "splitBom": ["", "A"],
            "labels": ["utf-8", false],
            "label": "RangeError",
            "views": ["hi", "hi", ""],
        })
    );
}

#[test]
fn url_parsing_serialisation_and_resolution() {
    let v = run_js(
        r#"export default () => {
            const u = new URL('../x/./y?q=1 2#f g', 'https://user:pw@EXAMPLE.com:8080/a/b/c');
            const parts = ['href', 'origin', 'protocol', 'username', 'password', 'host', 'hostname', 'port', 'pathname', 'search', 'hash'];
            let invalid = null;
            try { new URL('/relative'); } catch (e) { invalid = [e.name, e.message, e.code]; }
            return {
                parts: Object.fromEntries(parts.map(p => [p, u[p]])),
                data: [
                    new URL('data:image/jai,a b%20c#frag').href,
                    new URL('  data:image/jai,x y  ').href,
                    new URL('data:image/jai,a\tb\nc').href,
                    new URL('DATA:image/png;base64,iVBOR=').protocol,
                    new URL('data:text/plain,x').origin,
                ],
                normalised: [
                    new URL('HTTPS://EXAMPLE.com:443/a/./b/../c').href,
                    new URL('http://ÉXAMPLE.com/').hostname,
                    new URL('http://0x7f.1/').host,
                    new URL('file:///C:/Users/x/a b.jai').href,
                    new URL('https://example.test/p?x=é#é').href,
                    new URL('?a=b', 'https://example.test/dir/page.html').href,
                    new URL('//other.test/z', 'https://example.test/dir/').href,
                    new URL('https://example.test/page/').href,
                ],
                invalid,
                canParse: [URL.canParse('https://a.test'), URL.canParse('nope'), URL.canParse('x', 'https://a.test/')],
                parse: URL.parse('nope'),
                json: JSON.stringify({ u: new URL('https://a.test/x') }),
                string: String(new URL('https://a.test/x')),
            };
        }"#,
    );
    assert_eq!(
        v,
        json!({
            "parts": {
                "href": "https://user:pw@example.com:8080/a/x/y?q=1%202#f%20g",
                "origin": "https://example.com:8080",
                "protocol": "https:",
                "username": "user",
                "password": "pw",
                "host": "example.com:8080",
                "hostname": "example.com",
                "port": "8080",
                "pathname": "/a/x/y",
                "search": "?q=1%202",
                "hash": "#f%20g",
            },
            "data": [
                "data:image/jai,a b%20c#frag",
                "data:image/jai,x y",
                "data:image/jai,abc",
                "data:",
                "null",
            ],
            "normalised": [
                "https://example.com/a/c",
                "xn--xample-9ua.com",
                "127.0.0.1",
                "file:///C:/Users/x/a%20b.jai",
                "https://example.test/p?x=%C3%A9#%C3%A9",
                "https://example.test/dir/page.html?a=b",
                "https://other.test/z",
                "https://example.test/page/",
            ],
            "invalid": ["TypeError", "Invalid URL", "ERR_INVALID_URL"],
            "canParse": [true, false, true],
            "parse": null,
            "json": "{\"u\":\"https://a.test/x\"}",
            "string": "https://a.test/x",
        })
    );
}

#[test]
fn url_setters_and_live_search_params() {
    let v = run_js(
        r#"export default () => {
            const u = new URL('https://a.test/p?x=1');
            const sp = u.searchParams;
            sp.append('y', 'a b');
            const afterAppend = u.href;
            u.search = '?z=3&z=4';
            const afterSearch = [sp.getAll('z'), sp.has('x'), u.searchParams === sp];
            sp.delete('z');
            const afterDelete = u.href;
            u.hash = 'h';
            u.pathname = '/p q';
            u.port = '8443';
            u.port = 'nope';
            u.protocol = 'http';
            u.hostname = 'b.test';
            u.username = 'me';
            const href = u.href;
            let threw = null;
            try { u.href = 'not a url'; } catch (e) { threw = e.name; }
            const opaque = new URL('data:text/plain,x  ?q');
            opaque.search = '';
            return { afterAppend, afterSearch, afterDelete, href, threw, unchanged: u.href === href, opaque: opaque.href };
        }"#,
    );
    assert_eq!(
        v,
        json!({
            "afterAppend": "https://a.test/p?x=1&y=a+b",
            "afterSearch": [["3", "4"], false, true],
            "afterDelete": "https://a.test/p",
            "href": "http://me@b.test:8443/p%20q#h",
            "threw": "TypeError",
            "unchanged": true,
            "opaque": "data:text/plain,x",
        })
    );
}

#[test]
fn url_search_params_follow_the_url_standard() {
    let v = run_js(
        r#"export default () => {
            const p = new URLSearchParams('?a=1&b=x+y&a=%zz&c=%C3%A9&&d');
            const entries = [...p];
            p.set('a', 'only');
            p.sort();
            const fromRecord = new URLSearchParams({ k: 'v w', 'é': '*-._~!' }).toString();
            const fromPairs = new URLSearchParams([['x', '1'], ['y', '2']]);
            let bad = null;
            try { new URLSearchParams([['x']]); } catch (e) { bad = e.name; }
            const keys = [];
            fromPairs.forEach((v, k) => keys.push(k + v));
            return { entries, after: p.toString(), get: [p.get('a'), p.get('missing')], size: p.size, fromRecord, keys, bad,
                     has: [p.has('b', 'x y'), p.has('b', 'nope')], lone: new URLSearchParams([['\ud800', 'v']]).toString() };
        }"#,
    );
    assert_eq!(
        v,
        json!({
            "entries": [["a", "1"], ["b", "x y"], ["a", "%zz"], ["c", "é"], ["d", ""]],
            "after": "a=only&b=x+y&c=%C3%A9&d=",
            "get": ["only", null],
            "size": 4,
            "fromRecord": "k=v+w&%C3%A9=*-._%7E%21",
            "keys": ["x1", "y2"],
            "bad": "TypeError",
            "has": [true, false],
            "lone": "%EF%BF%BD=v",
        })
    );
}

#[test]
fn crypto_digest_random_values_and_uuid() {
    let v = run_js(
        r#"export default async () => {
            const hex = b => Array.from(new Uint8Array(b), x => x.toString(16).padStart(2, '0')).join('');
            const enc = s => new TextEncoder().encode(s);
            const d = await Promise.all([
                crypto.subtle.digest('SHA-256', enc('')),
                crypto.subtle.digest('SHA-256', enc('abc')),
                crypto.subtle.digest({ name: 'sha-384' }, enc('abc')),
                crypto.subtle.digest('SHA-512', enc('abc').buffer),
            ]);
            const errors = [];
            for (const [alg, data] of [['SHA-1', enc('')], ['MD5', enc('')], ['SHA-256', 'text']]) {
                try { await crypto.subtle.digest(alg, data); errors.push('resolved'); } catch (e) { errors.push(e.name); }
            }
            const a = new Uint32Array(16);
            const same = crypto.getRandomValues(a) === a;
            const quota = (() => { try { crypto.getRandomValues(new Uint8Array(65537)); } catch (e) { return e.name; } })();
            const float = (() => { try { crypto.getRandomValues(new Float32Array(2)); } catch (e) { return e.name; } })();
            const uuid = crypto.randomUUID();
            return { d: d.map(hex), isBuffer: d[0] instanceof ArrayBuffer, errors, same, filled: a.some(x => x !== 0), quota, float,
                     uuid: /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(uuid), unique: uuid !== crypto.randomUUID() };
        }"#,
    );
    assert_eq!(
        v,
        json!({
            "d": [
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
                "cb00753f45a35e8bb5a03d699ac65007272c32ab0eded1631a8b605a43ff5bed8086072ba1e7cc2358baeca134c825a7",
                "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f",
            ],
            "isBuffer": true,
            "errors": ["NotSupportedError", "NotSupportedError", "TypeError"],
            "same": true,
            "filled": true,
            "quota": "QuotaExceededError",
            "float": "TypeMismatchError",
            "uuid": true,
            "unique": true,
        })
    );
}

#[test]
fn timers_fire_in_order_and_clear() {
    let v = run_js(
        r#"export default () => new Promise(resolve => {
            const log = [];
            setTimeout(() => log.push('a'), 20);
            setTimeout(() => log.push('b'), 20);
            setTimeout((x, y) => log.push('c' + x + y), 0, 1, 2);
            const cleared = setTimeout(() => log.push('never'), 5);
            clearTimeout(cleared);
            clearTimeout(undefined);
            let n = 0;
            const iv = setInterval(() => { log.push('i' + ++n); if (n === 3) clearInterval(iv); }, 1);
            setTimeout(() => { throw new Error('timer-callback-error-marker'); }, 1);
            setTimeout(() => resolve({ log, ids: typeof iv }), 60);
        })"#,
    );
    let log: Vec<String> = serde_json::from_value(v["log"].clone()).unwrap();
    assert_eq!(
        log.iter().filter(|s| s.starts_with('i')).count(),
        3,
        "{log:?}"
    );
    let pos = |s: &str| {
        log.iter()
            .position(|x| x == s)
            .unwrap_or_else(|| panic!("{s} missing in {log:?}"))
    };
    assert!(pos("c12") < pos("a") && pos("a") < pos("b"), "{log:?}");
    assert!(!log.contains(&"never".to_string()));
    assert_eq!(v["ids"], "number");
    let lines = LOG_LINES.lock().unwrap().clone();
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("error: Uncaught Error: timer-callback-error-marker")),
        "{lines:?}"
    );
}

#[test]
fn abort_controller_and_signal() {
    let v = run_js(
        r#"export default async () => {
            const c = new AbortController();
            const seen = [];
            c.signal.addEventListener('abort', e => seen.push(['listener', e.type, e.target === c.signal]));
            c.signal.addEventListener('abort', () => seen.push('once'), { once: true });
            const removed = () => seen.push('removed');
            c.signal.addEventListener('abort', removed);
            c.signal.removeEventListener('abort', removed);
            c.signal.onabort = () => seen.push('onabort');
            c.signal.addEventListener('abort', () => { throw new Error('listener-error-marker'); });
            c.abort();
            c.abort();
            let thrown = null;
            try { c.signal.throwIfAborted(); } catch (e) { thrown = [e.name, e.code, e instanceof DOMException]; }
            const custom = new AbortController();
            custom.abort('why');
            const any = AbortSignal.any([new AbortController().signal, custom.signal]);
            const outer = new AbortController();
            const follower = AbortSignal.any([outer.signal]);
            const chained = AbortSignal.any([follower]);
            let order = [];
            follower.addEventListener('abort', () => order.push('follower'));
            chained.addEventListener('abort', () => order.push('chained'));
            outer.signal.addEventListener('abort', () => order.push('outer'));
            outer.abort('stop');
            const timeout = AbortSignal.timeout(5);
            await new Promise(r => timeout.addEventListener('abort', r));
            let illegal = null;
            try { new AbortSignal(); } catch (e) { illegal = e.name; }
            return { aborted: c.signal.aborted, seen, thrown, reason: c.signal.reason.name, custom: custom.signal.reason,
                     anyReason: any.reason, anyAborted: any.aborted, order, chained: chained.reason,
                     timeout: timeout.reason.name, static: AbortSignal.abort().reason.name, illegal };
        }"#,
    );
    assert_eq!(
        v,
        json!({
            "aborted": true,
            "seen": [["listener", "abort", true], "once", "onabort"],
            "thrown": ["AbortError", 20, true],
            "reason": "AbortError",
            "custom": "why",
            "anyReason": "why",
            "anyAborted": true,
            "order": ["outer", "follower", "chained"],
            "chained": "stop",
            "timeout": "TimeoutError",
            "static": "AbortError",
            "illegal": "TypeError",
        })
    );
    let lines = LOG_LINES.lock().unwrap().clone();
    assert!(
        lines.iter().any(|l| l.contains("listener-error-marker")),
        "{lines:?}"
    );
}

#[test]
fn console_formats_values() {
    run_js(
        r#"export default () => {
            const o = { a: 1, s: 'x', nested: { deep: { deeper: { deepest: 1 } } }, list: [1, 'two', null, undefined], u8: new Uint8Array([1, 2]) };
            o.self = o;
            console.log('console-marker-1', o);
            console.info('console-marker-2 %s=%d %j %%', 'n', 42.9, { j: true });
            console.warn('console-marker-3', new TypeError('bad'), 10n, Symbol('s'), -0);
            console.error('console-marker-4', new Map([[1, 'a']]), new Set(['b']), () => {});
            console.debug('console-marker-5', 'lone \ud800');
            console.assert(false, 'console-marker-6');
            console.group('console-marker-7'); console.log('console-marker-8'); console.groupEnd();
        }"#,
    );
    let lines = LOG_LINES.lock().unwrap().clone();
    let find = |m: &str| {
        lines
            .iter()
            .find(|l| l.contains(m))
            .cloned()
            .unwrap_or_else(|| panic!("{m} missing in {lines:?}"))
    };
    assert_eq!(
        find("console-marker-1"),
        "log: console-marker-1 { a: 1, s: 'x', nested: { deep: { deeper: [Object] } }, list: [ 1, 'two', null, undefined ], u8: Uint8Array(2) [ 1, 2 ], self: [Circular] }"
    );
    assert_eq!(
        find("console-marker-2"),
        "info: console-marker-2 n=42.9 {\"j\":true} %"
    );
    assert!(
        find("console-marker-3").starts_with("warn: console-marker-3 TypeError: bad\n"),
        "{}",
        find("console-marker-3")
    );
    assert!(
        find("console-marker-3").ends_with(" 10n Symbol(s) -0"),
        "{}",
        find("console-marker-3")
    );
    assert_eq!(
        find("console-marker-4"),
        "error: console-marker-4 Map(1) [ 1 => 'a' ] Set(1) [ 'b' ] [Function: (anonymous)]"
    );
    assert_eq!(
        find("console-marker-5"),
        "debug: console-marker-5 lone \u{fffd}"
    );
    assert_eq!(
        find("console-marker-6"),
        "error: Assertion failed: console-marker-6"
    );
    assert_eq!(find("console-marker-8"), "log:   console-marker-8");
}

#[test]
fn headers_and_response() {
    let v = run_js(
        r#"export default async () => {
            const h = new Headers({ 'Content-Type': 'text/plain', 'X-B': ' 1 ' });
            h.append('x-b', '2');
            h.append('Set-Cookie', 'a=1');
            h.append('set-cookie', 'b=2');
            const errors = [];
            for (const f of [() => h.append('bad name', 'x'), () => h.set('x', 'a\nb'), () => new Headers([['only-one']])]) {
                try { f(); errors.push('ok'); } catch (e) { errors.push(e.name); }
            }
            const r = new Response('{"a":[1,2]}', { status: 201, statusText: 'Created', headers: [['X-Test', 'yes']] });
            const json = await r.json();
            let reused = null;
            try { await r.text(); } catch (e) { reused = e.name; }
            const bin = new Response(new Uint8Array([1, 2, 3]));
            const ab = await bin.clone().arrayBuffer();
            let range = null;
            try { new Response('', { status: 99 }); } catch (e) { range = e.name; }
            const j = Response.json({ ok: 1 });
            return {
                get: [h.get('content-type'), h.get('X-B'), h.get('missing'), h.has('X-b')],
                entries: [...h],
                cookies: h.getSetCookie(),
                errors,
                response: [r.status, r.ok, r.statusText, r.headers.get('x-test'), r.headers.get('content-type'), r.bodyUsed, json],
                reused,
                bin: [Array.from(new Uint8Array(ab)), bin.headers.get('content-type'), (await bin.bytes()).length],
                range,
                json: [j.headers.get('content-type'), await j.text()],
                empty: await new Response(null, { status: 204 }).text(),
            };
        }"#,
    );
    assert_eq!(
        v,
        json!({
            "get": ["text/plain", "1, 2", null, true],
            "entries": [["content-type", "text/plain"], ["set-cookie", "a=1"], ["set-cookie", "b=2"], ["x-b", "1, 2"]],
            "cookies": ["a=1", "b=2"],
            "errors": ["TypeError", "TypeError", "TypeError"],
            "response": [201, true, "Created", "yes", "text/plain;charset=UTF-8", true, { "a": [1, 2] }],
            "reused": "TypeError",
            "bin": [[1, 2, 3], null, 3],
            "range": "RangeError",
            "json": ["application/json", "{\"ok\":1}"],
            "empty": "",
        })
    );
}

#[test]
fn node_zlib_and_node_crypto_modules() {
    let v = run_js(
        r#"export default async () => {
            const zlib = await import('node:zlib');
            const { webcrypto, randomUUID } = await import('node:crypto');
            const text = 'hello hello hello hello '.repeat(50);
            const packed = zlib.deflateRawSync(new TextEncoder().encode(text), { level: 9 });
            const back = new TextDecoder().decode(zlib.inflateRawSync(packed));
            const fromString = zlib.inflateRawSync(zlib.default.deflateRawSync(text));
            const errors = [];
            for (const f of [() => zlib.inflateRawSync(new Uint8Array([0xff, 0xff, 0xff])), () => zlib.inflateRawSync(packed.subarray(0, 5)),
                             () => zlib.deflateRawSync(new Uint8Array(1), { level: 10 }), () => zlib.inflateRawSync(packed, { maxOutputLength: 10 })]) {
                try { f(); errors.push('ok'); } catch (e) { errors.push(`${e.name}:${e.code ?? ''}`); }
            }
            return { ratio: packed.length < text.length / 10, same: back === text, isU8: packed instanceof Uint8Array,
                     fromString: fromString.length, errors, webcrypto: webcrypto === crypto, uuid: typeof randomUUID() };
        }"#,
    );
    assert_eq!(
        v,
        json!({
            "ratio": true,
            "same": true,
            "isU8": true,
            "fromString": 1200,
            "errors": ["Error:Z_DATA_ERROR", "Error:Z_BUF_ERROR", "RangeError:", "RangeError:"],
            "webcrypto": true,
            "uuid": "string",
        })
    );
}

#[test]
fn library_gzip_output_is_compressed_through_node_zlib() {
    let jai = tools_dir().join("dist").join("jai.mjs");
    let source =
        std::fs::read_to_string(&jai).expect("dist/jai.mjs (run npm run build in jai-tools)");
    let mut job = module_job(
        r#"import * as J from 'jai:lib';
        export default async () => {
            const doc = J.parse('prompt=a lighthouse on a cliff, a lighthouse on a cliff, a lighthouse on a cliff').document;
            const file = await J.encode(doc);
            const back = J.decode(file);
            return { gzip: [file[0], file[1]], blockType: (file[10] >> 1) & 3, form: back.form,
                     same: J.serialize(back.document) === J.serialize(doc), identity: (await J.identity(doc)).length };
        }"#,
    );
    job.modules.push(("jai:lib".into(), source));
    let v = run_job(job).unwrap();
    assert_eq!(v["gzip"], json!([0x1f, 0x8b]));
    assert_ne!(
        v["blockType"], 0,
        "the first deflate block is stored, so node:zlib was not used"
    );
    assert_eq!(v["form"], "gzip");
    assert_eq!(v["same"], true);
    assert_eq!(v["identity"], 64);
}

#[test]
fn engine_builtins_are_present() {
    let v = run_js(
        r#"export default async () => {
            const order = [];
            queueMicrotask(() => order.push('micro'));
            await null;
            return { btoa: btoa('hi'), atob: atob('aGk='), perf: typeof performance.now(), order,
                     dom: new DOMException('m', 'NotFoundError').code, structured: typeof structuredClone };
        }"#,
    );
    assert_eq!(
        v,
        json!({ "btoa": "aGk=", "atob": "hi", "perf": "number", "order": ["micro"], "dom": 8, "structured": "undefined" })
    );
}
