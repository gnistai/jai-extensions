//! The runtime's lifecycle: cancellation through `__jaiAbort`, the hard stop,
//! dropped calls, work left pending at exit, errors, and the `__jai` host
//! functions.

use std::io::Read as _;
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use super::{module_job, run_job, run_job_async, run_js};
use crate::runtime::{Job, MAX_BUSY};
use crate::web::LOG_LINES;
use crate::{DocInput, EncodeSink, ServerRef};

fn cancel_after(token: &CancellationToken, delay: Duration) {
    let token = token.clone();
    std::thread::spawn(move || {
        std::thread::sleep(delay);
        token.cancel();
    });
}

/// Gives the job a global `ready()` and cancels `token` once the JavaScript
/// has called it: a cancel after a fixed delay can come, on a busy machine,
/// before the JavaScript has set `__jaiAbort`.
fn cancel_when_ready(job: &mut Job, token: &CancellationToken) {
    let (tx, rx) = mpsc::channel::<()>();
    job.setup = Some(Box::new(move |ctx: &rquickjs::Ctx<'_>| {
        ctx.globals().set(
            "ready",
            rquickjs::prelude::Func::from(move || {
                let _ = tx.send(());
            }),
        )
    }));
    let token = token.clone();
    std::thread::spawn(move || {
        if rx.recv().is_ok() {
            token.cancel();
        }
    });
}

#[test]
fn cancelling_calls_the_abort_hook() {
    let token = CancellationToken::new();
    let mut job = module_job(
        r#"export default () => new Promise(resolve => { globalThis.__jaiAbort = () => resolve('aborted'); ready(); })"#,
    );
    job.cancel = Some(token.clone());
    cancel_when_ready(&mut job, &token);
    assert_eq!(run_job(job).unwrap(), "aborted");
}

#[test]
fn a_cancel_nobody_handles_stops_after_the_grace_period() {
    let token = CancellationToken::new();
    let mut job = module_job("export default () => new Promise(() => {})");
    job.cancel = Some(token.clone());
    cancel_after(&token, Duration::from_millis(50));
    let started = Instant::now();
    let e = run_job(job).unwrap_err();
    assert_eq!(e.to_string(), "canceled");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn a_busy_loop_is_stopped_after_the_grace_period() {
    let token = CancellationToken::new();
    let mut job = module_job("export default () => { for (;;) {} }");
    job.cancel = Some(token.clone());
    cancel_after(&token, Duration::from_millis(50));
    let started = Instant::now();
    assert_eq!(run_job(job).unwrap_err().to_string(), "canceled");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn an_abort_hook_that_throws_is_logged() {
    let token = CancellationToken::new();
    let mut job = module_job(
        r#"export default () => new Promise(resolve => {
            globalThis.__jaiAbort = () => { setTimeout(() => resolve('still running'), 10); throw new Error('abort-hook-marker'); };
            ready();
        })"#,
    );
    job.cancel = Some(token.clone());
    cancel_when_ready(&mut job, &token);
    assert_eq!(run_job(job).unwrap(), "still running");
    let lines = LOG_LINES.lock().unwrap().clone();
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("warn: __jaiAbort threw: Error: abort-hook-marker")),
        "{lines:?}"
    );
}

#[test]
fn a_cancelled_token_never_starts_the_call() {
    let token = CancellationToken::new();
    token.cancel();
    let mut job = module_job("export default () => 1");
    job.cancel = Some(token);
    assert_eq!(run_job(job).unwrap_err().to_string(), "canceled");
}

/// Drops the call after `after` and waits for its thread to end.
async fn drop_and_wait(source: &str, after: Duration) {
    let (tx, rx) = mpsc::channel();
    let mut job = module_job(source);
    job.on_exit = Some(tx);
    assert!(
        tokio::time::timeout(after, run_job_async(job))
            .await
            .is_err(),
        "the call finished before it was dropped"
    );
    tokio::task::spawn_blocking(move || rx.recv_timeout(Duration::from_secs(5)))
        .await
        .unwrap()
        .expect("the JavaScript thread kept running after the call was dropped");
}

#[tokio::test]
async fn dropping_a_call_interrupts_running_javascript() {
    drop_and_wait(
        "export default () => { for (;;) {} }",
        Duration::from_millis(200),
    )
    .await;
}

#[tokio::test]
async fn dropping_a_call_stops_pending_work() {
    drop_and_wait(
        "export default async () => { await new Promise(r => setTimeout(r, 60_000)); }",
        Duration::from_millis(200),
    )
    .await;
}

#[test]
fn work_left_pending_at_exit_is_freed() {
    // A server that accepts connections and never answers.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        let mut held = Vec::new();
        for mut stream in listener.incoming().flatten() {
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf);
            held.push(stream);
        }
    });
    let mut job = module_job(
        r#"export default async () => {
            setTimeout(() => {}, 60_000);
            setInterval(() => {}, 5);
            const signal = new AbortController().signal;
            fetch(URL_BASE + '/hang', { signal }).catch(() => {});
            fetch(URL_BASE + '/hang').catch(() => {});
            AbortSignal.timeout(60_000);
            await new Promise(r => setTimeout(r, 50));
            return 'done';
        }"#
        .replace("URL_BASE", &serde_json::to_string(&url).unwrap())
        .as_str(),
    );
    job.server = Some(ServerRef {
        url,
        token: String::new(),
    });
    // Freeing the runtime asserts that no JavaScript object is still alive; a
    // leak would abort the test process here.
    assert_eq!(run_job(job).unwrap(), "done");
}

/// A module job whose JavaScript may run `limit` without a pause.
fn limited(source: &str, limit: Duration) -> Job {
    let mut job = module_job(source);
    job.max_busy = Some(limit);
    job
}

fn assert_too_busy(result: anyhow::Result<Value>, started: Instant) {
    let e = result.unwrap_err();
    assert!(e.to_string().contains("without a pause"), "{e:#}");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
}

/// SERVER-JS-1: JavaScript that never pauses is stopped at the limit.
#[test]
fn javascript_that_never_pauses_is_stopped() {
    let started = Instant::now();
    let job = limited(
        "export default () => { for (;;) {} }",
        Duration::from_millis(100),
    );
    assert_too_busy(run_job(job), started);
}

/// QuickJS's regular expression engine checks the limit while it
/// backtracks.
#[test]
fn a_backtracking_regular_expression_is_stopped() {
    let started = Instant::now();
    let job = limited(
        "export default () => /^(a+)+$/.test('a'.repeat(48) + 'b')",
        Duration::from_millis(100),
    );
    assert_too_busy(run_job(job), started);
}

/// The call ends even when the promise it waits for can no longer settle.
#[test]
fn a_busy_timer_callback_ends_the_call() {
    let started = Instant::now();
    let job = limited(
        "export default () => new Promise(() => { setTimeout(() => { for (;;) {} }, 10); })",
        Duration::from_millis(100),
    );
    assert_too_busy(run_job(job), started);
}

/// Waiting for timers (or fetches) is a pause: the clock restarts after it.
#[test]
fn waiting_is_not_running() {
    let job = limited(
        "export default async () => { for (let i = 0; i < 15; i++) await new Promise(r => setTimeout(r, 100)); return 'waited'; }",
        Duration::from_secs(1),
    );
    assert_eq!(run_job(job).unwrap(), "waited");
}

/// SERVER-JS-1: a document crafted to make the library's newline check
/// backtrack (many newlines in one value) cannot hold `validate` for long,
/// whether or not the library's regular expression is linear yet.
#[test]
fn a_document_crafted_to_be_slow_cannot_hold_validate() {
    let doc = format!("{{\"prompt\":\"a{}b\"}}", "\\n".repeat(200_000));
    let started = Instant::now();
    if let Err(e) = crate::validate(DocInput::Text(doc)) {
        assert!(e.to_string().contains("without a pause"), "{e:#}");
    }
    assert!(
        started.elapsed() < MAX_BUSY + Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn deep_recursion_is_an_exception_not_a_crash() {
    let v = run_js("export default () => { const f = n => f(n + 1) + 1; try { f(0); return 'returned'; } catch (e) { return e.name; } }");
    assert!(v == "RangeError" || v == "InternalError", "{v}");
}

#[test]
fn exceptions_are_described_with_their_stack() {
    let text = |source: &str| format!("{:#}", run_job(module_job(source)).unwrap_err());
    let thrown = text("export default () => { throw new RangeError('boom-marker'); }");
    assert!(thrown.starts_with("RangeError: boom-marker\n"), "{thrown}");
    assert!(thrown.contains("test.mjs"), "{thrown}");
    assert_eq!(
        text("export default async () => { throw { code: 7 }; }"),
        r#"{"code":7}"#
    );
    assert_eq!(text("export default () => { throw 'plain'; }"), "plain");
    assert!(text("export default (").starts_with("SyntaxError"));
    assert!(
        text("import 'node:fs';").contains("node:fs"),
        "unknown modules do not resolve"
    );
}

#[test]
fn unhandled_rejections_are_logged() {
    assert_eq!(run_js("export default () => { Promise.reject(new Error('unhandled-rejection-marker')); return 1; }"), 1);
    let lines = LOG_LINES.lock().unwrap().clone();
    assert!(
        lines.iter().any(|l| l
            .starts_with("warn: unhandled promise rejection: Error: unhandled-rejection-marker")),
        "{lines:?}"
    );
}

#[test]
fn calls_run_in_parallel_on_their_own_runtimes() {
    let handles: Vec<_> = (0..8)
        .map(|i| std::thread::spawn(move || run_js(&format!("globalThis.mine = {i}; export default async () => {{ await null; return globalThis.mine * 2; }}"))))
        .collect();
    let results: Vec<Value> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(results, (0..8).map(|i| json!(i * 2)).collect::<Vec<_>>());
}

fn png_base64() -> String {
    let img = image::RgbaImage::from_fn(40, 20, |x, _| {
        if x < 20 {
            image::Rgba([255, 0, 0, 255])
        } else {
            image::Rgba([0, 0, 255, 255])
        }
    });
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png).unwrap();
    base64::engine::general_purpose::STANDARD.encode(out.into_inner())
}

#[test]
fn jai_host_functions_with_a_sink() {
    let dir = tempfile::tempdir().unwrap();
    let render_dir = dir.path().join("renders");
    let events = Arc::new(Mutex::new(Vec::new()));
    let seen = events.clone();
    let mut job = module_job(
        &r#"export default () => {
            const png = Uint8Array.fromBase64(PNG);
            const t = __jai.thumbnail(png);
            __jai.emit(JSON.stringify({ type: 'log', message: 'lone \ud800 surrogate' }));
            __jai.saveRender('001-l1g0c1-s1.png', png);
            __jai.saveRender('002-l1g0c2-s1.png', png.buffer);
            const errors = [];
            for (const f of [() => __jai.saveRender('../escape.png', png), () => __jai.saveRender('a/b.png', png), () => __jai.saveRender('nul.png', png),
                             () => __jai.saveRender('x.png', 'not bytes'), () => __jai.thumbnail(new Uint8Array([1, 2, 3])), () => __jai.emit('not json')]) {
                try { f(); errors.push('ok'); } catch (e) { errors.push(e.name); }
            }
            return { thumb: [t.width, t.height, t.data.length, t.data instanceof Uint8Array, Array.from(t.data.subarray(0, 4))], errors };
        }"#
        .replace("PNG", &serde_json::to_string(&png_base64()).unwrap()),
    );
    job.sink = Some(EncodeSink {
        on_event: Arc::new(move |e| seen.lock().unwrap().push(e)),
        render_dir: render_dir.clone(),
    });
    let v = run_job(job).unwrap();
    // Centre square of red|blue: the left half of the thumbnail is red.
    assert_eq!(v["thumb"], json!([16, 16, 1024, true, [255, 0, 0, 255]]));
    assert_eq!(
        v["errors"],
        json!([
            "TypeError",
            "TypeError",
            "TypeError",
            "TypeError",
            "Error",
            "TypeError"
        ])
    );
    assert_eq!(
        *events.lock().unwrap(),
        vec![json!({ "type": "log", "message": "lone \u{fffd} surrogate" })]
    );
    let mut files: Vec<String> = std::fs::read_dir(&render_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    files.sort();
    assert_eq!(files, ["001-l1g0c1-s1.png", "002-l1g0c2-s1.png"]);
    assert!(!dir.path().join("escape.png").exists());
}

#[test]
fn jai_host_functions_without_a_sink() {
    let v = run_js(
        r#"export default () => {
            __jai.emit('{"type":"log"}');
            try { __jai.saveRender('a.png', new Uint8Array(1)); return 'saved'; } catch (e) { return e.message; }
        }"#,
    );
    assert_eq!(v, "__jai.saveRender is only available while encoding");
}
