//! The modules a host runtime can import: the synthetic `node:zlib`
//! (deflateRawSync, inflateRawSync) and `node:crypto` (`{ webcrypto: crypto }`)
//! of DESIGN.md section 8, plus extra ES modules registered by exact
//! specifier (the conformance test maps the runner's dynamic import of
//! `file:///…/dist/jai.mjs` this way). Nothing is read from disk here.

use std::collections::HashMap;
use std::io::Write;

use flate2::write::DeflateEncoder;
use flate2::{Compression, Decompress, FlushDecompress, Status};
use rquickjs::loader::{ImportAttributes, Loader, Resolver};
use rquickjs::module::{Declarations, Declared, Exports, ModuleDef};
use rquickjs::prelude::Opt;
use rquickjs::{Ctx, Exception, Function, Module, Object, TypedArray, Value};
use url::Url;

use crate::web::buffer_source;

/// Largest output `inflateRawSync` produces unless `maxOutputLength` says otherwise.
const MAX_INFLATE: usize = 256 << 20;

const ZLIB: &str = "node:zlib";
const CRYPTO: &str = "node:crypto";

/// The canonical name of a specifier: synthetic modules accept the bare Node
/// names too; URLs are compared in their WHATWG serialisation.
fn canonical(name: &str) -> String {
    match name {
        "zlib" | ZLIB => ZLIB.to_owned(),
        "crypto" | CRYPTO => CRYPTO.to_owned(),
        _ => Url::parse(name)
            .map(String::from)
            .unwrap_or_else(|_| name.to_owned()),
    }
}

/// Resolves the synthetic modules and the registered extra modules.
pub(crate) struct HostResolver {
    extra: Vec<String>,
}

/// Loads the synthetic modules and the registered extra modules.
pub(crate) struct HostLoader {
    extra: HashMap<String, String>,
}

/// The resolver and loader for one runtime; `extra` is `(specifier, source)`.
pub(crate) fn loader(extra: Vec<(String, String)>) -> (HostResolver, HostLoader) {
    let extra: HashMap<String, String> = extra
        .into_iter()
        .map(|(name, source)| (canonical(&name), source))
        .collect();
    (
        HostResolver {
            extra: extra.keys().cloned().collect(),
        },
        HostLoader { extra },
    )
}

impl Resolver for HostResolver {
    fn resolve<'js>(
        &mut self,
        _ctx: &Ctx<'js>,
        base: &str,
        name: &str,
        _attributes: Option<ImportAttributes<'js>>,
    ) -> rquickjs::Result<String> {
        let key = canonical(name);
        if key == ZLIB || key == CRYPTO || self.extra.contains(&key) {
            Ok(key)
        } else {
            Err(rquickjs::Error::new_resolving(base, name))
        }
    }
}

impl Loader for HostLoader {
    fn load<'js>(
        &mut self,
        ctx: &Ctx<'js>,
        name: &str,
        _attributes: Option<ImportAttributes<'js>>,
    ) -> rquickjs::Result<Module<'js, Declared>> {
        match name {
            ZLIB => Module::declare_def::<ZlibModule, _>(ctx.clone(), name),
            CRYPTO => Module::declare_def::<CryptoModule, _>(ctx.clone(), name),
            _ => match self.extra.get(name) {
                Some(source) => Module::declare(ctx.clone(), name, source.as_str()),
                None => Err(rquickjs::Error::new_loading(name)),
            },
        }
    }
}

/// `node:zlib`: `deflateRawSync(data, {level})` and `inflateRawSync(data,
/// {maxOutputLength})`, returning Uint8Arrays.
struct ZlibModule;

impl ModuleDef for ZlibModule {
    fn declare<'js>(decl: &Declarations<'js>) -> rquickjs::Result<()> {
        decl.declare("deflateRawSync")?;
        decl.declare("inflateRawSync")?;
        decl.declare("default")?;
        Ok(())
    }

    fn evaluate<'js>(ctx: &Ctx<'js>, exports: &Exports<'js>) -> rquickjs::Result<()> {
        let deflate = Function::new(ctx.clone(), deflate_raw_sync)?.with_name("deflateRawSync")?;
        let inflate = Function::new(ctx.clone(), inflate_raw_sync)?.with_name("inflateRawSync")?;
        let default = Object::new(ctx.clone())?;
        default.set("deflateRawSync", deflate.clone())?;
        default.set("inflateRawSync", inflate.clone())?;
        exports.export("deflateRawSync", deflate)?;
        exports.export("inflateRawSync", inflate)?;
        exports.export("default", default)?;
        Ok(())
    }
}

/// Node accepts strings (as UTF-8) as well as buffers.
fn zlib_input<'js>(ctx: &Ctx<'js>, data: &Value<'js>) -> rquickjs::Result<Vec<u8>> {
    match data.as_string() {
        Some(s) => Ok(s.to_string()?.into_bytes()),
        None => buffer_source(ctx, data),
    }
}

/// A numeric option, or None when absent.
fn number_option<'js>(options: &Opt<Value<'js>>, key: &str) -> rquickjs::Result<Option<f64>> {
    match options.0.as_ref().and_then(Value::as_object) {
        Some(o) => o.get::<_, Option<f64>>(key),
        None => Ok(None),
    }
}

fn deflate_raw_sync<'js>(
    ctx: Ctx<'js>,
    data: Value<'js>,
    options: Opt<Value<'js>>,
) -> rquickjs::Result<TypedArray<'js, u8>> {
    let input = zlib_input(&ctx, &data)?;
    let level = number_option(&options, "level")?.unwrap_or(-1.0);
    if level.fract() != 0.0 || !(-1.0..=9.0).contains(&level) {
        return Err(Exception::throw_range(
            &ctx,
            &format!("The value of \"options.level\" is out of range. It must be >= -1 and <= 9. Received {level}"),
        ));
    }
    // -1 is zlib's Z_DEFAULT_COMPRESSION (level 6); the check above makes the cast exact.
    let compression = if level < 0.0 {
        Compression::default()
    } else {
        Compression::new(level as u32)
    };
    let mut encoder = DeflateEncoder::new(Vec::with_capacity(input.len() / 2 + 64), compression);
    let out = encoder
        .write_all(&input)
        .and_then(|()| encoder.finish())
        .map_err(|e| Exception::throw_message(&ctx, &format!("deflateRawSync: {e}")))?;
    TypedArray::new(ctx, out)
}

fn inflate_raw_sync<'js>(
    ctx: Ctx<'js>,
    data: Value<'js>,
    options: Opt<Value<'js>>,
) -> rquickjs::Result<TypedArray<'js, u8>> {
    let input = zlib_input(&ctx, &data)?;
    let max = match number_option(&options, "maxOutputLength")? {
        Some(n) if n >= 1.0 && n.fract() == 0.0 => n.min(MAX_INFLATE as f64) as usize,
        Some(n) => {
            return Err(Exception::throw_range(
                &ctx,
                &format!("The value of \"options.maxOutputLength\" is out of range. It must be >= 1. Received {n}"),
            ))
        }
        None => MAX_INFLATE,
    };
    match inflate_raw(&input, max) {
        Ok(out) => TypedArray::new(ctx, out),
        Err(InflateError::TooLarge) => Err(Exception::throw_range(
            &ctx,
            &format!("Cannot create a Buffer larger than {max} bytes"),
        )),
        Err(InflateError::Truncated) => {
            Err(zlib_error(&ctx, "unexpected end of file", "Z_BUF_ERROR"))
        }
        Err(InflateError::Data(message)) => Err(zlib_error(&ctx, &message, "Z_DATA_ERROR")),
    }
}

/// An Error with Node's `code` property.
fn zlib_error(ctx: &Ctx<'_>, message: &str, code: &str) -> rquickjs::Error {
    match Exception::from_message(ctx.clone(), message) {
        Ok(error) => {
            if let Err(e) = error.set("code", code) {
                return e;
            }
            error.throw()
        }
        Err(e) => e,
    }
}

#[derive(Debug)]
enum InflateError {
    TooLarge,
    Truncated,
    Data(String),
}

/// Inflates a raw DEFLATE stream (no zlib or gzip header) of at most `max`
/// bytes. Bytes after the end of the stream are ignored.
fn inflate_raw(input: &[u8], max: usize) -> Result<Vec<u8>, InflateError> {
    // One byte over the limit is enough to tell that the output is too large.
    let limit = max.saturating_add(1);
    let mut d = Decompress::new(false);
    let mut out: Vec<u8> = Vec::with_capacity(input.len().saturating_mul(4).max(64).min(limit));
    loop {
        if out.len() == out.capacity() {
            if out.len() > max {
                return Err(InflateError::TooLarge);
            }
            out.reserve(out.len().max(64).min(limit - out.len()));
        }
        let (before_in, before_out) = (d.total_in(), d.total_out());
        let consumed = usize::try_from(before_in)
            .unwrap_or(usize::MAX)
            .min(input.len());
        // FlushDecompress::None: miniz_oxide treats Finish on a first call whose
        // output buffer is too small as a permanent failure.
        let status = d
            .decompress_vec(&input[consumed..], &mut out, FlushDecompress::None)
            .map_err(|e| InflateError::Data(e.to_string()))?;
        if out.len() > max {
            return Err(InflateError::TooLarge);
        }
        match status {
            Status::StreamEnd => return Ok(out),
            Status::Ok | Status::BufError => {
                let progressed = d.total_in() != before_in || d.total_out() != before_out;
                if !progressed && out.len() < out.capacity() {
                    return Err(InflateError::Truncated);
                }
            }
        }
    }
}

/// `node:crypto`: `{ webcrypto, subtle, getRandomValues, randomUUID }` over the
/// global `crypto`.
struct CryptoModule;

impl ModuleDef for CryptoModule {
    fn declare<'js>(decl: &Declarations<'js>) -> rquickjs::Result<()> {
        for name in [
            "webcrypto",
            "subtle",
            "getRandomValues",
            "randomUUID",
            "default",
        ] {
            decl.declare(name)?;
        }
        Ok(())
    }

    fn evaluate<'js>(ctx: &Ctx<'js>, exports: &Exports<'js>) -> rquickjs::Result<()> {
        let crypto: Object = ctx.globals().get("crypto")?;
        let default = Object::new(ctx.clone())?;
        default.set("webcrypto", crypto.clone())?;
        exports.export("webcrypto", crypto.clone())?;
        for name in ["subtle", "getRandomValues", "randomUUID"] {
            let value: Value = crypto.get(name)?;
            default.set(name, value.clone())?;
            exports.export(name, value)?;
        }
        exports.export("default", default)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::DeflateEncoder;

    fn deflate(data: &[u8]) -> Vec<u8> {
        let mut e = DeflateEncoder::new(Vec::new(), Compression::best());
        e.write_all(data).unwrap();
        e.finish().unwrap()
    }

    #[test]
    fn inflate_round_trip_and_limits() {
        let data: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        let packed = deflate(&data);
        assert_eq!(inflate_raw(&packed, MAX_INFLATE).unwrap(), data);
        let mut trailing = packed.clone();
        trailing.extend_from_slice(b"garbage");
        assert_eq!(inflate_raw(&trailing, MAX_INFLATE).unwrap(), data);
        assert!(matches!(
            inflate_raw(&packed, 1000),
            Err(InflateError::TooLarge)
        ));
        assert!(matches!(
            inflate_raw(&packed[..packed.len() / 2], MAX_INFLATE),
            Err(InflateError::Truncated)
        ));
        assert!(matches!(
            inflate_raw(&[0xff, 0xff, 0xff], MAX_INFLATE),
            Err(InflateError::Data(_))
        ));
        assert_eq!(inflate_raw(&deflate(b""), MAX_INFLATE).unwrap(), b"");
        assert_eq!(inflate_raw(&packed, data.len()).unwrap(), data);
    }

    #[test]
    fn canonical_names() {
        assert_eq!(canonical("zlib"), "node:zlib");
        assert_eq!(canonical("node:crypto"), "node:crypto");
        assert_eq!(canonical("file:///a/b%20c.mjs"), "file:///a/b%20c.mjs");
        assert_eq!(canonical("file:///a/b c.mjs"), "file:///a/b%20c.mjs");
    }
}
