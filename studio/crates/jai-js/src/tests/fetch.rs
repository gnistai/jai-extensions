//! fetch against local mocks: the origin allow-list, request headers and
//! bodies, the Response readers, and abort (including the server seeing the
//! connection close).

use std::collections::BTreeMap;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::Path;
use axum::http::{header, HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Redirect};
use axum::routing::{any, get};
use axum::{Json, Router};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::{module_job, run_job_async, run_js_with_server};
use crate::ServerRef;

async fn echo(method: Method, uri: Uri, headers: HeaderMap, body: Bytes) -> Json<Value> {
    let mut combined: BTreeMap<String, String> = BTreeMap::new();
    for (k, v) in &headers {
        let v = String::from_utf8_lossy(v.as_bytes());
        combined
            .entry(k.as_str().to_owned())
            .and_modify(|s| *s = format!("{s}, {v}"))
            .or_insert_with(|| v.into_owned());
    }
    let headers = combined;
    Json(json!({
        "method": method.as_str(),
        "path": uri.path(),
        "query": uri.query(),
        "headers": headers,
        "body": String::from_utf8_lossy(&body),
    }))
}

async fn status(Path(code): Path<u16>) -> impl IntoResponse {
    (
        StatusCode::from_u16(code).unwrap_or(StatusCode::IM_A_TEAPOT),
        [("x-status", code.to_string())],
        "status body",
    )
}

/// The mock server's base URL.
async fn mock_server() -> String {
    let app = Router::new()
        .route("/echo", any(echo))
        .route(
            "/json",
            get(|| async { Json(json!({ "a": 1, "b": [true, null], "s": "é" })) }),
        )
        .route(
            "/bytes",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "application/octet-stream")],
                    vec![0u8, 1, 2, 254, 255],
                )
            }),
        )
        .route("/bom", get(|| async { b"\xef\xbb\xbf{\"x\":1}".to_vec() }))
        .route("/status/{code}", get(status))
        .route(
            "/redirect-same",
            get(|| async { Redirect::temporary("/json") }),
        )
        .route(
            "/redirect-other",
            get(|| async { Redirect::temporary("http://127.0.0.2:9/json") }),
        )
        .route(
            "/slow",
            get(|| async {
                tokio::time::sleep(Duration::from_secs(30)).await;
                "late"
            }),
        );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            eprintln!("mock server stopped: {e}");
        }
    });
    format!("http://{addr}")
}

fn with_base(source: &str, base: &str) -> String {
    source.replace("BASE", &serde_json::to_string(base).unwrap())
}

#[tokio::test]
async fn only_the_local_server_origin_is_reachable() {
    let base = mock_server().await;
    let other_port = format!(
        "http://127.0.0.1:{}",
        base.rsplit(':').next().unwrap().parse::<u16>().unwrap() ^ 1
    );
    let source = with_base(
        r#"export default async () => {
            const attempt = async (u, init) => {
                try { const r = await fetch(u, init); return ['resolved', r.status]; }
                catch (e) { return [e.name, e.message.startsWith('fetch blocked') ? 'blocked' : e.message]; }
            };
            return {
                ok: await attempt(BASE + '/json'),
                otherHost: await attempt('http://127.0.0.2:1/json'),
                otherPort: await attempt(OTHER + '/json'),
                https: await attempt(BASE.replace('http:', 'https:') + '/json'),
                remote: await attempt('https://example.com/'),
                data: await attempt('data:text/plain,hi'),
                file: await attempt('file:///etc/passwd'),
                redirect: await attempt(BASE + '/redirect-other'),
                credentials: (await attempt(BASE.replace('//', '//user:pw@') + '/json'))[0],
                relative: (await attempt('/json'))[0],
                getBody: (await attempt(BASE + '/echo', { body: 'x' }))[0],
                method: (await attempt(BASE + '/echo', { method: 'CONNECT' }))[0],
            };
        }"#
        .replace("OTHER", &serde_json::to_string(&other_port).unwrap())
        .as_str(),
        &base,
    );
    let v = run_js_with_server(&source, &base).await;
    let blocked = json!(["TypeError", "blocked"]);
    assert_eq!(v["ok"], json!(["resolved", 200]));
    for key in [
        "otherHost",
        "otherPort",
        "https",
        "remote",
        "data",
        "file",
        "redirect",
    ] {
        assert_eq!(v[key], blocked, "{key}: {v}");
    }
    for key in ["credentials", "relative", "getBody", "method"] {
        assert_eq!(v[key], "TypeError", "{key}: {v}");
    }
}

#[tokio::test]
async fn without_a_server_every_fetch_is_blocked() {
    let base = mock_server().await;
    let job = module_job(&with_base(
        r#"export default async () => { try { await fetch(BASE + '/json'); return 'resolved'; } catch (e) { return [e.name, e.message]; } }"#,
        &base,
    ));
    let v = run_job_async(job).await.unwrap();
    assert_eq!(v[0], "TypeError");
    assert!(v[1].as_str().unwrap().starts_with("fetch blocked"), "{v}");
}

#[tokio::test]
async fn request_methods_headers_and_bodies() {
    let base = mock_server().await;
    let source = with_base(
        r#"export default async () => {
            const call = async (path, init) => (await fetch(BASE + path, init)).json();
            const a = await call('/echo?q=1', { method: 'post', headers: { 'X-One': '1', 'Content-Type': 'application/json' }, body: JSON.stringify({ a: 1 }) });
            const b = await call('/echo', { method: 'POST', headers: [['x-two', '2'], ['x-two', '3']], body: new Uint8Array([104, 105]) });
            const c = await call('/echo', { headers: new Headers({ 'x-three': '3', Host: 'evil.test', Origin: 'https://evil.test', Cookie: 'c=1' }) });
            const d = await call('/echo', { method: 'POST', body: 'plain' });
            const e = await call('/echo', { method: 'POST', body: new URLSearchParams({ a: 'b c' }) });
            const f = await call('/echo', { method: 'PUT', body: new Uint16Array([0x6968]).buffer });
            const g = await fetch(new URL(BASE + '/echo'), { method: 'HEAD' });
            return {
                a: [a.method, a.path, a.query, a.headers['x-one'], a.headers['content-type'], a.body, a.headers.accept],
                b: [b.headers['x-two'], b.body, b.headers['content-type'] ?? null],
                c: [c.method, c.headers['x-three'], c.headers.host === BASE.slice(7), c.headers.origin ?? null, c.headers.cookie ?? null],
                d: [d.headers['content-type'], d.body],
                e: [e.headers['content-type'], e.body],
                f: [f.method, f.body],
                g: [g.status, await g.text()],
            };
        }"#,
        &base,
    );
    let v = run_js_with_server(&source, &base).await;
    assert_eq!(
        v,
        json!({
            "a": ["POST", "/echo", "q=1", "1", "application/json", "{\"a\":1}", "*/*"],
            "b": ["2, 3", "hi", null],
            "c": ["GET", "3", true, null, null],
            "d": ["text/plain;charset=UTF-8", "plain"],
            "e": ["application/x-www-form-urlencoded;charset=UTF-8", "a=b+c"],
            "f": ["PUT", "hi"],
            "g": [200, ""],
        })
    );
}

#[tokio::test]
async fn response_readers_status_and_headers() {
    let base = mock_server().await;
    let source = with_base(
        r#"export default async () => {
            const json = await fetch(BASE + '/json');
            const body = await json.json();
            let reused = null;
            try { await json.text(); } catch (e) { reused = e.name; }
            const bytes = await fetch(BASE + '/bytes');
            const missing = await fetch(BASE + '/status/404');
            let immutable = null;
            try { missing.headers.set('x', 'y'); } catch (e) { immutable = e.name; }
            const redirect = await fetch(BASE + '/redirect-same');
            const bom = await (await fetch(BASE + '/bom')).text();
            const clone = (await fetch(BASE + '/json')).clone();
            return {
                json: [json.status, json.ok, json.statusText, json.headers.get('Content-Type'), json.type, json.redirected, body, json.bodyUsed, reused],
                bytes: Array.from(new Uint8Array(await bytes.arrayBuffer())),
                missing: [missing.status, missing.ok, missing.statusText, missing.headers.get('x-status'), await missing.text(), immutable],
                redirect: [redirect.redirected, redirect.url === BASE + '/json', (await redirect.json()).a],
                bom,
                clone: (await clone.json()).s,
                headerNames: [...missing.headers.keys()].filter(k => k.startsWith('x-')),
            };
        }"#,
        &base,
    );
    let v = run_js_with_server(&source, &base).await;
    assert_eq!(
        v,
        json!({
            "json": [200, true, "OK", "application/json", "basic", false, { "a": 1, "b": [true, null], "s": "é" }, true, "TypeError"],
            "bytes": [0, 1, 2, 254, 255],
            "missing": [404, false, "Not Found", "404", "status body", "TypeError"],
            "redirect": [true, true, 1],
            "bom": "{\"x\":1}",
            "clone": "é",
            "headerNames": ["x-status"],
        })
    );
}

#[tokio::test]
async fn abort_rejects_with_abort_error_and_reasons() {
    let base = mock_server().await;
    let source = with_base(
        r#"export default async () => {
            const outcome = async promise => { try { await promise; return 'resolved'; } catch (e) { return typeof e === 'string' ? e : e.name; } };
            const pre = await outcome(fetch(BASE + '/json', { signal: AbortSignal.abort() }));
            const c1 = new AbortController();
            const p1 = fetch(BASE + '/slow', { signal: c1.signal });
            setTimeout(() => c1.abort(), 20);
            const t0 = Date.now();
            const during = await outcome(p1);
            const quick = Date.now() - t0 < 5000;
            const c2 = new AbortController();
            const p2 = fetch(BASE + '/slow', { signal: c2.signal });
            setTimeout(() => c2.abort('custom-reason'), 20);
            const custom = await outcome(p2);
            const timeout = await outcome(fetch(BASE + '/slow', { signal: AbortSignal.timeout(30) }));
            const c3 = new AbortController();
            const r = await fetch(BASE + '/json', { signal: c3.signal });
            c3.abort();
            const after = await outcome(r.text());
            return { pre, during, quick, custom, timeout, after };
        }"#,
        &base,
    );
    let v = run_js_with_server(&source, &base).await;
    assert_eq!(
        v,
        json!({ "pre": "AbortError", "during": "AbortError", "quick": true, "custom": "custom-reason", "timeout": "TimeoutError", "after": "AbortError" })
    );
}

#[tokio::test]
async fn abort_closes_the_connection_to_the_server() {
    // A raw server that reads one request, has the fetch aborted (through
    // the job's cancellation token and `__jaiAbort`, so never before the
    // request is in), and reports when the client hangs up.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let (closed_tx, closed_rx) = tokio::sync::oneshot::channel::<()>();
    let abort = tokio_util::sync::CancellationToken::new();
    let abort_now = abort.clone();
    tokio::spawn(async move {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        let mut request = Vec::new();
        let mut buf = [0u8; 1024];
        while !request.windows(4).any(|w| w == b"\r\n\r\n") {
            match socket.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(n) => request.extend_from_slice(&buf[..n]),
            }
        }
        abort_now.cancel();
        // Never answer; wait for end of file.
        loop {
            match socket.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
        let _ = closed_tx.send(());
        let _ = socket.shutdown().await;
    });
    let source = with_base(
        r#"export default async () => {
            const c = new AbortController();
            globalThis.__jaiAbort = () => c.abort();
            try { await fetch(BASE + '/hang', { signal: c.signal }); return 'resolved'; } catch (e) { return e.name; }
        }"#,
        &base,
    );
    let mut job = module_job(&source);
    job.server = Some(ServerRef {
        url: base.clone(),
        token: String::new(),
    });
    job.cancel = Some(abort);
    let v = run_job_async(job).await.unwrap();
    assert_eq!(v, "AbortError");
    tokio::time::timeout(Duration::from_secs(5), closed_rx)
        .await
        .expect("the server never saw the connection close")
        .expect("the server task ended without a request");
}

#[tokio::test]
async fn network_errors_are_type_errors() {
    // A port with nothing listening on it.
    let base = {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        format!("http://{}", listener.local_addr().unwrap())
    };
    let mut job = module_job(&with_base(
        r#"export default async () => { try { await fetch(BASE + '/x'); return 'resolved'; } catch (e) { return [e.name, e.message]; } }"#,
        &base,
    ));
    job.server = Some(ServerRef {
        url: base.clone(),
        token: String::new(),
    });
    let v = run_job_async(job).await.unwrap();
    assert_eq!(v[0], "TypeError");
    assert!(v[1].as_str().unwrap().starts_with("fetch failed: "), "{v}");
}
