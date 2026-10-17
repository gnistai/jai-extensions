//! `globalThis.__jai` (thumbnail, emit, saveRender) and `globalThis.__jaiAbort`
//! (DESIGN.md section 8, "Host functions the bundle calls").

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rquickjs::prelude::Func;
use rquickjs::{Ctx, Exception, Object, TypedArray, Value};

use crate::imageops;
use crate::types::EncodeSink;
use crate::web::buffer_source;

type OnEvent = Arc<dyn Fn(serde_json::Value) + Send + Sync>;

/// Installs `globalThis.__jai`. Without a sink (validate, render), `emit`
/// drops events and `saveRender` throws.
pub(crate) fn install<'js>(ctx: &Ctx<'js>, sink: Option<&EncodeSink>) -> rquickjs::Result<()> {
    let jai = Object::new(ctx.clone())?;
    jai.set("thumbnail", Func::from(thumbnail))?;
    let on_event: Option<OnEvent> = sink.map(|s| s.on_event.clone());
    jai.set(
        "emit",
        Func::from(move |ctx: Ctx<'js>, json: String| emit(&ctx, on_event.as_ref(), &json)),
    )?;
    let render_dir: Option<PathBuf> = sink.map(|s| s.render_dir.clone());
    jai.set(
        "saveRender",
        Func::from(move |ctx: Ctx<'js>, name: String, bytes: Value<'js>| {
            save_render(&ctx, render_dir.as_deref(), &name, &bytes)
        }),
    )?;
    ctx.globals().set("__jai", jai)
}

/// Calls `globalThis.__jaiAbort()` if the bundle has set it: Rust's side of
/// encode cancellation.
pub(crate) fn abort(ctx: &Ctx<'_>) {
    let hook = match ctx.globals().get::<_, Value>("__jaiAbort") {
        Ok(hook) => hook,
        Err(e) => {
            tracing::warn!(target: "jai_js", "cannot read __jaiAbort: {}", crate::runtime::js_error(ctx, e));
            return;
        }
    };
    match hook.as_function() {
        Some(f) => {
            if let Err(e) = f.call::<_, ()>(()) {
                let error = crate::runtime::js_error(ctx, e);
                #[cfg(test)]
                crate::web::test_log("warn", &format!("__jaiAbort threw: {error}"));
                tracing::warn!(target: "jai_js", "__jaiAbort threw: {error}");
            }
        }
        None => {
            tracing::debug!(target: "jai_js", "cancel requested before the bundle set __jaiAbort")
        }
    }
}

/// `__jai.thumbnail(bytes) -> {width, height, data}`: imageops::thumbnail.
fn thumbnail<'js>(ctx: Ctx<'js>, bytes: Value<'js>) -> rquickjs::Result<Object<'js>> {
    let data = buffer_source(&ctx, &bytes)?;
    let thumb = imageops::thumbnail(&data)
        .map_err(|e| Exception::throw_message(&ctx, &format!("thumbnail: {e:#}")))?;
    let out = Object::new(ctx.clone())?;
    out.set("width", thumb.width)?;
    out.set("height", thumb.height)?;
    out.set("data", TypedArray::new(ctx.clone(), thumb.data)?)?;
    Ok(out)
}

/// `__jai.emit(json)`: one encode event for the sink.
fn emit(ctx: &Ctx<'_>, on_event: Option<&OnEvent>, json: &str) -> rquickjs::Result<()> {
    let event: serde_json::Value = crate::json::from_js_text(json)
        .map_err(|e| Exception::throw_type(ctx, &format!("__jai.emit: invalid JSON: {e}")))?;
    match on_event {
        Some(on_event) => on_event(event),
        None => tracing::debug!(target: "jai_js", "encode event with no sink: {json}"),
    }
    Ok(())
}

/// `__jai.saveRender(name, bytes)`: writes `<render_dir>/<name>`.
fn save_render<'js>(
    ctx: &Ctx<'js>,
    dir: Option<&Path>,
    name: &str,
    bytes: &Value<'js>,
) -> rquickjs::Result<()> {
    let Some(dir) = dir else {
        return Err(Exception::throw_message(
            ctx,
            "__jai.saveRender is only available while encoding",
        ));
    };
    if !valid_render_name(name) {
        return Err(Exception::throw_type(
            ctx,
            &format!("__jai.saveRender: invalid file name {name:?}"),
        ));
    }
    let data = buffer_source(ctx, bytes)?;
    std::fs::create_dir_all(dir)
        .and_then(|()| std::fs::write(dir.join(name), &data))
        .map_err(|e| {
            Exception::throw_message(ctx, &format!("__jai.saveRender: cannot write {name}: {e}"))
        })
}

/// `^[A-Za-z0-9._-]{1,120}$` (DESIGN.md), and not a name Windows reserves or
/// one made only of dots.
pub(crate) fn valid_render_name(name: &str) -> bool {
    const RESERVED: [&str; 4] = ["con", "prn", "aux", "nul"];
    if name.is_empty()
        || name.len() > 120
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    {
        return false;
    }
    if name.bytes().all(|b| b == b'.') {
        return false;
    }
    let stem = name.split('.').next().unwrap_or("").to_ascii_lowercase();
    let device = RESERVED.contains(&stem.as_str())
        || (stem.len() == 4
            && (stem.starts_with("com") || stem.starts_with("lpt"))
            && stem.as_bytes()[3].is_ascii_digit()
            && stem.as_bytes()[3] != b'0');
    !device
}

#[cfg(test)]
mod tests {
    use super::valid_render_name;

    #[test]
    fn render_names() {
        for ok in [
            "001-c1-flux-schnell-s1.jpg",
            "a",
            "A.b_c-d",
            &"x".repeat(120),
            "console.png",
            "com0.jpg",
        ] {
            assert!(valid_render_name(ok), "{ok}");
        }
        for bad in [
            "",
            ".",
            "..",
            "...",
            "a/b.png",
            "a\\b",
            "x y",
            "é.png",
            &"x".repeat(121),
            "NUL",
            "nul.png",
            "COM1.jpg",
            "lpt9",
        ] {
            assert!(!valid_render_name(bad), "{bad}");
        }
    }
}
