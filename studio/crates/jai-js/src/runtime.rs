//! The QuickJS runtime for one call: its own OS thread with an 8 MiB stack, a
//! current-thread tokio runtime, `set_max_stack_size(4 MiB)`, the web globals,
//! and `HOST_JS` evaluated as an ES module (DESIGN.md section 8).
//!
//! Stopping a call: dropping the future of an async call stops its thread (an
//! interrupt handler ends running JavaScript and pending work is dropped). An
//! encode's cancellation token calls `globalThis.__jaiAbort()` so the bundle
//! can stop cleanly; if it has not finished `CANCEL_GRACE` later, the call is
//! stopped the hard way. A call may also limit how long its JavaScript runs
//! without a pause (`Job::max_busy`), so input crafted to make the library
//! slow (a regular expression that backtracks, say) cannot hold a thread and
//! a core forever. A runtime stopped mid-flight is leaked, not freed: QuickJS
//! asserts that no object is alive when a runtime is freed.

use std::future::Future;
use std::pin::pin;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context as _};
use rquickjs::function::IntoArgs;
use rquickjs::module::Evaluated;
use rquickjs::{
    Array, AsyncContext, AsyncRuntime, Coerced, Ctx, Function, Module, Object, TypedArray, Value,
};
use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::types::{
    DocInput, EncodeOptions, EncodeOutput, EncodeSink, RenderOptions, RenderOutput, ServerRef,
    ValidateResult,
};
use crate::{host, json, modules, web};

/// Stack of each JavaScript thread.
const THREAD_STACK: usize = 8 << 20;
/// QuickJS's own stack limit, well inside the thread's stack.
const JS_STACK: usize = 4 << 20;
/// How long an encode may take to wind down after `__jaiAbort()`.
#[cfg(not(test))]
const CANCEL_GRACE: Duration = Duration::from_secs(15);
/// Long enough that a loaded test machine does not end calls that do stop.
#[cfg(test)]
const CANCEL_GRACE: Duration = Duration::from_secs(1);
/// The module name of `HOST_JS`, as stack traces show it.
const HOST_MODULE: &str = "jai-host/host.mjs";
/// How long the public calls (validate, render, encode) let JavaScript run
/// without a pause. Waiting for a fetch or a timer is a pause.
#[cfg(not(test))]
pub(crate) const MAX_BUSY: Duration = Duration::from_secs(30);
#[cfg(test)]
pub(crate) const MAX_BUSY: Duration = Duration::from_secs(1);

/// Why the interrupt handler stopped a call (`Stop`), kept in an `AtomicU8`.
const RUNNING: u8 = 0;
const STOPPED: u8 = 1;
const TOO_BUSY: u8 = 2;

const CANCELED: &str = "canceled";
const THREAD_LOST: &str = "the JavaScript host thread stopped unexpectedly";

/// What a call runs.
pub(crate) enum Entry {
    Validate(DocInput),
    Render(DocInput, RenderOptions),
    Encode(EncodeOptions),
    /// An ES module (tests only). The result is its default export as JSON:
    /// called first when it is a function, awaited when it is a promise.
    #[cfg(test)]
    Module {
        name: String,
        source: String,
    },
}

/// What a call returns, by entry.
pub(crate) enum Output {
    Validate(ValidateResult),
    Render(RenderOutput),
    Encode(EncodeOutput),
    #[cfg(test)]
    Json(serde_json::Value),
}

/// Extra setup on the context after the globals are installed and before the
/// entry runs (used by the tests).
pub(crate) type Setup = Box<dyn for<'js> FnOnce(&Ctx<'js>) -> rquickjs::Result<()> + Send>;

/// One call: an entry and what its runtime gets.
pub(crate) struct Job {
    pub entry: Entry,
    /// The only origin `fetch` may reach.
    pub server: Option<ServerRef>,
    pub sink: Option<EncodeSink>,
    /// Fires `__jaiAbort()`.
    pub cancel: Option<CancellationToken>,
    /// The longest the JavaScript may run without a pause; the call then
    /// fails. None: no limit.
    pub max_busy: Option<Duration>,
    /// Extra ES modules by exact specifier: `(specifier, source)`.
    pub modules: Vec<(String, String)>,
    pub setup: Option<Setup>,
    /// Signalled when the thread has finished, however it finished.
    #[cfg(test)]
    pub on_exit: Option<std::sync::mpsc::Sender<()>>,
}

impl Job {
    pub(crate) fn new(entry: Entry) -> Job {
        Job {
            entry,
            server: None,
            sink: None,
            cancel: None,
            max_busy: None,
            modules: Vec::new(),
            setup: None,
            #[cfg(test)]
            on_exit: None,
        }
    }
}

/// Runs a job and blocks the calling thread until it ends.
pub(crate) fn run_blocking(job: Job) -> anyhow::Result<Output> {
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    spawn(job, CancellationToken::new(), move |result| {
        // The receiver only goes away if the caller is gone too.
        let _ = tx.send(result);
    })?;
    rx.recv().map_err(|_| anyhow!(THREAD_LOST))?
}

/// Runs a job; dropping the returned future stops it.
pub(crate) async fn run(job: Job) -> anyhow::Result<Output> {
    let kill = CancellationToken::new();
    let guard = kill.clone().drop_guard();
    let (tx, rx) = tokio::sync::oneshot::channel();
    spawn(job, kill, move |result| {
        // The receiver is gone when the caller dropped the call.
        let _ = tx.send(result);
    })?;
    let result = rx.await.map_err(|_| anyhow!(THREAD_LOST))?;
    guard.disarm();
    result
}

fn spawn(
    job: Job,
    kill: CancellationToken,
    done: impl FnOnce(anyhow::Result<Output>) + Send + 'static,
) -> anyhow::Result<()> {
    std::thread::Builder::new()
        .name("jai-js".to_owned())
        .stack_size(THREAD_STACK)
        .spawn(move || {
            #[cfg(test)]
            let (job, on_exit) = {
                let mut job = job;
                let on_exit = job.on_exit.take();
                (job, on_exit)
            };
            let result = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .context("cannot start the JavaScript host's async runtime")
                .and_then(|rt| rt.block_on(execute(job, kill)));
            done(result);
            #[cfg(test)]
            if let Some(on_exit) = on_exit {
                let _ = on_exit.send(());
            }
        })
        .context("cannot start the JavaScript host thread")?;
    Ok(())
}

async fn execute(job: Job, kill: CancellationToken) -> anyhow::Result<Output> {
    let Job {
        entry,
        server,
        sink,
        cancel,
        max_busy,
        modules,
        setup,
        ..
    } = job;
    if cancel.as_ref().is_some_and(CancellationToken::is_cancelled) {
        return Err(anyhow!(CANCELED));
    }
    let rt = AsyncRuntime::new().context("cannot create a QuickJS runtime")?;
    rt.set_max_stack_size(JS_STACK).await;
    rt.set_host_promise_rejection_tracker(Some(Box::new(track_rejection)))
        .await;
    // Set once the call is being stopped; the runtime is then leaked, not freed.
    let stop = Arc::new(AtomicU8::new(RUNNING));
    let clock = BusyClock::new();
    rt.set_interrupt_handler(Some(interrupt_handler(
        kill.clone(),
        cancel.clone(),
        max_busy.map(|limit| (clock.clone(), limit)),
        stop.clone(),
    )))
    .await;
    let (resolver, loader) = modules::loader(modules);
    rt.set_loader(resolver, loader).await;
    let ctx = AsyncContext::full(&rt)
        .await
        .context("cannot create a QuickJS context")?;

    let watched = cancel.clone();
    let overdue = async move {
        match watched {
            Some(token) => {
                token.cancelled().await;
                tokio::time::sleep(CANCEL_GRACE).await;
            }
            None => std::future::pending::<()>().await,
        }
    };
    let finished = {
        let work = ctx
            .async_with(async move |ctx| run_entry(ctx, entry, server, sink, cancel, setup).await);
        // All JavaScript runs inside polls of `work`: the clock marks them.
        let work = clock.timed(work);
        tokio::select! {
            result = work => Some(result),
            () = kill.cancelled() => None,
            () = overdue => None,
        }
    };
    match finished {
        Some(result) if !kill.is_cancelled() && stop.load(Ordering::Relaxed) == RUNNING => {
            drop(ctx);
            drop(rt);
            result
        }
        _ => {
            std::mem::forget(ctx);
            std::mem::forget(rt);
            match (stop.load(Ordering::Relaxed), max_busy) {
                (TOO_BUSY, Some(limit)) => Err(anyhow!(too_busy(limit))),
                _ => Err(anyhow!(CANCELED)),
            }
        }
    }
}

/// The error of a call whose JavaScript ran past its `max_busy`.
pub(crate) fn too_busy(limit: Duration) -> String {
    format!(
        "stopped after {} s of JavaScript without a pause: the input takes too long to process",
        limit.as_secs_f64()
    )
}

/// When the current poll of a call began, while one runs: the JavaScript
/// has run without a pause since then.
#[derive(Clone)]
struct BusyClock {
    epoch: Instant,
    /// Nanoseconds from `epoch` to the start of the running poll, plus one;
    /// 0 between polls.
    since: Arc<AtomicU64>,
}

impl BusyClock {
    fn new() -> BusyClock {
        BusyClock {
            epoch: Instant::now(),
            since: Arc::new(AtomicU64::new(0)),
        }
    }

    /// `future`, with each of its polls marked.
    fn timed<F: Future>(&self, future: F) -> impl Future<Output = F::Output> {
        let clock = self.clone();
        async move {
            let mut future = pin!(future);
            std::future::poll_fn(|cx| {
                let started = u64::try_from(clock.epoch.elapsed().as_nanos()).unwrap_or(u64::MAX);
                clock
                    .since
                    .store(started.saturating_add(1), Ordering::Relaxed);
                let poll = future.as_mut().poll(cx);
                clock.since.store(0, Ordering::Relaxed);
                poll
            })
            .await
        }
    }

    /// How long the running poll has taken; zero between polls.
    fn busy_for(&self) -> Duration {
        match self.since.load(Ordering::Relaxed) {
            0 => Duration::ZERO,
            since => self
                .epoch
                .elapsed()
                .saturating_sub(Duration::from_nanos(since - 1)),
        }
    }
}

/// Ends running JavaScript (an uncatchable exception) when the call is dropped,
/// when an encode is still running `CANCEL_GRACE` after its cancellation, or
/// when the JavaScript has run past `busy`'s limit without a pause (then it
/// also cancels `kill`, so that the call ends even if what it waits for never
/// settles). Timers cannot fire while JavaScript runs, so the handler keeps
/// the time. Once it has stopped a call it keeps stopping it. QuickJS calls
/// it often while JavaScript runs, regular expressions included.
fn interrupt_handler(
    kill: CancellationToken,
    cancel: Option<CancellationToken>,
    busy: Option<(BusyClock, Duration)>,
    stop: Arc<AtomicU8>,
) -> rquickjs::runtime::InterruptHandler {
    let mut cancelled_at: Option<Instant> = None;
    Box::new(move || {
        if stop.load(Ordering::Relaxed) != RUNNING {
            return true;
        }
        let overdue = match &cancel {
            Some(token) if token.is_cancelled() => {
                cancelled_at.get_or_insert_with(Instant::now).elapsed() > CANCEL_GRACE
            }
            _ => false,
        };
        if kill.is_cancelled() || overdue {
            stop.store(STOPPED, Ordering::Relaxed);
            return true;
        }
        if let Some((clock, limit)) = &busy {
            if clock.busy_for() > *limit {
                stop.store(TOO_BUSY, Ordering::Relaxed);
                kill.cancel();
                return true;
            }
        }
        false
    })
}

async fn run_entry<'js>(
    ctx: Ctx<'js>,
    entry: Entry,
    server: Option<ServerRef>,
    sink: Option<EncodeSink>,
    cancel: Option<CancellationToken>,
    setup: Option<Setup>,
) -> anyhow::Result<Output> {
    web::install(&ctx, server.as_ref()).js(&ctx)?;
    host::install(&ctx, sink.as_ref()).js(&ctx)?;
    if let Some(setup) = setup {
        setup(&ctx).js(&ctx)?;
    }
    if let Some(token) = cancel.clone() {
        let hook = ctx.clone();
        ctx.spawn(async move {
            token.cancelled().await;
            host::abort(&hook);
        });
    }
    match entry {
        Entry::Validate(input) => {
            let host = load_host(&ctx).await?;
            let input = doc_input(&ctx, input).js(&ctx)?;
            let value = call(&ctx, &host, "validate", (input,)).await?;
            Ok(Output::Validate(from_js(&ctx, value)?))
        }
        Entry::Render(input, options) => {
            let host = load_host(&ctx).await?;
            let input = doc_input(&ctx, input).js(&ctx)?;
            let options = to_js(&ctx, &options)?;
            let value = call(&ctx, &host, "render", (input, options)).await?;
            let bytes = take_bytes(&ctx, &value, "bytes")?;
            let mut out: RenderOutput = from_js(&ctx, value)?;
            out.bytes = bytes;
            Ok(Output::Render(out))
        }
        Entry::Encode(options) => {
            let host = load_host(&ctx).await?;
            let options = encode_options(&ctx, &options)?;
            if cancel.as_ref().is_some_and(CancellationToken::is_cancelled) {
                return Err(anyhow!(CANCELED));
            }
            let value = call(&ctx, &host, "encode", (options,)).await?;
            let jai = take_bytes(&ctx, &value, "jai")?;
            let mut out: EncodeOutput = from_js(&ctx, value)?;
            out.jai = jai;
            Ok(Output::Encode(out))
        }
        #[cfg(test)]
        Entry::Module { name, source } => {
            let (module, evaluated) = Module::declare(ctx.clone(), name, source)
                .js(&ctx)?
                .eval()
                .js(&ctx)?;
            evaluated.into_future::<()>().await.js(&ctx)?;
            let export: Value = module.get("default").js(&ctx)?;
            let value = match export.as_function() {
                Some(f) => settle(&ctx, f.call(()).js(&ctx)?).await?,
                None => settle(&ctx, export).await?,
            };
            Ok(Output::Json(if value.is_undefined() {
                serde_json::Value::Null
            } else {
                from_js(&ctx, value)?
            }))
        }
    }
}

/// Evaluates `HOST_JS`.
async fn load_host<'js>(ctx: &Ctx<'js>) -> anyhow::Result<Module<'js, Evaluated>> {
    let (module, evaluated) = Module::declare(ctx.clone(), HOST_MODULE, crate::HOST_JS)
        .js(ctx)?
        .eval()
        .js(ctx)?;
    evaluated.into_future::<()>().await.js(ctx)?;
    Ok(module)
}

/// Calls one of the host module's exports and waits for its result.
async fn call<'js, A: IntoArgs<'js>>(
    ctx: &Ctx<'js>,
    host: &Module<'js, Evaluated>,
    name: &str,
    args: A,
) -> anyhow::Result<Value<'js>> {
    let function: Function = host
        .get(name)
        .js(ctx)
        .with_context(|| format!("host.mjs has no function {name}"))?;
    let value: Value = function.call(args).js(ctx)?;
    settle(ctx, value).await
}

/// The value itself, or what it resolves to when it is a promise.
async fn settle<'js>(ctx: &Ctx<'js>, value: Value<'js>) -> anyhow::Result<Value<'js>> {
    match value.as_promise() {
        Some(promise) => promise.clone().into_future::<Value>().await.js(ctx),
        None => Ok(value),
    }
}

/// `{bytes: Uint8Array}` or `{text: string}`.
fn doc_input<'js>(ctx: &Ctx<'js>, input: DocInput) -> rquickjs::Result<Value<'js>> {
    let object = Object::new(ctx.clone())?;
    match input {
        DocInput::Bytes(bytes) => object.set("bytes", TypedArray::new(ctx.clone(), bytes)?)?,
        DocInput::Text(text) => object.set("text", text)?,
    }
    Ok(object.into_value())
}

/// A serde value as a JavaScript value (through JSON).
fn to_js<'js, T: Serialize>(ctx: &Ctx<'js>, value: &T) -> anyhow::Result<Value<'js>> {
    let text = serde_json::to_string(value).context("cannot serialize the host arguments")?;
    ctx.json_parse(text).js(ctx)
}

/// EncodeHostOptions, with `targetThumbnail.data` as a Uint8Array.
fn encode_options<'js>(ctx: &Ctx<'js>, options: &EncodeOptions) -> anyhow::Result<Value<'js>> {
    let mut light = options.clone();
    let data = std::mem::take(&mut light.target_thumbnail.data);
    let value = to_js(ctx, &light)?;
    let thumb: Object = value
        .as_object()
        .ok_or_else(|| anyhow!("the encode options are not an object"))?
        .get("targetThumbnail")
        .js(ctx)?;
    thumb
        .set("data", TypedArray::new(ctx.clone(), data).js(ctx)?)
        .js(ctx)?;
    Ok(value)
}

/// Copies binary field `key` out of a host result and replaces it by an empty
/// array, so the rest can go through JSON.
fn take_bytes<'js>(ctx: &Ctx<'js>, value: &Value<'js>, key: &str) -> anyhow::Result<Vec<u8>> {
    let object = value
        .as_object()
        .ok_or_else(|| anyhow!("the host returned no object"))?;
    let field: Value = object.get(key).js(ctx)?;
    let bytes = web::buffer_source(ctx, &field)
        .js(ctx)
        .with_context(|| format!("the host result's {key} is not binary"))?;
    object.set(key, Array::new(ctx.clone()).js(ctx)?).js(ctx)?;
    Ok(bytes)
}

/// A JavaScript value as a serde type (through JSON).
fn from_js<'js, T: DeserializeOwned>(ctx: &Ctx<'js>, value: Value<'js>) -> anyhow::Result<T> {
    let text = ctx
        .json_stringify(value)
        .js(ctx)?
        .ok_or_else(|| anyhow!("the host returned undefined"))?
        .to_string()
        .js(ctx)?;
    json::from_js_text(&text).context("the host returned an unexpected value")
}

/// rquickjs errors as anyhow errors, with the JavaScript exception described.
pub(crate) trait JsResultExt<T> {
    fn js(self, ctx: &Ctx<'_>) -> anyhow::Result<T>;
}

impl<T> JsResultExt<T> for rquickjs::Result<T> {
    fn js(self, ctx: &Ctx<'_>) -> anyhow::Result<T> {
        self.map_err(|e| js_error(ctx, e))
    }
}

/// An rquickjs error as an anyhow error; a pending exception is taken and described.
pub(crate) fn js_error(ctx: &Ctx<'_>, error: rquickjs::Error) -> anyhow::Error {
    match error {
        rquickjs::Error::Exception => anyhow!("{}", describe(ctx, &ctx.catch())),
        other => anyhow!("{other}"),
    }
}

/// A thrown JavaScript value as text: `name: message` and the stack for errors.
pub(crate) fn describe<'js>(ctx: &Ctx<'js>, value: &Value<'js>) -> String {
    if let Some(object) = value.as_object() {
        let name = text_property(ctx, object, "name");
        let message = text_property(ctx, object, "message");
        if name.is_some() || message.is_some() {
            let mut out = match (name, message) {
                (Some(n), Some(m)) if !m.is_empty() => format!("{n}: {m}"),
                (Some(n), _) => n,
                (None, m) => m.unwrap_or_default(),
            };
            if let Some(stack) = text_property(ctx, object, "stack") {
                let stack = stack.trim_end();
                if !stack.is_empty() {
                    out.push('\n');
                    out.push_str(stack);
                }
            }
            return out;
        }
        match ctx.json_stringify(value.clone()) {
            Ok(Some(text)) => {
                if let Ok(text) = text.to_string() {
                    return text;
                }
            }
            Ok(None) => {}
            Err(_) => drop(ctx.catch()),
        }
    }
    match value.get::<Coerced<String>>() {
        Ok(Coerced(text)) => text,
        Err(_) => {
            drop(ctx.catch());
            format!("<{}>", value.type_of().as_str())
        }
    }
}

/// A string property, or None (exceptions from getters are swallowed).
fn text_property<'js>(ctx: &Ctx<'js>, object: &Object<'js>, key: &str) -> Option<String> {
    match object.get::<_, Value>(key) {
        Ok(value) => value.as_string().and_then(|s| s.to_string().ok()),
        Err(_) => {
            drop(ctx.catch());
            None
        }
    }
}

/// Unhandled promise rejections are logged; rquickjs would print them.
fn track_rejection<'js>(ctx: Ctx<'js>, _promise: Value<'js>, reason: Value<'js>, is_handled: bool) {
    if !is_handled {
        let reason = describe(&ctx, &reason);
        #[cfg(test)]
        web::test_log("warn", &format!("unhandled promise rejection: {reason}"));
        tracing::warn!(target: "jai_js", "unhandled promise rejection: {reason}");
    }
}
